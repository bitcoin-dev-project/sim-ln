//! The environment layer: provisions a lightning network and describes it as a partial simulation
//! config. Implementations know nothing about the payment activity that will run on the network.

pub mod containers;
pub mod simulated;

use bitcoin::secp256k1::PublicKey;

/// The node implementation backing a [`NodeHandle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeImpl {
    Simulated,
    Lnd,
    Cln,
    Eclair,
    LdkServer,
}

/// A node in a test network, exposing just enough for scenarios to reference it in config and for
/// assertions to identify it in results.
#[derive(Debug, Clone)]
pub struct NodeHandle {
    pub pubkey: PublicKey,
    pub alias: String,
    pub implementation: NodeImpl,
}

/// The environment's contribution to a sim.json file. Built as raw JSON rather than the parsing
/// crate's own types so that tests exercise real deserialization of the file format.
#[derive(Debug, Clone)]
pub enum ConfigFragment {
    /// Entries for the `nodes` key: connection details for real nodes.
    RealNodes(Vec<serde_json::Value>),
    /// Entries for the `sim_network` key: channels of a simulated network.
    SimGraph(Vec<serde_json::Value>),
}

/// A provisioned network that simulations can run against.
pub trait TestNetwork {
    /// The nodes that the simulation will control.
    fn nodes(&self) -> &[NodeHandle];

    /// The network's contribution to the simulation config.
    fn config_fragment(&self) -> ConfigFragment;
}
