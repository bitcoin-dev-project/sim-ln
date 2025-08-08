#![cfg(test)]
use async_trait::async_trait;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bitcoin::Network;
use lightning::ln::features::Features;
use mockall::mock;
use rand::distributions::Uniform;
use rand::Rng;
use std::collections::HashMap;
use std::{fmt, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;

use crate::clock::SystemClock;
use crate::{
    ActivityDefinition, Graph, LightningError, LightningNode, NodeInfo, PaymentGenerationError,
    PaymentGenerator, Simulation, SimulationCfg, ValueOrRange,
};

/// Utility function to create a vector of pseudo random bytes.
///
/// Mainly used for testing purposes.
pub fn get_random_bytes(size: usize) -> Vec<u8> {
    rand::thread_rng()
        .sample_iter(Uniform::new(u8::MIN, u8::MAX))
        .take(size)
        .collect()
}

/// Utility function to create a random integer in a given range
pub fn get_random_int(s: u64, e: u64) -> u64 {
    rand::thread_rng().gen_range(s..e)
}

/// Gets a key pair generated in a pseudorandom way.
pub fn get_random_keypair() -> (SecretKey, PublicKey) {
    loop {
        if let Ok(sk) = SecretKey::from_slice(&get_random_bytes(32)) {
            return (sk, PublicKey::from_secret_key(&Secp256k1::new(), &sk));
        }
    }
}

/// Creates n nodes with the capacity specified.
pub fn create_nodes(n: usize, node_capacity: u64) -> Vec<(NodeInfo, u64)> {
    (1..=n)
        .map(|_| {
            (
                NodeInfo {
                    pubkey: get_random_keypair().1,
                    alias: String::new(),
                    features: Features::empty(),
                },
                node_capacity,
            )
        })
        .collect()
}

mock! {
    pub Generator {}

    impl fmt::Display for Generator {
        fn fmt<'a>(&self, f: &mut fmt::Formatter<'a>) -> fmt::Result;
    }

    impl PaymentGenerator for Generator {
        fn payment_start(&self) -> Option<Duration>;
        fn payment_count(&self) -> Option<u64>;
        fn next_payment_wait(&self) -> Result<Duration, PaymentGenerationError>;
        fn payment_amount(&self, destination_capacity: Option<u64>) -> Result<u64, PaymentGenerationError>;
    }
}

mock! {
    pub LightningNode {}
    #[async_trait]
    impl crate::LightningNode for LightningNode {
        fn get_info(&self) -> &NodeInfo;
        fn get_network(&self) -> bitcoin::Network;
        async fn send_payment(
                &self,
                dest: bitcoin::secp256k1::PublicKey,
                amount_msat: u64,
            ) -> Result<lightning::ln::PaymentHash, LightningError>;
        async fn track_payment(
                &self,
                hash: &lightning::ln::PaymentHash,
                shutdown: triggered::Listener,
            ) -> Result<crate::PaymentResult, LightningError>;
        async fn get_node_info(&self, node_id: &PublicKey) -> Result<NodeInfo, LightningError>;
        async fn list_channels(&self) -> Result<Vec<u64>, LightningError>;
        async fn get_graph(&self) -> Result<Graph, LightningError>;
    }
}

/// Type alias for the result of setup_test_nodes.
pub struct TestNodesResult {
    pub nodes: Vec<NodeInfo>,
    pub clients: Vec<Arc<Mutex<MockLightningNode>>>,
}

impl TestNodesResult {
    // Returns a hashmap of the mocked lightning clients, cast to dyn LightningNode.
    pub fn get_client_hashmap(&self) -> HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> {
        let mut client_map: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> =
            HashMap::with_capacity(self.nodes.len());

        for (idx, node) in self.nodes.iter().enumerate() {
            client_map.insert(node.pubkey, self.clients[idx].clone());
        }
        client_map
    }
}

/// A builder for creating mock Lightning nodes for testing purposes.
///
/// This struct provides a flexible way to configure test nodes with features like
/// keysend support and specific networks. It supports both full node setups (with
/// node info and clients) and client-only setups for network testing.
///
/// # Examples
///
/// Using the builder for more control:
/// let (nodes, clients) = LightningTestNodeBuilder::new(5)
///     .with_keysend_nodes(vec![0, 2, 4])
///     .build_full();
///
/// Building clients with specific networks:
/// let clients = LightningTestNodeBuilder::new(3)
///     .with_networks(vec![Network::Bitcoin, Network::Testnet, Network::Regtest])
///     .build_clients_only();
pub struct LightningTestNodeBuilder {
    // The number of nodes in the network.
    node_count: usize,
    // The balance to fund channels with, default: 100_000.
    initial_balance: u64,
    // The indexes of nodes that support keysend in the network.
    keysend_indices: Vec<usize>,
    // The networks that that each node supports, length must equal node_count.
    networks: Option<Vec<Network>>,
    // An optional set of fixed pubkey values for the network, used when tests required
    // deterministic values.
    fixed_pubkeys: Vec<PublicKey>,
}

impl LightningTestNodeBuilder {
    /// Creates a new builder instance with the specified number of nodes.
    /// The default configuration includes a balance of 100,000 units per node,
    /// keysend support ON, and Regtest network.
    pub fn new(node_count: usize) -> Self {
        Self {
            node_count,
            initial_balance: 100_000,
            // Turn keysend on by default.
            keysend_indices: (0..node_count).collect(),
            networks: Some(vec![Network::Regtest; node_count]),
            fixed_pubkeys: vec![],
        }
    }

    /// Specifies which nodes should have the keysend feature enabled.
    pub fn with_keysend_nodes(mut self, indices: Vec<usize>) -> Self {
        self.keysend_indices = indices;
        self
    }

    /// Specifies the public keys for each node in the test network.
    pub fn with_fixed_pubkeys(mut self, pubkeys: Vec<PublicKey>) -> Self {
        assert_eq!(
            pubkeys.len(),
            self.node_count,
            "Must specify a fixed pubkey for each node",
        );
        self.fixed_pubkeys = pubkeys;
        self
    }

    /// Sets specific networks for each node, asserting that the correct number of networks for
    /// was provided.
    pub fn with_networks(mut self, networks: Vec<Network>) -> Self {
        // Validate that we have the correct number of networks
        assert_eq!(
            networks.len(),
            self.node_count,
            "Must specify a network for each node"
        );
        self.networks = Some(networks);
        self
    }

    /// Builds the full test setup, including node info and clients. Returns a tuple of node
    /// information and a map of public keys to mocked Lightning node clients.
    pub fn build_full(self) -> TestNodesResult {
        let node_info_list = create_nodes(self.node_count, self.initial_balance);
        let mut nodes = Vec::with_capacity(node_info_list.len());
        let mut clients = Vec::with_capacity(node_info_list.len());

        for (idx, (mut node_info, _)) in node_info_list.into_iter().enumerate() {
            if self.keysend_indices.contains(&idx) {
                node_info.features.set_keysend_optional();
            }

            if !self.fixed_pubkeys.is_empty() {
                node_info.pubkey = self.fixed_pubkeys[idx]
            }

            let mut mock_node = MockLightningNode::new();
            mock_node.expect_get_info().return_const(node_info.clone());

            if let Some(networks) = &self.networks {
                let network = networks[idx];
                mock_node.expect_get_network().return_const(network);
            }

            clients.push(Arc::new(Mutex::new(mock_node)));
            nodes.push(node_info);
        }

        TestNodesResult { nodes, clients }
    }
}

/// Creates a new simulation with the given clients and activity definitions.
/// Note: This sets a runtime for the simulation of 0, so run() will exit immediately.
pub fn create_simulation(
    clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
) -> Simulation<SystemClock> {
    let (shutdown_trigger, shutdown_listener) = triggered::trigger();
    Simulation::new(
        SimulationCfg::new(Some(0), 0, 0.0, None, None),
        clients,
        TaskTracker::new(),
        Arc::new(SystemClock {}),
        shutdown_trigger,
        shutdown_listener,
    )
}
pub fn create_activity(
    source: NodeInfo,
    destination: NodeInfo,
    amount_msat: u64,
) -> ActivityDefinition {
    ActivityDefinition {
        source,
        destination,
        start_secs: None,
        count: None,
        interval_secs: ValueOrRange::Value(5),
        amount_msat: ValueOrRange::Value(amount_msat),
    }
}
