use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use lightning::ln::PaymentHash;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;

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
pub enum PaymentError {
    #[error("Get info failed {0}")]
    GetInfoFailed(String),
    #[error("Other: {0:?}")]
    Error(#[from] anyhow::Error),
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
    async fn get_info(&self) -> Result<NodeInfo, PaymentError>;

    /// Keysend payment worth `amount_msat` from a source node to the destination node.
    async fn send_payment(
        &self,
        dest: PublicKey,
        amount_msat: u64,
    ) -> Result<PaymentHash, PaymentError>;

    /// Track a payment with the specified hash.
    async fn track_payment(&self, hash: PaymentHash) -> Result<(), PaymentError>;
}

#[allow(dead_code)]
enum NodeAction {
    // Dispatch a payment of the specified amount to the public key provided.
    SendPayment(PublicKey, u64),
}

#[allow(dead_code)]
struct Event {
    // The public key of the node executing this event.
    source: PublicKey,

    // Offset is the time offset from the beginning of execution that this event should be executed.
    offset: u64,

    // An action to be executed on the source node.
    action: NodeAction,
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
    nodes: HashMap<PublicKey, Arc<dyn LightningNode>>,

    // The activity that are to be executed on the node.
    activity: Vec<ActivityDefinition>,
}

impl Simulation {
    pub fn new(
        nodes: HashMap<PublicKey, Arc<dyn LightningNode>>,
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
