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

/// The 95th percentile (z-score) of the standard normal distribution. Payment amount distributions are chosen such
/// that at least 95% of sampled amounts fall below the payment limit for the node pair.
const PAYMENT_LIMIT_QUANTILE: f64 = 1.6449;

/// The maximum sigma for the log normal distribution that payment amounts are sampled from, sqrt(ln(2)), which caps
/// the distribution's coefficient of variation (standard deviation / mean) at one. Bounding sigma keeps the tail of
/// the distribution light enough that the average of sampled payment amounts converges on the expected payment
/// amount within a reasonable number of payments (the standard error of the mean over n payments is at most
/// expected_payment_amt / sqrt(n)).
const MAX_SIGMA: f64 = 0.8325546111576977;

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
        // We will not be able to generate payments if the payment limit for a node pair (half of the smaller node's
        // capacity) is beneath the expected payment amount, because no log normal distribution has the expected
        // amount as its mean while keeping 95% of its samples below a limit that is smaller than that mean.
        //
        // The payment limit is at most node_capacity_msat / 2 (it may be smaller, depending on the capacity of the
        // destination chosen per-payment), so we require:
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

    /// Returns the payment amount for a payment to a node with the destination capacity provided. Amounts are
    /// sampled from a log normal distribution with mean equal to the simulation's expected payment amount, and
    /// variance chosen per node-pair such that at least 95% of sampled amounts fall below a payment limit of 50%
    /// of the capacity of the node with the least channel capacity. Node pairs with more deployed capital therefore
    /// see a wider range of payment amounts, while the average amount remains the expected payment amount, so the
    /// volume that a node sends over time converges on the total implied by its capacity and the simulation's
    /// activity multiplier.
    fn payment_amount(
        &self,
        destination_capacity: Option<u64>,
    ) -> Result<u64, PaymentGenerationError> {
        let destination_capacity = destination_capacity.ok_or(PaymentGenerationError(
            "destination amount required for payment activity generator".to_string(),
        ))?;

        let payment_limit = std::cmp::min(self.source_capacity, destination_capacity) / 2;

        // A log normal distribution with mu = ln(mean) - sigma^2 / 2 has the expected payment amount as its mean
        // for any choice of sigma. We pick sigma as large as possible for the node pair, subject to two constraints:
        // * At least 95% of sampled amounts fall below the payment limit, which requires:
        //   (ln(limit) - mu) / sigma >= z, ie: sigma^2 - 2 * z * sigma + 2 * ln(limit / mean) >= 0.
        // * Sigma may not exceed MAX_SIGMA, so that the tail of the distribution stays light enough for sample
        //   averages to converge on the mean quickly.
        //
        // When the quadratic's discriminant is negative, the limit constraint holds for any sigma (the limit is far
        // above the mean); otherwise sigma must fall below the quadratic's smaller root. The region above the larger
        // root also satisfies the constraint, but produces a heavy-tailed distribution where most payments are tiny
        // and the volume target is carried by extremely rare, large payments.
        let ln_ratio = (payment_limit as f64 / self.expected_payment_amt as f64).ln();
        if ln_ratio < 0.0 {
            return Err(PaymentGenerationError(format!(
                "expected payment amount: {} exceeds payment limit: {payment_limit}",
                self.expected_payment_amt
            )));
        }

        let discriminant = PAYMENT_LIMIT_QUANTILE * PAYMENT_LIMIT_QUANTILE - 2.0 * ln_ratio;
        let sigma = if discriminant <= 0.0 {
            MAX_SIGMA
        } else {
            MAX_SIGMA.min(PAYMENT_LIMIT_QUANTILE - discriminant.sqrt())
        };
        let mu = (self.expected_payment_amt as f64).ln() - sigma * sigma / 2.0;

        let log_normal =
            LogNormal::new(mu, sigma).map_err(|e| PaymentGenerationError(e.to_string()))?;

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
            // Payment amounts can only be generated if the payment limit for the node pair (half of the smaller
            // node's capacity) is at least the expected payment amount, because no log normal distribution has the
            // expected amount as its mean while keeping 95% of samples below a limit that is smaller than that mean.
            // Given that the constructor of the PaymentActivityGenerator already forces its internal values to be
            // greater than zero, the only values that are left are all values of `destination_capacity` small enough
            // to drag the payment limit beneath the expected payment amount, which we have a sanity check for.
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

        #[test]
        fn test_payment_amount_mean_convergence() {
            // The average of generated payment amounts should converge on the expected payment amount within a
            // reasonable number of payments, otherwise nodes do not send the volume that their capacity and the
            // simulation's activity multiplier imply. Capacities are much larger than the expected payment amount
            // so that sigma is at its cap; with a coefficient of variation of at most one, the standard error of
            // the mean over 1000 samples is ~3% of the expected amount, so a 15% bound has comfortable headroom
            // while still catching heavy-tailed parametrizations (which produce sample means that are off by
            // orders of magnitude).
            let expected_payment = 3_800_000;
            let capacity = 1_000_000_000_000;
            let rng = MutRng::new(Some((42, None)));
            let pag = RandomPaymentActivity::new(capacity, expected_payment, 1.0, rng).unwrap();

            let n = 1000;
            let sum: u64 = (0..n)
                .map(|_| pag.payment_amount(Some(capacity)).unwrap())
                .sum();
            let mean = sum as f64 / n as f64;

            assert!(
                (mean - expected_payment as f64).abs() < 0.15 * expected_payment as f64,
                "sample mean {mean} not within 15% of expected payment amount {expected_payment}",
            );
        }

        #[test]
        fn test_payment_amount_respects_limit() {
            // At least 95% of payment amounts should fall below the payment limit of half of the smaller node's
            // capacity. The destination is small relative to the source so that the limit constraint (rather than
            // the cap on sigma) binds.
            let expected_payment = 3_800_000;
            let rng = MutRng::new(Some((42, None)));
            let pag =
                RandomPaymentActivity::new(1_000_000_000_000, expected_payment, 1.0, rng).unwrap();

            let destination_capacity = 10 * expected_payment;
            let limit = destination_capacity / 2;
            let n = 1000;
            let below = (0..n)
                .filter(|_| pag.payment_amount(Some(destination_capacity)).unwrap() <= limit)
                .count();

            assert!(
                below >= 950,
                "expected at least 950/1000 payment amounts below limit {limit}, got {below}",
            );
        }

        #[test]
        fn test_payment_amount_degenerate_limit() {
            // When the payment limit is exactly the expected payment amount, the only distribution that has the
            // expected amount as its mean and respects the limit is a constant, so every payment should be the
            // expected amount (within one msat of floating point truncation).
            let expected_payment = 3_800_000;
            let rng = MutRng::new(Some((42, None)));
            let pag = RandomPaymentActivity::new(2 * expected_payment, expected_payment, 1.0, rng)
                .unwrap();

            for _ in 0..10 {
                let amt = pag.payment_amount(Some(2 * expected_payment)).unwrap();
                assert!(
                    amt.abs_diff(expected_payment) <= 1,
                    "expected constant payment of {expected_payment}, got {amt}",
                );
            }
        }
    }
}
