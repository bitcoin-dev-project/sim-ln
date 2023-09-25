use core::fmt;
use std::collections::HashMap;
use std::fmt::Display;

use bitcoin::secp256k1::PublicKey;
use rand_distr::{Distribution, WeightedIndex};

use crate::{NetworkGenerator, SimulationError};

/// NetworkGraphView maintains a view of the network graph that can be used to pick nodes by their deployed liquidity
/// and track node capacity within the network. Tracking nodes in the network is memory-expensive, so we use a single
/// tracker for the whole network (in an unbounded environment, we'd make one _per_ node generating random activity,
/// which has a view of the full network except for itself).
pub struct NetworkGraphView {
    node_picker: WeightedIndex<u64>,
    nodes: Vec<(PublicKey, u64)>,
}

impl NetworkGraphView {
    // Creates a network view for the map of node public keys to capacity (in millisatoshis) provided. Returns an error
    // if any node's capacity is zero (the node cannot receive), or there are not at least two nodes (one node can't
    // send to itself).
    pub fn new(node_capacities: HashMap<PublicKey, u64>) -> Result<Self, SimulationError> {
        if node_capacities.len() < 2 {
            return Err(SimulationError::RandomActivityError(
                "at least two nodes required for activity generation".to_string(),
            ));
        }

        if node_capacities.values().any(|v| *v == 0) {
            return Err(SimulationError::RandomActivityError(
                "network generator created with zero capacity node".to_string(),
            ));
        }

        // To create a weighted index we're going to need a vector of nodes that we index and weights that are set
        // by their deployed capacity. To efficiently store our view of nodes capacity, we're also going to store
        // capacity along with the node pubkey because we query the two at the same time. Zero capacity nodes are
        // filtered out because they have no chance of being selected (and wont' be able to receive payments).
        let nodes = node_capacities.iter().map(|(k, v)| (*k, *v)).collect();

        let node_picker = WeightedIndex::new(node_capacities.into_values().collect::<Vec<u64>>())
            .map_err(|e| SimulationError::RandomActivityError(e.to_string()))?;

        Ok(NetworkGraphView { node_picker, nodes })
    }
}

impl NetworkGenerator for NetworkGraphView {
    /// Randomly samples the network for a node, weighted by capacity.  Using a single graph view means that it's
    /// possible for a source node to select itself. After sufficient retries, this is highly improbable (even  with
    /// very small graphs, or those with one node significantly more capitalized than others).
    fn sample_node_by_capacity(&self, source: PublicKey) -> (PublicKey, u64) {
        let mut rng = rand::thread_rng();

        // While it's very unlikely that we can't pick a destination that is not our source, it's possible that there's
        // a bug in our selection, so we track attempts to select a non-source node so that we can warn if this takes
        // improbably long.
        let mut i = 1;
        loop {
            let index = self.node_picker.sample(&mut rng);
            let destination = self.nodes[index];

            if destination.0 != source {
                return destination;
            }

            if i % 50 == 0 {
                log::warn!("Unable to select a destination for: {source} after {i} attempts. Please report a bug!")
            }
            i += 1
        }
    }
}

impl Display for NetworkGraphView {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "network graph view with: {} channels", self.nodes.len())
    }
}
