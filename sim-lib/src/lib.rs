use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use lightning::events::PaymentFailureReason;
use lightning::ln::PaymentHash;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::marker::Send;
use std::{collections::HashMap, sync::Arc, time::SystemTime};
use thiserror::Error;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time;
use triggered::{Listener, Trigger};
pub mod lnd;

const KEYSEND_OPTIONAL: u32 = 55;

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
    #[error("Track payment error {0}")]
    TrackPaymentError(String),
    #[error("Invalid payment hash")]
    InvalidPaymentHash,
    #[error("RPC error: {0:?}")]
    RpcError(#[from] tonic_lnd::tonic::Status),
    #[error("Get node info error {0}")]
    GetNodeInfoError(String),
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
        &mut self,
        dest: PublicKey,
        amount_msat: u64,
    ) -> Result<PaymentHash, LightningError>;
    /// Track a payment with the specified hash.
    async fn track_payment(
        &mut self,
        hash: PaymentHash,
        shutdown: Listener,
    ) -> Result<PaymentResult, LightningError>;
    /// Looks up a node's announcement in the graph. This function currently only returns features, as they're all we
    /// need, but may be updated to include any other node announcement fields if required.
    async fn get_node_announcement(&self, node: PublicKey) -> Result<HashSet<u32>, LightningError>;
}

#[derive(Clone, Copy)]
enum NodeAction {
    // Dispatch a payment of the specified amount to the public key provided.
    SendPayment(PublicKey, u64),
}

#[derive(Debug, Clone)]
pub struct PaymentResult {
    pub settled: bool,
    pub htlc_count: usize,
    pub failure_reason: Option<PaymentFailureReason>,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct DispatchedPayment {
    source: PublicKey,
    destination: PublicKey,
    hash: PaymentHash,
    amount_msat: u64,
    dispatch_time: SystemTime,
}

#[derive(Debug, Clone, Copy)]
enum ActionOutcome {
    // The payment hash that results from a SendPayment NodeAction being triggered.
    PaymentSent(DispatchedPayment),
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

        // High level triggers used to manage our tasks.
        let (shutdown_trigger, shutdown_listener) = triggered::trigger();

        // Before we start the simulation up, start tasks that will be responsible for gathering simulation data.
        // The action channels are shared across our functionality:
        // - Action Sender: used by the simulation to inform data reporting that it needs to start tracking the
        //   final outcome of the action that it has taken.
        // - Action Receiver: used by data reporting to receive events that have been simulated that need to be
        //   tracked and recorded.
        let (action_sender, action_receiver) = channel(1);
        let mut record_data_set = self.run_data_collection(
            action_receiver,
            shutdown_trigger.clone(),
            shutdown_listener.clone(),
        );

        // Next, we'll spin up our actual activity generator that will be responsible for triggering the activity that
        // has been configured, passing in the channel that is used to notify data collection that actions  have been
        // generated.
        let mut generate_activity_set = self
            .generate_activity(action_sender, shutdown_trigger, shutdown_listener)
            .await?;

        // We always want to wait ofr all threads to exit, so we wait for all of them to exit and track any errors
        // that surface. It's okay if there are multiple and one is overwritten, we just want to know whether we
        // exited with an error or not.
        // TODO: more succinct handling of tasks here.
        let mut success = true;
        while let Some(res) = record_data_set.join_next().await {
            if let Err(e) = res {
                log::error!("Task exited with error: {e}");
                success = false;
            }
        }
        while let Some(res) = generate_activity_set.join_next().await {
            if let Err(e) = res {
                log::error!("Task exited with error: {e}");
                success = false;
            }
        }
        success.then_some(()).ok_or(SimulationError::TaskError)
    }

    // run_data_collection starts the tasks required for the simulation to report of the outcomes of the activity that
    // it generates. The simulation should report actions via the receiver that is passed in.
    fn run_data_collection(
        &self,
        action_receiver: Receiver<ActionOutcome>,
        shutdown: Trigger,
        listener: Listener,
    ) -> tokio::task::JoinSet<()> {
        log::debug!("Simulator data recording starting.");
        let mut set = JoinSet::new();

        // Create a sender/receiver pair that will be used to report final results of action outcomes.
        let (results_sender, results_receiver) = channel(1);

        set.spawn(produce_simulation_results(
            self.nodes.clone(),
            action_receiver,
            results_sender,
            listener,
        ));

        set.spawn(consume_simulation_results(results_receiver, shutdown));

        log::debug!("Simulator data recording exiting.");
        set
    }

    async fn generate_activity(
        &self,
        executed_actions: Sender<ActionOutcome>,
        shutdown: Trigger,
        listener: Listener,
    ) -> Result<JoinSet<()>, SimulationError> {
        let mut set = JoinSet::new();
        // Before we start the simulation, we'll spin up the infrastructure that we need to record data:
        // We only need to spin up producers for nodes that are contained in our activity description, as there will be
        // no events for nodes that are not source nodes.
        let mut producer_channels = HashMap::new();

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

            // Generate a consumer for the receiving end of the channel. It takes the event receiver that it'll pull
            // events from and the results sender to report the events it has triggered for further monitoring.
            set.spawn(consume_events(
                node.clone(),
                receiver,
                executed_actions.clone(),
                shutdown.clone(),
            ));

            // Add the producer channel to our map so that various activity descriptions can use it. We may have multiple
            // activity descriptions that have the same source node.
            producer_channels.insert(id, sender);
        }

        for description in self.activity.iter() {
            let sender_chan = producer_channels.get(&description.source).unwrap();
            set.spawn(produce_events(
                *description,
                sender_chan.clone(),
                shutdown.clone(),
                listener.clone(),
            ));
        }

        Ok(set)
    }
}

// consume_events processes events that are crated for a lightning node that we can execute actions on. Any output
// that is generated from the action being taken is piped into a channel to handle the result of the action. If it
// exits, it will use the trigger provided to trigger shutdown in other threads. If an error occurs elsewhere, we
// expect the senders corresponding to our receiver to be dropped, which will cause the receiver to error out and
// exit.
async fn consume_events(
    node: Arc<Mutex<dyn LightningNode + Send>>,
    mut receiver: Receiver<NodeAction>,
    sender: Sender<ActionOutcome>,
    shutdown: triggered::Trigger,
) {
    let node_id = node.lock().await.get_info().pubkey;
    log::debug!("Started consumer for {}", node_id);

    while let Some(action) = receiver.recv().await {
        match action {
            NodeAction::SendPayment(dest, amt_msat) => {
                let mut node = node.lock().await;
                let payment = node.send_payment(dest, amt_msat);

                match payment.await {
                    Ok(payment_hash) => {
                        log::info!(
                            "Send payment: {} -> {}: ({})",
                            node_id,
                            dest,
                            hex::encode(payment_hash.0)
                        );

                        log::info!("Sending action for {:?}", payment_hash);
                        let outcome = ActionOutcome::PaymentSent(DispatchedPayment {
                            source: node.get_info().pubkey,
                            hash: payment_hash,
                            amount_msat: amt_msat,
                            destination: dest,
                            dispatch_time: SystemTime::now(),
                        });

                        match sender.send(outcome).await {
                            Ok(_) => {}
                            Err(e) => {
                                log::error!("Error sending action outcome: {:?}", e);
                                break;
                            }
                        }
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
async fn produce_events(
    act: ActivityDefinition,
    sender: Sender<NodeAction>,
    shutdown: Trigger,
    listener: Listener,
) {
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
        if time::timeout(interval, listener.clone()).await.is_ok() {
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

    // On exit call our shutdown trigger to inform other threads that we have exited, and they need to shut down.
    shutdown.trigger();
}

async fn consume_simulation_results(
    mut receiver: Receiver<(DispatchedPayment, PaymentResult)>,
    shutdown: triggered::Trigger,
) {
    log::debug!("Simulation results consumer started.");

    while let Some(resolved_payment) = receiver.recv().await {
        // TODO - write to CSV.
        println!("Resolved payment received: {:?}", resolved_payment);
    }

    log::debug!("Simulation results consumer exiting");
    shutdown.trigger();
}

/// produce_results is responsible for receiving the outcomes of actions that the simulator has taken and
/// spinning up a producer that will report the results to our main result consumer. We handle each outcome
/// separately because they can take a long time to resolve (eg, a payemnt that ends up on chain will take a long
/// time to resolve).
async fn produce_simulation_results(
    nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>>,
    mut outcomes: Receiver<ActionOutcome>,
    results: Sender<(DispatchedPayment, PaymentResult)>,
    shutdown: Listener,
) {
    log::debug!("Simulation results producer started.");

    let mut set = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            outcome = outcomes.recv() => {
                match outcome{
                    Some(action_outcome) => {
                        match action_outcome{
                            ActionOutcome::PaymentSent(dispatched_payment) => {
                                let source_node = nodes.get(&dispatched_payment.source).unwrap().clone();

                                set.spawn(track_outcome(
                                    source_node,results.clone(),action_outcome, shutdown.clone(),
                                ));
                            },
                        };

                    },
                    None => {
                        return
                    }
                }
            }
            _ = shutdown.clone() => {
                break;
            },
        }
    }

    log::debug!("Simulation results producer exiting.");
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            log::error!("Simulation results producer task exited with error: {e}");
        }
    }
}

async fn track_outcome(
    node: Arc<Mutex<dyn LightningNode + Send>>,
    results: Sender<(DispatchedPayment, PaymentResult)>,
    outcome: ActionOutcome,
    shutdown: Listener,
) {
    log::trace!("Outcome tracker starting.");

    let mut node = node.lock().await;

    match outcome {
        ActionOutcome::PaymentSent(payment) => {
            let track_payment = node.track_payment(payment.hash, shutdown.clone());

            match track_payment.await {
                Ok(res) => {
                    if results.clone().send((payment, res)).await.is_err() {
                        log::debug!("Could not send payment result for {:?}.", payment.hash);
                    }
                }
                Err(e) => log::error!("Track payment failed for {:?}: {e}", payment.hash),
            }
        }
    }

    log::trace!("Outcome tracker exiting.");
}
