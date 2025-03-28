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

// Mock implementations for tests
#[cfg(test)]
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

#[cfg(test)]
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

/// Creates a set of nodes with mock implementations
#[cfg(test)]
pub fn setup_test_nodes(node_count: usize, keysend_indices: &[usize]) -> TestNodesResult {
    let nodes = create_nodes(node_count, 100_000);
    let mut node_infos = Vec::new();
    let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> = HashMap::new();

    for (idx, (node_info, _)) in nodes.into_iter().enumerate() {
        let mut node = node_info.clone();

        // Enable keysend on specified nodes
        if keysend_indices.contains(&idx) {
            node.features.set_keysend_optional();
        }

        // Create and configure mock
        let mut mock_node = MockLightningNode::new();
        mock_node.expect_get_info().return_const(node.clone());

        // Store in map
        clients.insert(node.pubkey, Arc::new(Mutex::new(mock_node)));
        node_infos.push(node);
    }

    (node_infos, clients)
}

/// Creates a simulation with the given nodes and activity
#[cfg(test)]
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

/// Creates an activity definition
#[cfg(test)]
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

/// Setup nodes with different networks for testing network validation
#[cfg(test)]
pub fn setup_network_test_nodes(
    node_count: usize,
    networks: Vec<Network>,
) -> HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> {
    assert_eq!(
        node_count,
        networks.len(),
        "Must specify a network for each node"
    );

    let nodes = create_nodes(node_count, 100_000);
    let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> = HashMap::new();

    for (idx, (node_info, _)) in nodes.into_iter().enumerate() {
        let mut mock_node = MockLightningNode::new();

        // Configure get_info to return the node info
        mock_node.expect_get_info().return_const(node_info.clone());

        // Configure get_network to return the specified network
        let network = networks[idx];
        mock_node
            .expect_get_network()
            .returning(move || Ok(network));

        // Store in map
        clients.insert(node_info.pubkey, Arc::new(Mutex::new(mock_node)));
    }

    clients
}
