//! An in-process simulated network: a deterministic graph emitted as a `sim_network` config
//! section, requiring no external processes.

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde_json::{json, Value};

use super::{ConfigFragment, NodeHandle, NodeImpl, TestNetwork};

/// Capacity of every channel in the simulated network. Large relative to test payment amounts so
/// that liquidity never limits a test scenario.
pub const CHANNEL_CAPACITY_MSAT: u64 = 100_000_000;

/// A deterministic five-node simulated network: a ring of nodes 0-1-2-3 plus a spur node 4
/// attached to node 0, so every pair of nodes is connected and some pairs (e.g. 4 -> 2) can only
/// be reached over multiple hops.
///
/// Fees are set to zero on all channels so that payment amounts observed in results exactly match
/// the amounts scenarios dispatch, and routing fee budgets can never interfere with tests that are
/// not about fees.
pub struct SimulatedNetwork {
    handles: Vec<NodeHandle>,
    channels: Vec<Value>,
}

impl SimulatedNetwork {
    /// The channel endpoints of the network, as indices into the node list.
    const EDGES: [(usize, usize); 5] = [(0, 1), (1, 2), (2, 3), (3, 0), (0, 4)];

    pub fn new() -> Self {
        Self::with_aliases(&["node_0", "node_1", "node_2", "node_3", "node_4"])
    }

    /// A network where two nodes share an alias, for tests asserting that duplicate aliases are
    /// rejected.
    pub fn with_duplicate_alias() -> Self {
        Self::with_aliases(&["node_0", "node_1", "node_2", "node_3", "node_3"])
    }

    fn with_aliases(aliases: &[&str]) -> Self {
        let secp = Secp256k1::new();
        let handles: Vec<NodeHandle> = aliases
            .iter()
            .enumerate()
            .map(|(i, alias)| {
                // Deterministic keys so that runs are reproducible and pubkey-referenced configs
                // can be asserted against.
                let secret = SecretKey::from_slice(&[i as u8 + 1; 32]).expect("static key valid");
                NodeHandle {
                    pubkey: PublicKey::from_secret_key(&secp, &secret),
                    alias: alias.to_string(),
                    implementation: NodeImpl::Simulated,
                }
            })
            .collect();

        let channels = Self::EDGES
            .iter()
            .enumerate()
            .map(|(i, (a, b))| {
                json!({
                    "scid": i as u64 + 1,
                    "capacity_msat": CHANNEL_CAPACITY_MSAT,
                    "node_1": Self::policy(&handles[*a]),
                    "node_2": Self::policy(&handles[*b]),
                })
            })
            .collect();

        SimulatedNetwork { handles, channels }
    }

    fn policy(node: &NodeHandle) -> Value {
        json!({
            "pubkey": node.pubkey.to_string(),
            "alias": node.alias,
            "max_htlc_count": 483,
            "max_in_flight_msat": CHANNEL_CAPACITY_MSAT,
            "min_htlc_size_msat": 1,
            "max_htlc_size_msat": CHANNEL_CAPACITY_MSAT,
            "cltv_expiry_delta": 40,
            "base_fee": 0,
            "fee_rate_prop": 0,
        })
    }
}

impl Default for SimulatedNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl TestNetwork for SimulatedNetwork {
    fn nodes(&self) -> &[NodeHandle] {
        &self.handles
    }

    fn config_fragment(&self) -> ConfigFragment {
        ConfigFragment::SimGraph(self.channels.clone())
    }
}
