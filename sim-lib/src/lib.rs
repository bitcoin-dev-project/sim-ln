use bitcoin::secp256k1::PublicKey;
use lightning::ln::PaymentHash;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

// Phase 0: User input - see config.json

// Phase 1: Parsed User Input

#[derive(Serialize, Deserialize, Debug)]
pub struct NodeConnection {
    id: String,
    host: String,
    macaroon: String,
    cert: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    nodes: Vec<NodeConnection>,
}

pub struct ActivityDefinition {
    // The source of the action.
    source: PublicKey,
    // The destination of the action.
    dest: PublicKey,
    // The frequency of the action, as in number of times per minute.
    frequency: u16,
    // The amount of m_sat to used in this action.
    amt_msat: u64,
}

// Phase 2: Event Queue

#[allow(dead_code)]
enum PaymentError {}

/// LightningNode represents the functionality that is required to execute events on a lightning node.
trait LightningNode {
    fn send_payment(&self, dest: PublicKey, amt_msat: u64) -> Result<PaymentHash, ()>;
    fn track_payment(&self, hash: PaymentHash) -> Result<(), PaymentError>;
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
