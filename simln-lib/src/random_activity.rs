use core::fmt;
use std::fmt::Display;
use thiserror::Error;

use bitcoin::secp256k1::PublicKey;
use rand_distr::{Distribution, Exp, LogNormal, WeightedIndex};
use std::time::Duration;

use crate::{
    DestinationGenerationError, DestinationGenerator, MutRng, NodeInfo, PaymentGenerationError,
    PaymentGenerator,
};

const HOURS_PER_MONTH: u64 = 30 * 24;
const SECONDS_PER_MONTH: u64 = HOURS_PER_MONTH * 60 * 60;

#[derive(Debug, Error)]
pub enum RandomActivityError {
    #[error("Value error: {0}")]
    ValueError(String),
    #[error("InsufficientCapacity: {0}")]
    InsufficientCapacity(String),
}

/// `NetworkGraphView` maintains a view of the network graph that can be used to pick nodes by their deployed liquidity
/// and track node capacity within the network. The `NetworkGraphView` also keeps a handle on a random number generator
/// that allows it to deterministically pick nodes. Tracking nodes in the network is memory-expensive, so we
/// use a single tracker for the whole network (in an unbounded environment, we'd make one _per_ node generating
/// random activity, which has a view of the full network except for itself).
/// In order to preserve deterministic events using the same seed, it is necessary to sort the nodes by its pubkey keys.
pub struct NetworkGraphView {
    node_picker: WeightedIndex<u64>,
    nodes: Vec<(NodeInfo, u64)>,
    rng: MutRng,
}

impl NetworkGraphView {
    /// Creates a network view for the map of node public keys to capacity (in millisatoshis) provided. Returns an error
    /// if any node's capacity is zero (the node cannot receive), or there are not at least two nodes (one node can't
    /// send to itself).
    pub fn new(nodes: Vec<(NodeInfo, u64)>, rng: MutRng) -> Result<Self, RandomActivityError> {
        if nodes.len() < 2 {
            return Err(RandomActivityError::ValueError(
                "at least two nodes required for activity generation".to_string(),
            ));
        }

        if nodes.iter().any(|(_, v)| *v == 0) {
            return Err(RandomActivityError::InsufficientCapacity(
                "network generator created with zero capacity node".to_string(),
            ));
        }

        // In order to choose a deterministic destination is necessary to sort the nodes by its public key.
        let mut sorted_actives_nodes: Vec<(NodeInfo, u64)> = nodes;
        sorted_actives_nodes.sort_by(|n1, n2| n1.0.pubkey.cmp(&n2.0.pubkey));

        // To create a weighted index we're going to need a vector of nodes that we index and weights that are set
        // by their deployed capacity. To efficiently store our view of nodes capacity, we're also going to store
        // capacity along with the node info because we query the two at the same time. Zero capacity nodes are
        // filtered out because they have no chance of being selected (and wont' be able to receive payments).
        let node_picker = WeightedIndex::new(
            sorted_actives_nodes
                .iter()
                .map(|(_, v)| *v)
                .collect::<Vec<u64>>(),
        )
        .map_err(|e| RandomActivityError::ValueError(e.to_string()))?;

        Ok(NetworkGraphView {
            node_picker,
            nodes: sorted_actives_nodes,
            rng,
        })
    }
}

impl DestinationGenerator for NetworkGraphView {
    /// Randomly samples the network for a node, weighted by capacity.  Using a single graph view means that it's
    /// possible for a source node to select itself. After sufficient retries, this is highly improbable (even  with
    /// very small graphs, or those with one node significantly more capitalized than others).
    fn choose_destination(
        &self,
        source: PublicKey,
    ) -> Result<(NodeInfo, Option<u64>), DestinationGenerationError> {
        let mut rng = self
            .rng
            .0
            .lock()
            .map_err(|e| DestinationGenerationError(e.to_string()))?;
        // While it's very unlikely that we can't pick a destination that is not our source, it's possible that there's
        // a bug in our selection, so we track attempts to select a non-source node so that we can warn if this takes
        // improbably long.
        let mut i = 1;
        loop {
            let index = self.node_picker.sample(&mut *rng);
            // Unwrapping is safe given `NetworkGraphView` has the same amount of elements for `nodes` and `node_picker`
            let (node_info, capacity) = self.nodes.get(index).unwrap();

            if node_info.pubkey != source {
                return Ok((node_info.clone(), Some(*capacity)));
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
/// payment flows. For deterministic time between events and payment amounts, the `RandomPaymentActivity` keeps a
/// handle on a random number generator.
pub struct RandomPaymentActivity {
    multiplier: f64,
    expected_payment_amt: u64,
    source_capacity: u64,
    event_dist: Exp<f64>,
    rng: MutRng,
}

impl RandomPaymentActivity {
    /// Creates a new activity generator for a node, returning an error if the node has insufficient capacity deployed
    /// for the expected payment amount provided. Capacity is defined as the sum of the channels that the node has
    /// open in the network divided by two (so that capacity is not double counted with channel counterparties).
    pub fn new(
        source_capacity_msat: u64,
        expected_payment_amt: u64,
        multiplier: f64,
        rng: MutRng,
    ) -> Result<Self, RandomActivityError> {
        if source_capacity_msat == 0 {
            return Err(RandomActivityError::ValueError(
                "source_capacity_msat cannot be zero".into(),
            ));
        }

        if expected_payment_amt == 0 {
            return Err(RandomActivityError::ValueError(
                "expected_payment_amt cannot be zero".into(),
            ));
        }

        if multiplier == 0.0 {
            return Err(RandomActivityError::ValueError(
                "multiplier cannot be zero".into(),
            ));
        }

        RandomPaymentActivity::validate_capacity(source_capacity_msat, expected_payment_amt)?;

        // Lamda for the exponential distribution that we'll use to randomly time events is equal to the number of
        // events that we expect to see within our set period.

        let lamda = events_per_month(source_capacity_msat, multiplier, expected_payment_amt)
            / (SECONDS_PER_MONTH as f64);

        let event_dist =
            Exp::new(lamda).map_err(|e| RandomActivityError::ValueError(e.to_string()))?;

        Ok(RandomPaymentActivity {
            multiplier,
            expected_payment_amt,
            source_capacity: source_capacity_msat,
            event_dist,
            rng,
        })
    }

    /// Validates that the generator will be able to generate payment amounts based on the node's capacity and the
    /// simulation's expected payment amount.
    pub fn validate_capacity(
        node_capacity_msat: u64,
        expected_payment_amt: u64,
    ) -> Result<(), RandomActivityError> {
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
            return Err(RandomActivityError::InsufficientCapacity(format!(
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

impl PaymentGenerator for RandomPaymentActivity {
    /// Returns the time that the payments should start. This will always be 0 for the RandomPaymentActivity type.
    fn payment_start(&self) -> Option<Duration> {
        None
    }

    /// Returns the number of payments that should be made. This will always be None for the RandomPaymentActivity type.
    fn payment_count(&self) -> Option<u64> {
        None
    }

    /// Returns the amount of time until the next payment should be scheduled for the node.
    fn next_payment_wait(&self) -> Result<Duration, PaymentGenerationError> {
        let mut rng = self
            .rng
            .0
            .lock()
            .map_err(|e| PaymentGenerationError(e.to_string()))?;
        let duration_in_secs = self.event_dist.sample(&mut *rng) as u64;

        Ok(Duration::from_secs(duration_in_secs))
    }

    /// Returns the payment amount for a payment to a node with the destination capacity provided. The expected value
    /// for the payment is the simulation expected payment amount, and the variance is determined by the channel
    /// capacity of the source and destination node. Variance is calculated such that 95% of payment amounts generated
    /// will fall between the expected payment amount and 50% of the capacity of the node with the least channel
    /// capacity. While the expected value of payments remains the same, scaling variance by node capacity means that
    /// nodes with more deployed capital will see a larger range of payment values than those with smaller total
    /// channel capacity.
    fn payment_amount(
        &self,
        destination_capacity: Option<u64>,
    ) -> Result<u64, PaymentGenerationError> {
        let destination_capacity = destination_capacity.ok_or(PaymentGenerationError(
            "destination amount required for payment activity generator".to_string(),
        ))?;

        let payment_limit = std::cmp::min(self.source_capacity, destination_capacity) / 2;

        let ln_pmt_amt = (self.expected_payment_amt as f64).ln();
        let ln_limit = (payment_limit as f64).ln();

        let mu = 2.0 * ln_pmt_amt - ln_limit;
        let sigma_square = 2.0 * (ln_limit - ln_pmt_amt);

        if sigma_square < 0.0 {
            return Err(PaymentGenerationError(format!(
                "payment amount not possible for limit: {payment_limit}, sigma squared: {sigma_square}"
            )));
        }

        let log_normal = LogNormal::new(mu, sigma_square.sqrt())
            .map_err(|e| PaymentGenerationError(e.to_string()))?;

        let mut rng = self
            .rng
            .0
            .lock()
            .map_err(|e| PaymentGenerationError(e.to_string()))?;
        let payment_amount = log_normal.sample(&mut *rng) as u64;

        Ok(payment_amount)
    }
}

impl Display for RandomPaymentActivity {
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

#[cfg(test)]
mod tests {
    mod test_network_graph_view {
        use ntest::timeout;

        use super::super::*;
        use crate::test_utils::create_nodes;

        #[test]
        fn test_new() {
            // Check that we need, at least, two nodes
            let rng = MutRng::new(Some((u64::MAX, None)));
            for i in 0..2 {
                assert!(matches!(
                    NetworkGraphView::new(create_nodes(i, 42 * (i as u64 + 1)), rng.clone()),
                    Err(RandomActivityError::ValueError { .. })
                ));
            }

            // Check that, even if we have two nodes, the node capacity of all of them must be greater than 0
            // One of them is 0
            let mut nodes = create_nodes(1, 0);
            nodes.extend(create_nodes(1, 21));
            assert!(matches!(
                NetworkGraphView::new(nodes, rng.clone()),
                Err(RandomActivityError::InsufficientCapacity { .. })
            ));

            // All of them are 0
            assert!(matches!(
                NetworkGraphView::new(create_nodes(2, 0), rng.clone()),
                Err(RandomActivityError::InsufficientCapacity { .. })
            ));

            // Otherwise we should be good
            assert!(NetworkGraphView::new(create_nodes(2, 42), rng).is_ok());
        }

        #[test]
        #[timeout(5000)]
        fn test_sample_node_by_capacity() {
            // Sample node by capacity returns a node to be used as payment destination of a random payment generation
            // given a graph view and a source node, ensuring the source and the destination are distinct.
            // The method is guaranteed to return, though how long it takes to pick a destination depends on the graph:
            // the bigger a node's capacity within the graph, the more likely it is to be picked.
            //
            // For efficiency reasons, there is a single `NetworkGraphView` shared between all workers in the simulator, therefore,
            // the source is part of the sampling pool when a destination is requested. This means that if the source capacity is
            // extremely big compared to the rest of nodes in the graph, it may take extraordinarily long for a destination
            // to be found.
            //
            // This tests a completely unrealistic yet pathological setup in where a single node has the vast majority of the network's
            // capacity, while every other single node has almost none. The scenario represents a start topology in where the big node has a
            // connection with every single other node, while the rest are only connected to it. Even in this extreme and unrealistic
            // situation, the method returns rather fast.

            let small_node_count = 999;
            let big_node_count = 1;
            let small_node_capacity = 1_000;
            let big_node_capacity = small_node_capacity * small_node_count as u64;

            let mut nodes = create_nodes(small_node_count, small_node_capacity);
            nodes.extend(create_nodes(big_node_count, big_node_capacity));
            let big_node = nodes.last().unwrap().0.pubkey;

            let rng = MutRng::new(Some((u64::MAX, None)));
            let view = NetworkGraphView::new(nodes, rng).unwrap();

            for _ in 0..10 {
                view.choose_destination(big_node).unwrap();
            }
        }
    }

    mod payment_activity_generator {
        use super::super::*;
        use crate::test_utils::get_random_int;

        #[test]
        fn test_new() {
            // For the payment activity generator to fail during construction either the provided capacity must fail validation or the exponential
            // distribution must fail building given the inputs. The former will be thoroughly tested in its own unit test, but we'll test some basic cases
            // here. Mainly, if the `capacity < expected_payment_amnt / 2`, the generator will fail building
            let rng = MutRng::new(Some((u64::MAX, None)));
            let expected_payment = get_random_int(1, 100);
            assert!(RandomPaymentActivity::new(
                2 * expected_payment,
                expected_payment,
                1.0,
                rng.clone()
            )
            .is_ok());
            assert!(matches!(
                RandomPaymentActivity::new(
                    2 * expected_payment,
                    expected_payment + 1,
                    1.0,
                    rng.clone()
                ),
                Err(RandomActivityError::InsufficientCapacity { .. })
            ));

            // Respecting the internal exponential distribution creation, neither of the parameters can be zero. Otherwise we may try to create an exponential
            // function with lambda = NaN, which will error out, or with lambda = Inf, which does not make sense for our use-case
            assert!(matches!(
                RandomPaymentActivity::new(
                    0,
                    get_random_int(1, 10),
                    get_random_int(1, 10) as f64,
                    rng.clone()
                ),
                Err(RandomActivityError::ValueError { .. })
            ));
            assert!(matches!(
                RandomPaymentActivity::new(
                    get_random_int(1, 10),
                    0,
                    get_random_int(1, 10) as f64,
                    rng.clone()
                ),
                Err(RandomActivityError::ValueError { .. })
            ));
            assert!(matches!(
                RandomPaymentActivity::new(get_random_int(1, 10), get_random_int(1, 10), 0.0, rng),
                Err(RandomActivityError::ValueError { .. })
            ));
        }

        #[test]
        fn test_validate_capacity() {
            // There's not much to be tested here, given a `node_capacity` and an `expected_payment`
            // if the former over two is smaller than the latter, the function will error out
            for _ in 0..=get_random_int(20, 100) {
                let capacity = get_random_int(0, 100);
                let payment_amt = get_random_int(0, 100);

                let r = RandomPaymentActivity::validate_capacity(capacity, payment_amt);
                if capacity < 2 * payment_amt {
                    assert!(matches!(
                        r,
                        Err(RandomActivityError::InsufficientCapacity { .. })
                    ));
                } else {
                    assert!(r.is_ok());
                }
            }
        }

        #[test]
        fn test_payment_amount() {
            // The special cases for payment_amount are those who may make the internal log normal distribution fail to build, which happens if
            // sigma squared is either +-INF or NaN. Given that the constructor of the PaymentActivityGenerator already forces its internal values
            // to be greater than zero, the only values that are left are all values of `destination_capacity` smaller or equal to the `source_capacity`
            // All of them will yield a sigma squared smaller than 0, which we have a sanity check for.
            let expected_payment = get_random_int(1, 100);
            let source_capacity = 2 * expected_payment;
            let rng = MutRng::new(Some((u64::MAX, None)));
            let pag =
                RandomPaymentActivity::new(source_capacity, expected_payment, 1.0, rng).unwrap();

            // Wrong cases
            for i in 0..source_capacity {
                assert!(matches!(
                    pag.payment_amount(Some(i)),
                    Err(PaymentGenerationError(..))
                ))
            }

            // All other cases will work. We are not going to exhaustively test for the rest up to u64::MAX, let just pick a bunch
            for i in source_capacity + 1..100 * source_capacity {
                assert!(pag.payment_amount(Some(i)).is_ok())
            }

            // We can even try really high numbers to make sure they are not troublesome
            for i in u64::MAX - 10000..u64::MAX {
                assert!(pag.payment_amount(Some(i)).is_ok())
            }

            assert!(matches!(
                pag.payment_amount(None),
                Err(PaymentGenerationError(..))
            ));
        }
    }
}
