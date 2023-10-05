use core::fmt;
use std::fmt::Display;

use bitcoin::secp256k1::PublicKey;
use rand_distr::{Distribution, Exp, LogNormal, WeightedIndex};
use std::time::Duration;

use crate::{NetworkGenerator, NodeInfo, PaymentGenerator, SimulationError};

const HOURS_PER_MONTH: u64 = 30 * 24;
const SECONDS_PER_MONTH: u64 = HOURS_PER_MONTH * 60 * 60;

/// NetworkGraphView maintains a view of the network graph that can be used to pick nodes by their deployed liquidity
/// and track node capacity within the network. Tracking nodes in the network is memory-expensive, so we use a single
/// tracker for the whole network (in an unbounded environment, we'd make one _per_ node generating random activity,
/// which has a view of the full network except for itself).
pub struct NetworkGraphView {
    node_picker: WeightedIndex<u64>,
    nodes: Vec<(NodeInfo, u64)>,
}

impl NetworkGraphView {
    // Creates a network view for the map of node public keys to capacity (in millisatoshis) provided. Returns an error
    // if any node's capacity is zero (the node cannot receive), or there are not at least two nodes (one node can't
    // send to itself).
    pub fn new(nodes: Vec<(NodeInfo, u64)>) -> Result<Self, SimulationError> {
        if nodes.len() < 2 {
            return Err(SimulationError::RandomActivityError(
                "at least two nodes required for activity generation".to_string(),
            ));
        }

        if nodes.iter().any(|(_, v)| *v == 0) {
            return Err(SimulationError::RandomActivityError(
                "network generator created with zero capacity node".to_string(),
            ));
        }

        // To create a weighted index we're going to need a vector of nodes that we index and weights that are set
        // by their deployed capacity. To efficiently store our view of nodes capacity, we're also going to store
        // capacity along with the node info because we query the two at the same time. Zero capacity nodes are
        // filtered out because they have no chance of being selected (and wont' be able to receive payments).
        let node_picker = WeightedIndex::new(nodes.iter().map(|(_, v)| *v).collect::<Vec<u64>>())
            .map_err(|e| SimulationError::RandomActivityError(e.to_string()))?;

        Ok(NetworkGraphView { node_picker, nodes })
    }
}

impl NetworkGenerator for NetworkGraphView {
    /// Randomly samples the network for a node, weighted by capacity.  Using a single graph view means that it's
    /// possible for a source node to select itself. After sufficient retries, this is highly improbable (even  with
    /// very small graphs, or those with one node significantly more capitalized than others).
    fn sample_node_by_capacity(&self, source: PublicKey) -> (NodeInfo, u64) {
        let mut rng = rand::thread_rng();

        // While it's very unlikely that we can't pick a destination that is not our source, it's possible that there's
        // a bug in our selection, so we track attempts to select a non-source node so that we can warn if this takes
        // improbably long.
        let mut i = 1;
        loop {
            let index = self.node_picker.sample(&mut rng);
            // Unwrapping is safe given `NetworkGraphView` has the same amount of elements for `nodes` and `node_picker`
            let destination = self.nodes.get(index).unwrap();

            if destination.0.pubkey != source {
                return destination.clone();
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

/// PaymentActivityGenerator manages generation of random payments for an individual node. For some multiplier of the
/// node's capacity, it will produce payments such that the node sends multiplier * capacity over a calendar month.
/// While the expected amount to be sent in a month and the mean payment amount are set, the generator will introduce
/// randomness both in the time between events and the variance of payment amounts sent to mimic more realistic
/// payment flows.
pub struct PaymentActivityGenerator {
    multiplier: f64,
    expected_payment_amt: u64,
    source_capacity: u64,
    event_dist: Exp<f64>,
}

impl PaymentActivityGenerator {
    /// Creates a new activity generator for a node, returning an error if the node has insufficient capacity deployed
    /// for the expected payment amount provided. Capacity is defined as the sum of the channels that the node has
    /// open in the network divided by two (so that capacity is not double counted with channel counterparties).
    pub fn new(
        source_capacity_msat: u64,
        expected_payment_amt: u64,
        multiplier: f64,
    ) -> Result<Self, SimulationError> {
        PaymentActivityGenerator::validate_capacity(source_capacity_msat, expected_payment_amt)?;

        // Lamda for the exponential distribution that we'll use to randomly time events is equal to the number of
        // events that we expect to see within our set period.
        let lamda = events_per_month(source_capacity_msat, multiplier, expected_payment_amt)
            / (SECONDS_PER_MONTH as f64);

        let event_dist =
            Exp::new(lamda).map_err(|e| SimulationError::RandomActivityError(e.to_string()))?;

        Ok(PaymentActivityGenerator {
            multiplier,
            expected_payment_amt,
            source_capacity: source_capacity_msat,
            event_dist,
        })
    }

    /// Validates that the generator will be able to generate payment amounts based on the node's capacity and the
    /// simulation's expected payment amount.
    pub fn validate_capacity(
        node_capacity_msat: u64,
        expected_payment_amt: u64,
    ) -> Result<(), SimulationError> {
        // We will not be able to generate payments if the variance of sigma squared for our log normal distribution
        // is < 0 (because we have to take a square root).
        //
        // Sigma squared is calculated as: 2(ln(payment_limit) - ln(expected_payment_amt))
        // Where: payment_limit = node_capacity_msat / 2.
        //
        // Therefore we can only process payments if: 2(ln(payment_limit) - ln(expected_payment_amt)) >= 0
        //   ln(payment_limit)      >= ln(expected_payment_amt)
        //   e^(ln(payment_limit)   >= e^(ln(expected_payment_amt))
        //   payment_limit          >= expected_payment_amt
        //   node_capacity_msat / 2 >= expected_payment_amt
        //   node_capacity_msat     >= 2 * expected_payment_amt
        let min_required_capacity = 2 * expected_payment_amt;
        if node_capacity_msat < min_required_capacity {
            return Err(SimulationError::RandomActivityError(format!(
                "node needs at least {} capacity (has: {}) to process expected payment amount: {}",
                min_required_capacity, node_capacity_msat, expected_payment_amt
            )));
        }

        Ok(())
    }
}

/// Returns the number of events that the simulation expects the node to process per month based on its capacity, a
/// multiplier which expresses the capital efficiently of the network (how "much" it uses its deployed liquidity) and
/// the expected payment amount for the simulation.
///
/// The total amount that we expect this node to send is capacity * multiplier, because the multiplier is the
/// expression of how many times a node sends its capacity within a month. For example:
/// - A multiplier of 0.5 indicates that the node processes half of its total channel capacity in sends in a month.
/// - A multiplier of 2 indicates that hte node processes twice of its total capacity in sends in a month.
///
/// The number of sends that the simulation will dispatch for this node is simply the total amount that the node is
/// expected to send divided by the expected payment amount (how much we'll send on average) for the simulation.
fn events_per_month(source_capacity_msat: u64, multiplier: f64, expected_payment_amt: u64) -> f64 {
    (source_capacity_msat as f64 * multiplier) / expected_payment_amt as f64
}

impl PaymentGenerator for PaymentActivityGenerator {
    /// Returns the amount of time until the next payment should be scheduled for the node.
    fn next_payment_wait(&self) -> Duration {
        let mut rng = rand::thread_rng();
        Duration::from_secs(self.event_dist.sample(&mut rng) as u64)
    }

    /// Returns the payment amount for a payment to a node with the destination capacity provided. The expected value
    /// for the payment is the simulation expected payment amount, and the variance is determined by the channel
    /// capacity of the source and destination node. Variance is calculated such that 95% of payment amounts generated
    /// will fall between the expected payment amount and 50% of the capacity of the node with the least channel
    /// capacity. While the expected value of payments remains the same, scaling variance by node capacity means that
    /// nodes with more deployed capital will see a larger range of payment values than those with smaller total
    /// channel capacity.
    fn payment_amount(&self, destination_capacity: u64) -> Result<u64, SimulationError> {
        let payment_limit = std::cmp::min(self.source_capacity, destination_capacity) / 2;

        let ln_pmt_amt = (self.expected_payment_amt as f64).ln();
        let ln_limit = (payment_limit as f64).ln();

        let mu = 2.0 * ln_pmt_amt - ln_limit;
        let sigma_square = 2.0 * (ln_limit - ln_pmt_amt);

        if sigma_square < 0.0 {
            return Err(SimulationError::RandomActivityError(format!(
                "payment amount not possible for limit: {payment_limit}, sigma squared: {sigma_square}"
            )));
        }

        let log_normal = LogNormal::new(mu, sigma_square.sqrt())
            .map_err(|e| SimulationError::RandomActivityError(e.to_string()))?;

        let mut rng = rand::thread_rng();
        Ok(log_normal.sample(&mut rng) as u64)
    }
}

impl Display for PaymentActivityGenerator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let monthly_events = events_per_month(
            self.source_capacity,
            self.multiplier,
            self.expected_payment_amt,
        );

        write!(
            f,
            "activity generator for capacity: {} with multiplier {}: {} payments per month ({} per hour)",
            self.source_capacity,
            self.multiplier,
            monthly_events,
            monthly_events / HOURS_PER_MONTH as f64
        )
    }
}
