use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use lightning::ln::PaymentHash;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::marker::Send;
use std::{collections::HashMap, sync::Arc, time::SystemTime};
use thiserror::Error;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time;
use triggered::{Listener, Trigger};

pub mod lnd;

// Phase 0: User input - see config.json

// Phase 1: Parsed User Input

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NodeConnection {
    pub id: PublicKey,
    pub address: String,
    pub macaroon: String,
    pub cert: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub nodes: Vec<NodeConnection>,
    pub activity: Vec<ActivityDefinition>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ActivityDefinition {
    // The source of the action.
    pub source: PublicKey,
    // The destination of the action.
    pub destination: PublicKey,
    // The frequency of the action, as in number of times per minute.
    pub frequency: u16,
    // The amount of m_sat to used in this action.
    pub amount_msat: u64,
}

#[derive(Debug, Error)]
pub enum SimulationError {
    #[error("Lightning Error: {0:?}")]
    LightningError(#[from] LightningError),
    #[error("TaskError")]
    TaskError,
}

#[derive(Debug, Error)]
pub enum LightningError {
    #[error("Node connection error {0}")]
    ConnectionError(String),
    #[error("Get info error {0}")]
    GetInfoError(String),
    #[error("Send payment error {0}")]
    SendPaymentError(String),
    #[error("Track pyment error {0}")]
    TrackPaymentError(String),
    #[error("Invalid payment hash")]
    InvalidPaymentHash,
    #[error("RPC error: {0:?}")]
    RpcError(#[from] tonic_lnd::tonic::Status),
}

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub pubkey: PublicKey,
    pub alias: String,
    pub features: Vec<u32>,
}

/// LightningNode represents the functionality that is required to execute events on a lightning node.
#[async_trait]
pub trait LightningNode {
    /// Get information about the node.
    fn get_info(&self) -> &NodeInfo;
    /// Keysend payment worth `amount_msat` from a source node to the destination node.
    async fn send_payment(
        &self,
        dest: PublicKey,
        amount_msat: u64,
    ) -> Result<PaymentHash, LightningError>;
    /// Track a payment with the specified hash.
    async fn track_payment(&self, hash: PaymentHash) -> Result<(), LightningError>;
}

#[derive(Clone, Copy)]
enum NodeAction {
    // Dispatch a payment of the specified amount to the public key provided.
    SendPayment(PublicKey, u64),
}

// Phase 3: CSV output

#[allow(dead_code)]
struct PaymentResult {
    source: PublicKey,
    dest: PublicKey,
    start: SystemTime,
    end: SystemTime,
    settled: bool,
    action: u8,
}

pub struct Simulation {
    // The lightning node that is being simulated.
    nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>>,

    // The activity that are to be executed on the node.
    activity: Vec<ActivityDefinition>,
}

impl Simulation {
    pub fn new(
        nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>>,
        activity: Vec<ActivityDefinition>,
    ) -> Self {
        Self { nodes, activity }
    }

    pub async fn run(&self) -> Result<(), SimulationError> {
        log::info!(
            "Simulating {} activity on {} nodes",
            self.activity.len(),
            self.nodes.len()
        );

        let (shutdown_trigger, shutdown_listener) = triggered::trigger();
        self.generate_activity(shutdown_trigger, shutdown_listener)
            .await
    }

    async fn generate_activity(
        &self,
        shutdown: Trigger,
        listener: Listener,
    ) -> Result<(), SimulationError> {
        // We only need to spin up producers for nodes that are contained in our activity description, as there will be
        // no events for nodes that are not source nodes.
        let mut producer_channels = HashMap::new();
        let mut set = tokio::task::JoinSet::new();

        for (id, node) in self.nodes.iter().filter(|(pk, _)| {
            self.activity
                .iter()
                .map(|a| a.source)
                .collect::<HashSet<PublicKey>>()
                .contains(pk)
        }) {
            // For each active node, we'll create a sender and receiver channel to produce and consumer
            // events. We do not buffer channels as we expect events to clear quickly.
            let (sender, receiver) = channel(1);

            // Generate a consumer for the receiving end of the channel.
            set.spawn(consume_events(node.clone(), receiver, shutdown.clone()));

            // Add the producer channel to our map so that various activity descriptions can use it. We may have multiple
            // activity descriptions that have the same source node.
            producer_channels.insert(id, sender);
        }

        for description in self.activity.iter() {
            let sender_chan = producer_channels.get(&description.source).unwrap();
            set.spawn(produce_events(
                *description,
                sender_chan.clone(),
                listener.clone(),
            ));
        }

        // We always want to wait ofr all threads to exit, so we wait for all of them to exit and track any errors
        // that surface. It's okay if there are multiple and one is overwritten, we just want to know whether we
        // exited with an error or not.
        let mut success = true;
        while let Some(res) = set.join_next().await {
            if let Err(e) = res {
                log::error!("Task exited with error: {e}");
                success = false;
            }
        }
        success.then_some(()).ok_or(SimulationError::TaskError)
    }
}

// consume_events processes events that are crated for a lightning node that we can execute actions on. If it exits,
// it will use the trigger provided to trigger shutdown in other threads. If an error occurs elsewhere, we expect the
// senders corresponding to our receiver to be dropped, which will cause the receiver to error out and exit.
async fn consume_events(
    node: Arc<Mutex<dyn LightningNode + Send>>,
    mut receiver: Receiver<NodeAction>,
    shutdown: triggered::Trigger,
) {
    let node_id = node.lock().await.get_info().pubkey;
    log::debug!("Started consumer for {}", node_id);
    while let Some(action) = receiver.recv().await {
        match action {
            NodeAction::SendPayment(dest, amt_msat) => {
                let node = node.lock().await;
                let payment = node.send_payment(dest, amt_msat);

                match payment.await {
                    Ok(payment_hash) => {
                        log::info!(
                            "Send payment: {} -> {}: ({})",
                            node_id,
                            dest,
                            hex::encode(payment_hash.0)
                        )
                    }
                    Err(e) => {
                        log::error!(
                            "Error while sending payment {} -> {}. Terminating consumer. {}",
                            node_id,
                            dest,
                            e
                        );
                        break;
                    }
                };
            }
        };
    }

    // On exit call our shutdown trigger to inform other threads that we have exited, and they need to shut down.
    shutdown.trigger();
}

// produce events generates events for the activity description provided. It accepts a shutdown listener so it can
// exit if other threads signal that they have errored out.
async fn produce_events(act: ActivityDefinition, sender: Sender<NodeAction>, shutdown: Listener) {
    let e: NodeAction = NodeAction::SendPayment(act.destination, act.amount_msat);
    let interval = time::Duration::from_secs(act.frequency as u64);

    log::debug!(
        "Started producer for {} every {}s: {} -> {}",
        act.amount_msat,
        act.frequency,
        act.source,
        act.destination
    );

    loop {
        if time::timeout(interval, shutdown.clone()).await.is_ok() {
            log::debug!(
                "Stopped producer for {}: {} -> {}. Received shutdown signal.",
                act.amount_msat,
                act.source,
                act.destination
            );
            break;
        }

        if sender.send(e).await.is_err() {
            break;
        }
    }
}
