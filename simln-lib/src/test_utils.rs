#[cfg(test)]
use async_trait::async_trait;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bitcoin::Network;
use lightning::ln::features::Features;
use mockall::mock;
use rand::distributions::Uniform;
use rand::Rng;
use std::{collections::HashMap, fmt, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;

use crate::{
    ActivityDefinition, LightningError, LightningNode, NodeInfo, PaymentGenerationError,
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
        async fn get_network(&mut self) -> Result<bitcoin::Network, LightningError>;
        async fn send_payment(
                &mut self,
                dest: bitcoin::secp256k1::PublicKey,
                amount_msat: u64,
            ) -> Result<lightning::ln::PaymentHash, LightningError>;
        async fn track_payment(
                &mut self,
                hash: &lightning::ln::PaymentHash,
                shutdown: triggered::Listener,
            ) -> Result<crate::PaymentResult, LightningError>;
        async fn get_node_info(&mut self, node_id: &PublicKey) -> Result<NodeInfo, LightningError>;
        async fn list_channels(&mut self) -> Result<Vec<u64>, LightningError>;
    }
}

/// Type alias for the result of setup_test_nodes
type TestNodesResult = (
    Vec<NodeInfo>,
    HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
);

/// A builder for creating mock Lightning nodes for testing purposes.
///
/// This struct provides a flexible way to configure test nodes with features like
/// keysend support and specific networks. It supports both full node setups (with
/// node info and clients) and client-only setups for network testing.
///
/// # Examples
/// Basic setup with keysend on specific nodes:
/// let (nodes, clients) = TestNetworkBuilder::setup_test_nodes(5, &[0, 2]);
///
/// Using the builder for more control:
/// let (nodes, clients) = TestNetworkBuilder::new(5)
///     .with_keysend_nodes(vec![0, 2, 4])
///     .build_full();
///
/// Building clients with specific networks:
/// let clients = TestNetworkBuilder::new(3)
///     .with_networks(vec![Network::Bitcoin, Network::Testnet, Network::Regtest])
///     .build_clients_only();
pub struct LightningTestNodeBuilder {
    node_count: usize,              // Required - must be provided at creation
    initial_balance: u64,           // Always has a value (default: 100,000)
    keysend_indices: Vec<usize>,    // Always a vector (default: empty)
    networks: Option<Vec<Network>>, // Can be None (default) or Some(networks)
}

impl LightningTestNodeBuilder {
    /// Creates test nodes with optional keysend support on specified nodes.
    /// A convenience method equivalent to `new(node_count).with_keysend_nodes(indices).build_full()`.
    pub fn setup_test_nodes(node_count: usize, keysend_indices: &[usize]) -> TestNodesResult {
        Self::new(node_count)
            .with_keysend_nodes(keysend_indices.to_vec())
            .build_full()
    }

    /// Creates test nodes with specified networks.
    /// A convenience method equivalent to `new(node_count).with_networks(networks).build_clients_only()`.
    pub fn setup_network_test_nodes(
        node_count: usize,
        networks: Vec<Network>,
    ) -> HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> {
        Self::new(node_count)
            .with_networks(networks)
            .build_clients_only()
    }

    /// Creates a new builder instance with the specified number of nodes.
    /// The default configuration includes a balance of 100,000 units per node,
    /// no keysend support, and no specific networks.
    pub fn new(node_count: usize) -> Self {
        Self {
            node_count,
            initial_balance: 100_000,    // Default 100k sats
            keysend_indices: Vec::new(), // No keysend by default
            networks: None,              // No specific networks by default
        }
    }

    /// Specifies which nodes should have the keysend feature enabled
    pub fn with_keysend_nodes(mut self, indices: Vec<usize>) -> Self {
        self.keysend_indices = indices;
        self
    }

    /// Sets specific networks for each node
    /// Will panic if the number of networks doesn't match node_count
    /// Returns self for method chaining
    pub fn with_networks(mut self, networks: Vec<Network>) -> Self {
        // Validate that we have the correct number of networks
        if networks.len() != self.node_count {
            panic!("Must specify a network for each node");
        }
        self.networks = Some(networks);
        self
    }

    /// Builds only the client map, omitting node info.
    /// Useful for network-specific testing. Returns a map of public keys to mocked
    /// Lightning node clients.
    pub fn build_clients_only(self) -> HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> {
        let (_, clients) = self.build_full();
        clients
    }

    /// Builds the full test setup, including node info and clients.
    /// Returns a tuple of node information and a map of public keys to mocked
    /// Lightning node clients.
    pub fn build_full(self) -> TestNodesResult {
        let nodes = create_nodes(self.node_count, self.initial_balance);
        let mut node_infos = Vec::new();
        let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> = HashMap::new();

        for (idx, (mut node_info, _)) in nodes.into_iter().enumerate() {
            if self.keysend_indices.contains(&idx) {
                node_info.features.set_keysend_optional();
            }

            let mut mock_node = MockLightningNode::new();
            mock_node.expect_get_info().return_const(node_info.clone());

            if let Some(networks) = &self.networks {
                let network = networks[idx];
                mock_node
                    .expect_get_network()
                    .returning(move || Ok(network));
            }

            clients.insert(node_info.pubkey, Arc::new(Mutex::new(mock_node)));
            node_infos.push(node_info);
        }

        (node_infos, clients)
    }
}

pub fn create_simulation(
    clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
    activity: Vec<ActivityDefinition>,
) -> Simulation {
    Simulation::new(
        SimulationCfg::new(Some(0), 0, 0.0, None, None),
        clients,
        activity,
        TaskTracker::new(),
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
