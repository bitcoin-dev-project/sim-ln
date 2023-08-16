use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use lightning::ln::PaymentHash;
use serde::{Deserialize, Serialize};
use std::marker::Send;
use std::{collections::HashMap, sync::Arc, time::SystemTime};
use thiserror::Error;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time;
use triggered::Listener;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[error("Other: {0:?}")]
    Error(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum LightningError {
    #[error("Node connection error {0}")]
    ConnectionError(String),
    #[error("Get info error {0}")]
    GetInfoError(String),
    #[error("Send payment error {0}")]
    SendPaymentError(String),
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
    async fn get_info(&self) -> Result<NodeInfo, LightningError>;

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

    pub async fn run(&self) -> anyhow::Result<()> {
        println!(
            "Simulating {} activity on {} nodes",
            self.activity.len(),
            self.nodes.len()
        );
        println!("42 and Done!");
        Ok(())
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
    while let Some(action) = receiver.recv().await {
        match action {
            NodeAction::SendPayment(dest, amt_msat) => {
                let node = node.lock().await;
                let payment = node.send_payment(dest, amt_msat);

                match payment.await {
                    Ok(payment_hash) => println!("Send payment: {:?}", payment_hash),
                    Err(_) => break,
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
    let e = NodeAction::SendPayment(act.destination, act.amount_msat);
    let interval = time::Duration::from_secs(act.frequency as u64);

    loop {
        if time::timeout(interval, shutdown.clone()).await.is_ok() {
            println!("Received shutting down signal. Shutting down");
            break;
        }

        if sender.send(e).await.is_err() {
            break;
        }
    }
}
