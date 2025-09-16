use crate::clock::Clock;
use crate::{
    Graph, LightningError, LightningNode, NodeInfo, PaymentOutcome, PaymentResult, SimulationError,
};
use async_trait::async_trait;
use bitcoin::constants::ChainHash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Network, ScriptBuf, TxOut};
use lightning::ln::chan_utils::make_funding_redeemscript;
use serde::{Deserialize, Serialize};
use std::collections::{hash_map::Entry, HashMap};
use std::fmt::Display;
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::task::JoinSet;
use tokio_util::task::TaskTracker;

use lightning::ln::features::{ChannelFeatures, NodeFeatures};
use lightning::ln::msgs::{
    LightningError as LdkError, UnsignedChannelAnnouncement, UnsignedChannelUpdate,
};
use lightning::ln::{PaymentHash, PaymentPreimage};
use lightning::routing::gossip::{NetworkGraph, NodeId};
use lightning::routing::router::{find_route, Path, PaymentParameters, Route, RouteParameters};
use lightning::routing::scoring::{
    ProbabilisticScorer, ProbabilisticScoringDecayParameters, ScoreUpdate,
};
use lightning::routing::utxo::{UtxoLookup, UtxoResult};
use lightning::util::logger::{Level, Logger, Record};
use thiserror::Error;
use tokio::select;
use tokio::sync::oneshot::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use triggered::{Listener, Trigger};

use crate::ShortChannelID;

/// ForwardingError represents the various errors that we can run into when forwarding payments in a simulated network.
/// Since we're not using real lightning nodes, these errors are not obfuscated and can be propagated to the sending
/// node and used for analysis.
#[derive(Debug, Error)]
pub enum ForwardingError {
    /// The forwarding node did not have sufficient outgoing balance to forward the htlc (htlc amount / balance).
    #[error("InsufficientBalance: amount: {0} > balance: {1}")]
    InsufficientBalance(u64, u64),
    /// The htlc forwarded is less than the channel's advertised minimum htlc amount (htlc amount / minimum).
    #[error("LessThanMinimum: amount: {0} < minimum: {1}")]
    LessThanMinimum(u64, u64),
    /// The htlc forwarded is more than the channel's advertised maximum htlc amount (htlc amount / maximum).
    #[error("MoreThanMaximum: amount: {0} > maximum: {1}")]
    MoreThanMaximum(u64, u64),
    /// The channel has reached its maximum allowable number of htlcs in flight (total in flight / maximim).
    #[error("ExceedsInFlightCount: total in flight: {0} > maximum count: {1}")]
    ExceedsInFlightCount(u64, u64),
    /// The forwarded htlc's amount would push the channel over its maximum allowable in flight total
    /// (total in flight / maximum).
    #[error("ExceedsInFlightTotal: total in flight amount: {0} > maximum amount: {0}")]
    ExceedsInFlightTotal(u64, u64),
    /// The forwarded htlc's cltv expiry exceeds the maximum value used to express block heights in Bitcoin.
    #[error("ExpiryInSeconds: cltv expressed in seconds: {0}")]
    ExpiryInSeconds(u32, u32),
    /// The forwarded htlc has insufficient cltv delta for the channel's minimum delta (cltv delta / minimum).
    #[error("InsufficientCltvDelta: cltv delta: {0} < required: {1}")]
    InsufficientCltvDelta(u32, u32),
    /// The forwarded htlc has insufficient fee for the channel's policy (fee / expected fee / base fee / prop fee).
    #[error("InsufficientFee: offered fee: {0} (base: {1}, prop: {2}) < expected: {3}")]
    InsufficientFee(u64, u64, u64, u64),
    /// Custom error for interceptors if encountered a forwarding error during htlc interception.
    #[error("InterceptorError: {0}")]
    InterceptorError(String),
}

/// CriticalError represents an error while propagating a payment in a simulated network that
/// warrants a shutdown.
#[derive(Debug, Error)]
pub enum CriticalError {
    /// Zero amount htlcs are invalid in the protocol.
    #[error("ZeroAmountHtlc")]
    ZeroAmountHtlc,
    /// The outgoing channel id was not found in the network graph.
    #[error("ChannelNotFound: {0}")]
    ChannelNotFound(ShortChannelID),
    /// The node pubkey provided was not associated with the channel in the network graph.
    #[error("NodeNotFound: {0:?}")]
    NodeNotFound(PublicKey),
    /// The channel has already forwarded an HTLC with the payment hash provided.
    /// TODO: remove if MPP support is added.
    #[error("PaymentHashExists: {0:?}")]
    PaymentHashExists(PaymentHash),
    /// An htlc with the payment hash provided could not be found to resolve.
    #[error("PaymentHashNotFound: {0:?}")]
    PaymentHashNotFound(PaymentHash),
    /// The fee policy for a htlc amount would overflow with the given fee policy (htlc amount / base fee / prop fee).
    #[error("FeeOverflow: htlc amount: {0} (base: {1}, prop: {2})")]
    FeeOverflow(u64, u64, u64),
    /// Sanity check on channel balances failed (node balances / channel capacity).
    #[error("SanityCheckFailed: node balance: {0} != capacity: {1}")]
    SanityCheckFailed(u64, u64),
    /// Intercepted HTLCs have duplicated custom records attached.
    #[error("DuplicateCustomRecord: key {0}")]
    DuplicateCustomRecord(u64),
    /// Custom error for interceptors if they encountered a critical error during htlc
    /// interception.
    #[error("InterceptorError: {0}")]
    InterceptorError(String),
}

type ForwardResult = Result<Result<(), ForwardingError>, CriticalError>;

/// Represents an in-flight htlc that has been forwarded over a channel that is awaiting resolution.
#[derive(Copy, Clone)]
struct Htlc {
    amount_msat: u64,
    cltv_expiry: u32,
}

/// Represents one node in the channel's forwarding policy and restrictions. Note that this doesn't directly map to
/// a single concept in the protocol, a few things have been combined for the sake of simplicity. Used to manage the
/// lightning "state machine" and check that HTLCs are added in accordance of the advertised policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelPolicy {
    pub pubkey: PublicKey,
    #[serde(default)]
    pub alias: String,
    pub max_htlc_count: u64,
    pub max_in_flight_msat: u64,
    pub min_htlc_size_msat: u64,
    pub max_htlc_size_msat: u64,
    pub cltv_expiry_delta: u32,
    pub base_fee: u64,
    pub fee_rate_prop: u64,
}

impl ChannelPolicy {
    /// Validates that the channel policy is acceptable for the size of the channel.
    fn validate(&self, capacity_msat: u64) -> Result<(), SimulationError> {
        if self.max_in_flight_msat > capacity_msat {
            return Err(SimulationError::SimulatedNetworkError(format!(
                "max_in_flight_msat {} > capacity {}",
                self.max_in_flight_msat, capacity_msat
            )));
        }
        if self.max_htlc_size_msat > capacity_msat {
            return Err(SimulationError::SimulatedNetworkError(format!(
                "max_htlc_size_msat {} > capacity {}",
                self.max_htlc_size_msat, capacity_msat
            )));
        }
        Ok(())
    }
}

/// Fails with the forwarding error provided if the value provided fails its inequality check.
macro_rules! fail_forwarding_inequality {
    ($value_1:expr, $op:tt, $value_2:expr, $error_variant:ident $(, $opt:expr)*) => {
        if $value_1 $op $value_2 {
            return Err(ForwardingError::$error_variant(
                    $value_1,
                    $value_2
                    $(
                        , $opt
                    )*
             ));
        }
    };
}

/// The internal state of one side of a simulated channel, including its forwarding parameters. This struct is
/// primarily responsible for handling our view of what's currently in-flight on the channel, and how much
/// liquidity we have.
#[derive(Clone)]
struct ChannelState {
    local_balance_msat: u64,
    /// Maps payment hash to htlc and index that it was added at.
    in_flight: HashMap<PaymentHash, (Htlc, u64)>,
    policy: ChannelPolicy,
    /// Tracks unique identifier for htlcs proposed by this node (sent in the outgoing direction).
    index: u64,
}

impl ChannelState {
    /// Creates a new channel with local liquidity as allocated by the caller. The responsibility of ensuring that the
    /// local balance of each side of the channel equals its total capacity is on the caller, as we are only dealing
    /// with a one-sided view of the channel's state.
    fn new(policy: ChannelPolicy, local_balance_msat: u64) -> Self {
        ChannelState {
            local_balance_msat,
            in_flight: HashMap::new(),
            policy,
            index: 0,
        }
    }

    /// Returns the sum of all the *in flight outgoing* HTLCs on the channel.
    fn in_flight_total(&self) -> u64 {
        self.in_flight.values().map(|h| h.0.amount_msat).sum()
    }

    /// Checks whether the proposed HTLC abides by the channel policy advertised for using this channel as the
    /// *outgoing* link in a forward.
    fn check_htlc_forward(&self, cltv_delta: u32, amt: u64, fee: u64) -> ForwardResult {
        if cltv_delta < self.policy.cltv_expiry_delta {
            return Ok(Err(ForwardingError::InsufficientCltvDelta(
                cltv_delta,
                self.policy.cltv_expiry_delta,
            )));
        }

        let expected_fee = amt
            .checked_mul(self.policy.fee_rate_prop)
            .and_then(|prop_fee| (prop_fee / 1000000).checked_add(self.policy.base_fee))
            .ok_or(CriticalError::FeeOverflow(
                amt,
                self.policy.base_fee,
                self.policy.fee_rate_prop,
            ))?;

        if fee < expected_fee {
            return Ok(Err(ForwardingError::InsufficientFee(
                fee,
                self.policy.base_fee,
                self.policy.fee_rate_prop,
                expected_fee,
            )));
        }

        Ok(Ok(()))
    }

    /// Checks whether the proposed HTLC can be added to the channel as an outgoing HTLC. This requires that we have
    /// sufficient liquidity, and that the restrictions on our in flight htlc balance and count are not violated by
    /// the addition of the HTLC. Specification sanity checks (such as reasonable CLTV) are also included, as this
    /// is where we'd check it in real life.
    fn check_outgoing_addition(&self, htlc: &Htlc) -> Result<(), ForwardingError> {
        fail_forwarding_inequality!(htlc.amount_msat, >, self.policy.max_htlc_size_msat, MoreThanMaximum);
        fail_forwarding_inequality!(htlc.amount_msat, <, self.policy.min_htlc_size_msat, LessThanMinimum);
        fail_forwarding_inequality!(
            self.in_flight.len() as u64 + 1, >, self.policy.max_htlc_count, ExceedsInFlightCount
        );
        fail_forwarding_inequality!(
            self.in_flight_total() + htlc.amount_msat, >, self.policy.max_in_flight_msat, ExceedsInFlightTotal
        );
        fail_forwarding_inequality!(htlc.amount_msat, >, self.local_balance_msat, InsufficientBalance);
        fail_forwarding_inequality!(htlc.cltv_expiry, >, 500000000, ExpiryInSeconds);

        Ok(())
    }

    /// Adds the HTLC to our set of outgoing in-flight HTLCs. [`check_outgoing_addition`] must be called before
    /// this to ensure that the restrictions on outgoing HTLCs are not violated. Local balance is decreased by the
    /// HTLC amount, as this liquidity is no longer available.
    ///
    /// Note: MPP payments are not currently supported, so this function will fail if a duplicate payment hash is
    /// reported.
    fn add_outgoing_htlc(
        &mut self,
        hash: PaymentHash,
        htlc: Htlc,
    ) -> Result<Result<u64, ForwardingError>, CriticalError> {
        if let Err(fwd_err) = self.check_outgoing_addition(&htlc) {
            return Ok(Err(fwd_err));
        }

        if self.in_flight.contains_key(&hash) {
            return Err(CriticalError::PaymentHashExists(hash));
        }
        let index = self.index;
        self.index += 1;

        self.local_balance_msat -= htlc.amount_msat;
        self.in_flight.insert(hash, (htlc, index));
        Ok(Ok(index))
    }

    /// Removes the HTLC from our set of outgoing in-flight HTLCs, failing if the payment hash is not found.
    fn remove_outgoing_htlc(&mut self, hash: &PaymentHash) -> Result<(Htlc, u64), CriticalError> {
        self.in_flight
            .remove(hash)
            .ok_or(CriticalError::PaymentHashNotFound(*hash))
    }

    // Updates channel state to account for the resolution of an outgoing in-flight HTLC. If the HTLC failed, the
    // balance is failed back to the channel's local balance. If not, the in-flight balance is settled to the other
    // node, so there is no operation.
    fn settle_outgoing_htlc(&mut self, amt: u64, success: bool) {
        if !success {
            self.local_balance_msat += amt
        }
    }

    // Updates channel state to account for the resolution of an incoming in-flight HTLC. If the HTLC succeeded,
    // the balance is settled to the channel's local balance. If not, the in-flight balance is failed back to the
    // other node, so there is no operation.
    fn settle_incoming_htlc(&mut self, amt: u64, success: bool) {
        if success {
            self.local_balance_msat += amt
        }
    }
}

/// Represents a simulated channel, and is responsible for managing addition and removal of HTLCs from the channel and
/// sanity checks. Channel state is tracked *unidirectionally* for each participant in the channel.
///
/// Each node represented in the channel tracks only its outgoing HTLCs, and balance is transferred between the two
/// nodes as they settle or fail. Given some channel: node_1 <----> node_2:
/// * HTLC sent node_1 -> node_2: added to in-flight outgoing htlcs on node_1.
/// * HTLC sent node_2 -> node_1: added to in-flight outgoing htlcs on node_2.
///
/// Rules for managing balance are as follows:
/// * When an HTLC is in flight, the channel's local outgoing liquidity decreases (as it's locked up).
/// * When an HTLC fails, the balance is returned to the local node (the one that it was in-flight / outgoing on).
/// * When an HTLC succeeds, the balance is sent to the remote node (the one that did not track it as in-flight).
///
/// With each state transition, the simulated channel checks that the sum of its local balances and in-flight equal the
/// total channel capacity. Failure of this sanity check represents a critical failure in the state machine.
#[derive(Clone)]
pub struct SimulatedChannel {
    capacity_msat: u64,
    short_channel_id: ShortChannelID,
    node_1: ChannelState,
    node_2: ChannelState,
}

impl SimulatedChannel {
    /// Creates a new channel with the capacity and policies provided. The total capacity of the channel is evenly split
    /// between the channel participants (this is an arbitrary decision).
    pub fn new(
        capacity_msat: u64,
        short_channel_id: ShortChannelID,
        node_1: ChannelPolicy,
        node_2: ChannelPolicy,
    ) -> Self {
        SimulatedChannel {
            capacity_msat,
            short_channel_id,
            node_1: ChannelState::new(node_1, capacity_msat / 2),
            node_2: ChannelState::new(node_2, capacity_msat / 2),
        }
    }

    /// Gets the public key of node 1 in the channel.
    pub fn get_node_1_pubkey(&self) -> PublicKey {
        self.node_1.policy.pubkey
    }

    /// Gets the public key of node 2 in the channel.
    pub fn get_node_2_pubkey(&self) -> PublicKey {
        self.node_2.policy.pubkey
    }

    /// Validates that a simulated channel has distinct node pairs and valid routing policies.
    fn validate(&self) -> Result<(), SimulationError> {
        if self.node_1.policy.pubkey == self.node_2.policy.pubkey {
            return Err(SimulationError::SimulatedNetworkError(format!(
                "Channel should have distinct node pubkeys, got: {} for both nodes.",
                self.node_1.policy.pubkey
            )));
        }

        self.node_1.policy.validate(self.capacity_msat)?;
        self.node_2.policy.validate(self.capacity_msat)?;

        Ok(())
    }

    fn get_node_mut(&mut self, pubkey: &PublicKey) -> Result<&mut ChannelState, CriticalError> {
        if pubkey == &self.node_1.policy.pubkey {
            Ok(&mut self.node_1)
        } else if pubkey == &self.node_2.policy.pubkey {
            Ok(&mut self.node_2)
        } else {
            Err(CriticalError::NodeNotFound(*pubkey))
        }
    }

    fn get_node(&self, pubkey: &PublicKey) -> Result<&ChannelState, CriticalError> {
        if pubkey == &self.node_1.policy.pubkey {
            Ok(&self.node_1)
        } else if pubkey == &self.node_2.policy.pubkey {
            Ok(&self.node_2)
        } else {
            Err(CriticalError::NodeNotFound(*pubkey))
        }
    }

    /// Adds an htlc to the appropriate side of the simulated channel, checking its policy and balance are okay. The
    /// public key of the node sending the HTLC (ie, the party that would send update_add_htlc in the protocol)
    /// must be provided to add the outgoing htlc to its side of the channel.
    fn add_htlc(
        &mut self,
        sending_node: &PublicKey,
        hash: PaymentHash,
        htlc: Htlc,
    ) -> Result<Result<u64, ForwardingError>, CriticalError> {
        if htlc.amount_msat == 0 {
            return Err(CriticalError::ZeroAmountHtlc);
        }

        self.sanity_check()?;
        self.get_node_mut(sending_node)?
            .add_outgoing_htlc(hash, htlc)
    }

    /// Performs a sanity check on the total balances in a channel. Note that we do not currently include on-chain
    /// fees or reserve so these values should exactly match.
    fn sanity_check(&self) -> Result<(), CriticalError> {
        let node_1_total = self.node_1.local_balance_msat + self.node_1.in_flight_total();
        let node_2_total = self.node_2.local_balance_msat + self.node_2.in_flight_total();

        let channel_balance = node_1_total + node_2_total;
        if channel_balance != self.capacity_msat {
            return Err(CriticalError::SanityCheckFailed(
                channel_balance,
                self.capacity_msat,
            ));
        }

        Ok(())
    }

    /// Removes an htlc from the appropriate side of the simulated channel, settling balances across channel sides
    /// based on the success of the htlc. The public key of the node that originally sent the HTLC (ie, the party
    /// that would send update_add_htlc in the protocol) must be provided to remove the htlc from its side of the
    /// channel.
    fn remove_htlc(
        &mut self,
        sending_node: &PublicKey,
        hash: &PaymentHash,
        success: bool,
    ) -> Result<(Htlc, u64), CriticalError> {
        let htlc = self
            .get_node_mut(sending_node)?
            .remove_outgoing_htlc(hash)?;
        self.settle_htlc(sending_node, htlc.0.amount_msat, success)?;
        self.sanity_check()?;

        Ok(htlc)
    }

    /// Updates the local balance of each node in the channel once a htlc has been resolved, pushing funds to the
    /// receiving nodes in the case of a successful payment and returning balance to the sender in the case of a
    /// failure.
    fn settle_htlc(
        &mut self,
        sending_node: &PublicKey,
        amount_msat: u64,
        success: bool,
    ) -> Result<(), CriticalError> {
        if sending_node == &self.node_1.policy.pubkey {
            self.node_1.settle_outgoing_htlc(amount_msat, success);
            self.node_2.settle_incoming_htlc(amount_msat, success);
            Ok(())
        } else if sending_node == &self.node_2.policy.pubkey {
            self.node_2.settle_outgoing_htlc(amount_msat, success);
            self.node_1.settle_incoming_htlc(amount_msat, success);
            Ok(())
        } else {
            Err(CriticalError::NodeNotFound(*sending_node))
        }
    }

    /// Checks an htlc forward against the outgoing policy of the node provided.
    fn check_htlc_forward(
        &self,
        forwarding_node: &PublicKey,
        cltv_delta: u32,
        amount_msat: u64,
        fee_msat: u64,
    ) -> ForwardResult {
        self.get_node(forwarding_node)?
            .check_htlc_forward(cltv_delta, amount_msat, fee_msat)
    }

    pub fn create_simulated_nodes(&self) -> (NodeInfo, NodeInfo) {
        (
            node_info(self.node_1.policy.pubkey, self.node_1.policy.alias.clone()),
            node_info(self.node_2.policy.pubkey, self.node_2.policy.alias.clone()),
        )
    }
}

/// SimNetwork represents a high level network coordinator that is responsible for the task of actually propagating
/// payments through the simulated network.
pub trait SimNetwork: Send + Sync {
    /// Sends payments over the route provided through the network, reporting the final payment outcome to the sender
    /// channel provided.
    fn dispatch_payment(
        &mut self,
        source: PublicKey,
        route: Route,
        custom_records: Option<CustomRecords>,
        payment_hash: PaymentHash,
        sender: Sender<Result<PaymentResult, LightningError>>,
    );

    /// Looks up a node in the simulated network and a list of its channel capacities.
    fn lookup_node(&self, node: &PublicKey) -> Result<(NodeInfo, Vec<u64>), LightningError>;
    /// Lists all nodes in the simulated network.
    fn list_nodes(&self) -> Vec<NodeInfo>;
}

//type LdkNetworkGraph = NetworkGraph<Arc<WrappedLog>>;
type LdkNetworkGraph = NetworkGraph<&'static WrappedLog>;
/// A trait for custom pathfinding implementations.
/// Finds a route from the source node to the destination node for the specified amount.
///
/// # Arguments
/// * `source` - The public key of the node initiating the payment.
/// * `dest` - The public key of the destination node to receive the payment.
/// * `amount_msat` - The amount to send in millisatoshis.
/// * `pathfinding_graph` - The network graph containing channel topology and routing information.
///
/// # Returns
/// Returns a `Route` containing the payment path, or a `SimulationError` if no route is found.
pub trait PathFinder: Send + Sync + Clone {
    fn find_route(
        &self,
        source: &PublicKey,
        dest: PublicKey,
        amount_msat: u64,
        pathfinding_graph: &LdkNetworkGraph,
    ) -> Result<Route, SimulationError>;
}

/// The default pathfinding implementation that uses LDK's built-in pathfinding algorithm.
#[derive(Clone)]
pub struct DefaultPathFinder;

impl DefaultPathFinder {
    pub fn new() -> Self {
        Self
    }
}

impl PathFinder for DefaultPathFinder {
    fn find_route(
        &self,
        source: &PublicKey,
        dest: PublicKey,
        amount_msat: u64,
        pathfinding_graph: &NetworkGraph<&'static WrappedLog>,
    ) -> Result<Route, SimulationError> {
        let scorer_graph = NetworkGraph::new(bitcoin::Network::Regtest, &WrappedLog {});
        let scorer = ProbabilisticScorer::new(
            ProbabilisticScoringDecayParameters::default(),
            Arc::new(scorer_graph),
            &WrappedLog {},
        );

        // Call LDK's find_route with the scorer (LDK-specific requirement)
        find_route(
            source,
            &RouteParameters {
                payment_params: PaymentParameters::from_node_id(dest, 0)
                    .with_max_total_cltv_expiry_delta(u32::MAX)
                    .with_max_path_count(1)
                    .with_max_channel_saturation_power_of_half(1),
                final_value_msat: amount_msat,
                max_total_routing_fee_msat: None,
            },
            pathfinding_graph, // This is the real network graph used for pathfinding
            None,
            &WrappedLog {},
            &scorer, // LDK requires a scorer, so we provide a simple one
            &Default::default(),
            &[0; 32],
        )
        .map_err(|e| SimulationError::SimulatedNetworkError(e.err))
    }
}

struct InFlightPayment {
    /// The channel used to report payment results to.
    track_payment_receiver: Receiver<Result<PaymentResult, LightningError>>,
    /// The path the payment was dispatched on.
    /// This should be set to `None` if no payment path was found and the payment
    /// was not dispatched.
    path: Option<Path>,
}

/// A wrapper struct used to implement the LightningNode trait (can be thought of as "the" lightning node). Passes
/// all functionality through to a coordinating simulation network. This implementation contains both the [`SimNetwork`]
/// implementation that will allow us to dispatch payments and a read-only NetworkGraph that is used for pathfinding.
/// While these two could be combined, we re-use the LDK-native struct to allow re-use of their pathfinding logic.
pub struct SimNode<T: SimNetwork, C: Clock, P: PathFinder = DefaultPathFinder> {
    info: NodeInfo,
    /// The underlying execution network that will be responsible for dispatching payments.
    network: Arc<Mutex<T>>,
    /// Tracks the channel that will provide updates for payments by hash.
    in_flight: Mutex<HashMap<PaymentHash, InFlightPayment>>,
    /// A read-only graph used for pathfinding.
    pathfinding_graph: Arc<LdkNetworkGraph>,
    /// Clock for tracking simulation time.
    clock: Arc<C>,
    /// The pathfinder implementation to use for finding routes
    pathfinder: P,
}

impl<T: SimNetwork, C: Clock, P: PathFinder> SimNode<T, C, P> {
    /// Creates a new simulation node that refers to the high level network coordinator provided to process payments
    /// on its behalf. The pathfinding graph is provided separately so that each node can handle its own pathfinding.
    pub fn new(
        info: NodeInfo,
        payment_network: Arc<Mutex<T>>,
        pathfinding_graph: Arc<LdkNetworkGraph>,
        clock: Arc<C>,
        pathfinder: P,
    ) -> Self {
        SimNode {
            info,
            network: payment_network,
            in_flight: Mutex::new(HashMap::new()),
            pathfinding_graph,
            clock,
            pathfinder,
        }
    }

    /// Dispatches a payment to a specified route. If `custom_records` is `Some`, they will be attached to the outgoing
    /// update_add_htlc, otherwise [`SimGraph::default_custom_records`] will be used if `None`. If default custom records
    /// are configured, but you want to send a payment without them specify `Some(CustomRecords::default())`.
    ///
    /// The [`lightning::routing::router::build_route_from_hops`] function can be used to build the route to be passed here.
    ///
    /// **Note:** The payment hash passed in here should be used in track_payment to track the payment outcome.
    ///
    /// **Note:** The route passed in here must contain only one path.
    pub async fn send_to_route(
        &mut self,
        route: Route,
        payment_hash: PaymentHash,
        custom_records: Option<CustomRecords>,
    ) -> Result<(), LightningError> {
        let (sender, receiver) = channel();

        if route.paths.len() != 1 {
            return Err(LightningError::SendPaymentError(
                "Route must contain exactly one path for this operation.".to_string(),
            ));
        }

        // Check for payment hash collision, failing the payment if we happen to repeat one.
        match self.in_flight.lock().await.entry(payment_hash) {
            Entry::Occupied(_) => {
                return Err(LightningError::SendPaymentError(
                    "payment hash exists".to_string(),
                ));
            },
            Entry::Vacant(vacant) => vacant.insert(InFlightPayment {
                track_payment_receiver: receiver,
                path: Some(route.paths[0].clone()), // TODO: MPP payments? we check in dispatch_payment
                                                    // should probably only pass a single path to dispatch
            }),
        };

        self.network.lock().await.dispatch_payment(
            self.info.pubkey,
            route,
            custom_records,
            payment_hash,
            sender,
        );

        Ok(())
    }
}

/// Produces the node info for a mocked node, filling in the features that the simulator requires.
fn node_info(pubkey: PublicKey, alias: String) -> NodeInfo {
    // Set any features that the simulator requires here.
    let mut features = NodeFeatures::empty();
    features.set_keysend_optional();

    NodeInfo {
        pubkey,
        alias,
        features,
    }
}

#[async_trait]
impl<T: SimNetwork, C: Clock, P: PathFinder> LightningNode for SimNode<T, C, P> {
    fn get_info(&self) -> &NodeInfo {
        &self.info
    }

    fn get_network(&self) -> Network {
        Network::Regtest
    }

    /// send_payment picks a random preimage for a payment, dispatches it in the network and adds a tracking channel
    /// to our node state to be used for subsequent track_payment calls.
    async fn send_payment(
        &self,
        dest: PublicKey,
        amount_msat: u64,
    ) -> Result<PaymentHash, LightningError> {
        // Create a channel to receive the payment result.
        let (sender, receiver) = channel();
        let preimage = PaymentPreimage(rand::random());
        let payment_hash = preimage.into();

        // Check for payment hash collision, failing the payment if we happen to repeat one.
        let mut in_flight_guard = self.in_flight.lock().await;
        let entry = match in_flight_guard.entry(payment_hash) {
            Entry::Occupied(_) => {
                return Err(LightningError::SendPaymentError(
                    "payment hash exists".to_string(),
                ));
            },
            Entry::Vacant(vacant) => vacant,
        };

        // Use the stored scorer when finding a route
        let route = match self.pathfinder.find_route(
            &self.info.pubkey,
            dest,
            amount_msat,
            &self.pathfinding_graph,
        ) {
            Ok(path) => path,
            // In the case that we can't find a route for the payment, we still report a successful payment *api call*
            // and report RouteNotFound to the tracking channel. This mimics the behavior of real nodes.
            Err(e) => {
                log::trace!("Could not find path for payment: {:?}.", e);

                if let Err(e) = sender.send(Ok(PaymentResult {
                    htlc_count: 0,
                    payment_outcome: PaymentOutcome::RouteNotFound,
                })) {
                    log::error!("Could not send payment result: {:?}.", e);
                }

                entry.insert(InFlightPayment {
                    track_payment_receiver: receiver,
                    path: None, // TODO: how to handle non-MPP support (where would we do
                                // paths in the world where we have them?).
                });

                return Ok(payment_hash);
            },
        };

        if route.paths.len() != 1 {
            return Err(LightningError::SendPaymentError(
                "Route must contain exactly one path for this operation.".to_string(),
            ));
        }

        entry.insert(InFlightPayment {
            track_payment_receiver: receiver,
            path: Some(route.paths[0].clone()), // TODO: how to handle non-MPP support (where would we do
                                                // paths in the world where we have them?).
        });

        // If we did successfully obtain a route, dispatch the payment through the network and then report success.
        self.network.lock().await.dispatch_payment(
            self.info.pubkey,
            route,
            None,
            payment_hash,
            sender,
        );

        Ok(payment_hash)
    }

    /// track_payment blocks until a payment outcome is returned for the payment hash provided, or the shutdown listener
    /// provided is triggered. This call will fail if the hash provided was not obtained from send_payment or passed
    /// into send_to_route first.
    async fn track_payment(
        &self,
        hash: &PaymentHash,
        listener: Listener,
    ) -> Result<PaymentResult, LightningError> {
        match self.in_flight.lock().await.remove(hash) {
            Some(in_flight) => {
                select! {
                    biased;
                    _ = listener => Err(
                        LightningError::TrackPaymentError("shutdown during payment tracking".to_string()),
                    ),

                    // If we get a payment result back, remove from our in flight set of payments and return the result.
                    res = in_flight.track_payment_receiver => {
                        let track_result = res.map_err(|e| LightningError::TrackPaymentError(format!("channel receive err: {}", e)))?;
                        if let Ok(ref payment_result) = track_result {
                            let duration = match self.clock.now().duration_since(UNIX_EPOCH) {
                                Ok(d) => d,
                                Err(e) => {
                                    log::error!("Failed to get duration: {}", e);
                                    return Err(LightningError::SystemTimeConversionError(e));
                                }
                            };
                            match &in_flight.path {
                                Some(path) => {
                                    if payment_result.payment_outcome == PaymentOutcome::Success {
                                        self.scorer.lock().await.payment_path_successful(path, duration);
                                    } else if let PaymentOutcome::IndexFailure(index) = payment_result.payment_outcome {
                                        self.scorer.lock().await.payment_path_failed(path, index as u64, duration);
                                    }
                                },
                                None => {
                                    if payment_result.payment_outcome != PaymentOutcome::RouteNotFound {
                                        return Err(LightningError::TrackPaymentError(
                                            "payment outcome was not RouteNotFound, but no path was provided".to_string(),
                                        ))?;
                                    }
                                }
                            }
                        }
                        track_result
                    },
                }
            },
            None => Err(LightningError::TrackPaymentError(format!(
                "payment hash {} not found",
                hex::encode(hash.0),
            ))),
        }
    }

    async fn get_node_info(&self, node_id: &PublicKey) -> Result<NodeInfo, LightningError> {
        Ok(self.network.lock().await.lookup_node(node_id)?.0)
    }

    async fn list_channels(&self) -> Result<Vec<u64>, LightningError> {
        Ok(self.network.lock().await.lookup_node(&self.info.pubkey)?.1)
    }

    async fn get_graph(&self) -> Result<Graph, LightningError> {
        let nodes = self.network.lock().await.list_nodes();

        let mut nodes_by_pk = HashMap::new();
        for node in nodes {
            nodes_by_pk.insert(node.pubkey, node);
        }

        Ok(Graph { nodes_by_pk })
    }
}

#[async_trait]
pub trait Interceptor: Send + Sync {
    /// Implemented by HTLC interceptors that provide input on the resolution of HTLCs forwarded in the simulation.
    async fn intercept_htlc(
        &self,
        req: InterceptRequest,
    ) -> Result<Result<CustomRecords, ForwardingError>, CriticalError>;

    /// Notifies the interceptor that a previously intercepted htlc has been resolved. Default implementation is a no-op
    /// for cases where the interceptor only cares about interception, not resolution of htlcs.
    /// This should be non-blocking.
    async fn notify_resolution(&self, _res: InterceptResolution) -> Result<(), CriticalError> {
        Ok(())
    }

    /// Returns an identifying name for the interceptor for logging, does not need to be unique.
    fn name(&self) -> String;
}

/// Request sent to an external interceptor to provide feedback on the resolution of the HTLC.
#[derive(Debug, Clone)]
pub struct InterceptRequest {
    /// The node that is forwarding this htlc.
    pub forwarding_node: PublicKey,

    /// The payment hash for the htlc (note that this is not unique).
    pub payment_hash: PaymentHash,

    /// Unique identifier for the htlc that references the channel this htlc was delivered on.
    pub incoming_htlc: HtlcRef,

    /// Custom records provided by the incoming htlc.
    pub incoming_custom_records: CustomRecords,

    /// The short channel id for the outgoing channel that this htlc should be forwarded over. It
    /// will be None if intercepting node is the receiver of the payment.
    pub outgoing_channel_id: Option<ShortChannelID>,

    /// The amount that was forwarded over the incoming_channel_id.
    pub incoming_amount_msat: u64,

    /// The amount that will be forwarded over to outgoing_channel_id.
    pub outgoing_amount_msat: u64,

    /// The expiry height on the incoming htlc.
    pub incoming_expiry_height: u32,

    /// The expiry height on the outgoing htlc.
    pub outgoing_expiry_height: u32,

    // The listener on which the interceptor will receive shutdown signals. Interceptors should
    // listen on this signal and shutdown. If this is not done, it could block the quick resolution
    // of the htlc if another interceptor already signalled a failure.
    pub shutdown_listener: Listener,
}

impl Display for InterceptRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "htlc forwarded by {} over {}:{} -> {} forward amounts {} {}",
            self.forwarding_node,
            self.incoming_htlc.channel_id,
            self.incoming_htlc.index,
            {
                if let Some(c) = self.outgoing_channel_id {
                    format!("-> {c}")
                } else {
                    "receive".to_string()
                }
            },
            self.incoming_amount_msat,
            self.outgoing_amount_msat
        )
    }
}

/// Notification sent to an external interceptor notifying that a htlc that was previously intercepted has been
/// resolved.
pub struct InterceptResolution {
    /// The node that is forwarding this HTLC.
    pub forwarding_node: PublicKey,

    /// Unique identifier for the incoming htlc.
    pub incoming_htlc: HtlcRef,

    /// The short channel id for the outgoing channel that this htlc should be forwarded over, None if notifying the
    /// receiving node.
    pub outgoing_channel_id: Option<ShortChannelID>,

    /// True if the htlc was settled successfully.
    pub success: bool,
}

// Custom records that can be attached by an Interceptor to intercepted HTLCs.
pub type CustomRecords = HashMap<u64, Vec<u8>>;

#[derive(Debug, Clone)]
pub struct HtlcRef {
    pub channel_id: ShortChannelID,
    pub index: u64,
}

/// Handles intercepted HTLCs by calling the interceptors with the provided request.
/// If any interceptor returns a `ForwardingError`, it triggers a shutdown signal to all other
/// interceptors and waits for them to shutdown. The forwarding error is then returned.
/// If any interceptor returns a `CriticalError`, it is immediately returned to trigger a
/// simulation shutdown. TODO: If a critical error happens, we could instead trigger the shutdown
/// for the interceptors and let them finish before returning.
/// While waiting on the interceptors, it listens on the shutdown_listener for any signals from
/// upstream and trigger shutdowns to the interceptors if needed.
/// If all interceptors succeed, their custom records are merged. If two interceptors provide conflicting
/// custom records a `CriticalError::DuplicateCustomRecord` is returned.
async fn handle_intercepted_htlc(
    request: InterceptRequest,
    interceptors: &[Arc<dyn Interceptor>],
    interceptor_trigger: Trigger,
    shutdown_listener: Listener,
) -> Result<Result<CustomRecords, ForwardingError>, CriticalError> {
    if interceptors.is_empty() {
        return Ok(Ok(request.incoming_custom_records));
    }

    let mut attached_custom_records: CustomRecords = HashMap::new();
    let mut intercepts: JoinSet<Result<Result<CustomRecords, ForwardingError>, CriticalError>> =
        JoinSet::new();

    for interceptor in interceptors.iter() {
        let request = request.clone();
        log::trace!(
            "Sending HTLC to intercepor: {} {request}",
            interceptor.name()
        );

        let interceptor_clone = Arc::clone(interceptor);
        intercepts.spawn(async move { interceptor_clone.intercept_htlc(request).await });
    }

    // Read results from the interceptors and check whether any of them returned an instruction to fail
    // the HTLC. If any of the interceptors did return an error, we send a shutdown signal
    // to the other interceptors that may have not returned yet.
    let mut interceptor_failure = None;
    'get_resp: loop {
        tokio::select! {
            res = intercepts.join_next() => {
                let res = match res {
                    Some(res) => res,
                    None => break 'get_resp,
                };

                let res =
                    res.map_err(|join_err| CriticalError::InterceptorError(join_err.to_string()))?;

                match res {
                    Ok(Ok(records)) => {
                        // Interceptor call succeeded and indicated that we should proceed with the forward. Merge
                        // any custom records provided, failing if interceptors provide duplicate values for the
                        // same key.
                        for (k, v) in records {
                            match attached_custom_records.entry(k) {
                                Entry::Occupied(e) => {
                                    let existing_value = e.get();
                                    if *existing_value != v {
                                        return Err(CriticalError::DuplicateCustomRecord(k));
                                    }
                                },
                                Entry::Vacant(e) => {
                                    e.insert(v);
                                },
                            };
                        }
                    },
                    // Interceptor call succeeded but returned a ForwardingError. If this happens,
                    // we should send a trigger to other interceptors to let them know they should
                    // shutdown.
                    Ok(Err(fwd_error)) => {
                        interceptor_failure = Some(fwd_error);
                        interceptor_trigger.trigger();
                    },
                    Err(e) => {
                        return Err(e);
                    },
                }
            }
            _ = shutdown_listener.clone() => {
                // If we get a simulation shutdown signal, we trigger the interceptors to
                // shutdown and wait for them to finish before returning.
                interceptor_trigger.trigger();
                log::debug!("Waiting for interceptors to shutdown.");
                while intercepts.join_next().await.is_some() {}
                break 'get_resp
            }
        }
    }

    if let Some(e) = interceptor_failure {
        return Ok(Err(e));
    }

    Ok(Ok(attached_custom_records))
}

/// Graph is the top level struct that is used to coordinate simulation of lightning nodes.
pub struct SimGraph {
    /// nodes caches the list of nodes in the network with a vector of their channel capacities, only used for quick
    /// lookup.
    nodes: HashMap<PublicKey, (NodeInfo, Vec<u64>)>,

    /// channels maps the scid of a channel to its current simulation state.
    channels: Arc<Mutex<HashMap<ShortChannelID, SimulatedChannel>>>,

    /// track all tasks spawned to process payments in the graph. Note that handling the shutdown of tasks
    /// in this tracker must be done externally.
    tasks: TaskTracker,

    /// Optional set of interceptors that will be called every time a HTLC is added to a simulated channel.
    /// Interceptors can each attach custom records to HTLCs, however it will fail it if there are
    /// conflicting records between them. Note that if one interceptor fails the HTLC, it will
    /// trigger a shutdown signal to other interceptors.
    interceptors: Vec<Arc<dyn Interceptor>>,

    /// Custom records that will be added to the first outgoing HTLC in a payment.
    default_custom_records: CustomRecords,

    /// Shutdown signal that can be used to trigger a shutdown if a critical error occurs. Listener
    /// can be used to listen for shutdown signals coming from upstream.
    shutdown_signal: (Trigger, Listener),
}

impl SimGraph {
    /// Creates a graph on which to simulate payments.
    pub fn new(
        graph_channels: Vec<SimulatedChannel>,
        tasks: TaskTracker,
        interceptors: Vec<Arc<dyn Interceptor>>,
        default_custom_records: CustomRecords,
        shutdown_signal: (Trigger, Listener),
    ) -> Result<Self, SimulationError> {
        let mut nodes: HashMap<PublicKey, (NodeInfo, Vec<u64>)> = HashMap::new();
        let mut channels = HashMap::new();

        for channel in graph_channels.iter() {
            // Assert that the channel is valid and that its short channel ID is unique within the simulation, required
            // because we use scid to identify the channel.
            channel.validate()?;
            match channels.entry(channel.short_channel_id) {
                Entry::Occupied(_) => {
                    return Err(SimulationError::SimulatedNetworkError(format!(
                        "Simulated short channel ID should be unique: {} duplicated",
                        channel.short_channel_id
                    )))
                },
                Entry::Vacant(v) => v.insert(channel.clone()),
            };

            // It's okay to have duplicate pubkeys because one node can have many channels.
            for info in [&channel.node_1.policy, &channel.node_2.policy] {
                match nodes.entry(info.pubkey) {
                    Entry::Occupied(o) => o.into_mut().1.push(channel.capacity_msat),
                    Entry::Vacant(v) => {
                        v.insert((
                            node_info(info.pubkey, info.alias.clone()),
                            vec![channel.capacity_msat],
                        ));
                    },
                }
            }
        }

        Ok(SimGraph {
            nodes,
            channels: Arc::new(Mutex::new(channels)),
            tasks,
            interceptors,
            default_custom_records,
            shutdown_signal,
        })
    }
}

/// Produces a map of node public key to lightning node implementation to be used for simulations.
pub async fn ln_node_from_graph<C: Clock, P>(
    graph: Arc<Mutex<SimGraph>>,
    routing_graph: Arc<LdkNetworkGraph>,
    clock: Arc<C>,
    pathfinder: P,
) -> Result<HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>, LightningError> 
where
    P: PathFinder + 'static,
{
    let mut nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> = HashMap::new();

    for pk in graph.lock().await.nodes.keys() {
        nodes.insert(
            *node.0,
            Arc::new(Mutex::new(SimNode::new(
                node.1 .0.clone(),
                graph.clone(),
                routing_graph.clone(),
                clock.clone(),
                pathfinder.clone(),
            ))),
        );
    }

    Ok(nodes)
}

/// Populates a network graph based on the set of simulated channels provided. This function *only* applies channel
/// announcements, which has the effect of adding the nodes in each channel to the graph, because LDK does not export
/// all of the fields required to apply node announcements. This means that we will not have node-level information
/// (such as features) available in the routing graph.
pub fn populate_network_graph<C: Clock>(
    channels: Vec<SimulatedChannel>,
    clock: Arc<C>,
) -> Result<LdkNetworkGraph, LdkError> {
    let graph = NetworkGraph::new(Network::Regtest, Arc::new(WrappedLog {}));

    let chain_hash = ChainHash::using_genesis_block(Network::Regtest);

    for channel in channels {
        let announcement = UnsignedChannelAnnouncement {
            // For our purposes we don't currently need any channel level features.
            features: ChannelFeatures::empty(),
            chain_hash,
            short_channel_id: channel.short_channel_id.into(),
            node_id_1: NodeId::from_pubkey(&channel.node_1.policy.pubkey),
            node_id_2: NodeId::from_pubkey(&channel.node_2.policy.pubkey),
            // Note: we don't need bitcoin keys for our purposes, so we just copy them *but* remember that we do use
            // this for our fake utxo validation so they do matter for producing the script that we mock validate.
            bitcoin_key_1: NodeId::from_pubkey(&channel.node_1.policy.pubkey),
            bitcoin_key_2: NodeId::from_pubkey(&channel.node_2.policy.pubkey),
            // Internal field used by LDK, we don't need it.
            excess_data: Vec::new(),
        };

        let utxo_validator = UtxoValidator {
            amount_sat: channel.capacity_msat / 1000,
            script: make_funding_redeemscript(
                &channel.node_1.policy.pubkey,
                &channel.node_2.policy.pubkey,
            )
            .to_v0_p2wsh(),
        };

        graph.update_channel_from_unsigned_announcement(&announcement, &Some(&utxo_validator))?;

        // LDK only allows channel announcements up to 24h in the future. Use a fixed timestamp so that even if we've
        // sped up our clock dramatically, we won't hit that limit.
        let now = clock.now().duration_since(UNIX_EPOCH).unwrap().as_secs() as u32;
        for (i, node) in [channel.node_1, channel.node_2].iter().enumerate() {
            let update = UnsignedChannelUpdate {
                chain_hash,
                short_channel_id: channel.short_channel_id.into(),
                timestamp: now,
                // The least significant bit of the channel flag field represents the direction that the channel update
                // applies to. This value is interpreted as node_1 if it is zero, and node_2 otherwise.
                flags: i as u8,
                cltv_expiry_delta: node.policy.cltv_expiry_delta as u16,
                htlc_minimum_msat: node.policy.min_htlc_size_msat,
                htlc_maximum_msat: node.policy.max_htlc_size_msat,
                fee_base_msat: node.policy.base_fee as u32,
                fee_proportional_millionths: node.policy.fee_rate_prop as u32,
                excess_data: Vec::new(),
            };
            graph.update_channel_unsigned(&update)?;
        }
    }

    Ok(graph)
}

impl SimNetwork for SimGraph {
    /// dispatch_payment asynchronously propagates a payment through the simulated network, returning a tracking
    /// channel that can be used to obtain the result of the payment. At present, MPP payments are not supported.
    /// In future, we'll allow multiple paths for a single payment, so we allow the trait to accept a route with
    /// multiple paths to avoid future refactoring. If custom records are not provided,
    /// [`SimGraph::default_custom_records`] will be sent with the payment.
    fn dispatch_payment(
        &mut self,
        source: PublicKey,
        route: Route,
        custom_records: Option<CustomRecords>,
        payment_hash: PaymentHash,
        sender: Sender<Result<PaymentResult, LightningError>>,
    ) {
        // Expect only one path (right now), with the intention to support multiple in future.
        if route.paths.len() != 1 {
            log::error!("Route must contain exactly one path for this operation.");
            return;
        }

        let path = match route.paths.first() {
            Some(p) => p,
            None => {
                log::warn!("Find route did not return expected number of paths.");

                if let Err(e) = sender.send(Ok(PaymentResult {
                    htlc_count: 0,
                    payment_outcome: PaymentOutcome::RouteNotFound,
                })) {
                    log::error!("Could not send payment result: {:?}.", e);
                }

                return;
            },
        };

        self.tasks.spawn(propagate_payment(PropagatePaymentRequest {
            nodes: Arc::clone(&self.channels),
            source,
            route: path.clone(),
            payment_hash,
            sender,
            interceptors: self.interceptors.clone(),
            custom_records: custom_records.unwrap_or(self.default_custom_records.clone()),
            shutdown_signal: self.shutdown_signal.clone(),
        }));
    }

    /// lookup_node fetches a node's information and channel capacities.
    fn lookup_node(&self, node: &PublicKey) -> Result<(NodeInfo, Vec<u64>), LightningError> {
        match self.nodes.get(node) {
            Some(node) => Ok(node.clone()),
            None => Err(LightningError::GetNodeInfoError(
                "Node not found".to_string(),
            )),
        }
    }

    fn list_nodes(&self) -> Vec<NodeInfo> {
        let mut nodes = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            nodes.push(node.1 .0.clone());
        }
        nodes
    }
}

/// Adds htlcs to the simulation state along the path provided. If it encounters a critical error,
/// it will be returned in the outer Result to signal that the simulation should shut down. If a
/// forwarding error happens, it will return the index in the path from which to fail back htlcs
/// (if any) and the ForwardingError in the inner Result. We don't return the index during a
/// critical error since we expect to trigger a simulation shutdown after it.
///
/// For each hop in the route, we check both the addition of the HTLC and whether we can forward it. Take an example
/// route A --> B --> C, we will add this in two hops: A --> B then B -->C. For each hop, using A --> B as an example:
/// * Check whether A can add the outgoing HTLC (checks liquidity and in-flight restrictions).
///   * If no, fail the HTLC.
///   * If yes, add outgoing HTLC to A's channel.
/// * Check whether B will accept the forward.
///   * If no, fail the HTLC.
///   * If yes, continue to the next hop.
///
/// If successfully added to A --> B, this check will be repeated for B --> C.
///
/// Note that we don't have any special handling for the receiving node, once we've successfully added a outgoing HTLC
/// for the outgoing channel that is connected to the receiving node we'll return. To add invoice-related handling,
/// we'd need to include some logic that then decides whether to settle/fail the HTLC at the last hop here.
async fn add_htlcs(
    nodes: Arc<Mutex<HashMap<ShortChannelID, SimulatedChannel>>>,
    source: PublicKey,
    route: Path,
    payment_hash: PaymentHash,
    interceptors: Vec<Arc<dyn Interceptor>>,
    custom_records: CustomRecords,
    shutdown_listener: Listener,
) -> Result<Result<(), (Option<usize>, ForwardingError)>, CriticalError> {
    let mut outgoing_node = source;
    let mut outgoing_amount = route.fee_msat() + route.final_value_msat();
    let mut outgoing_cltv = route.hops.iter().map(|hop| hop.cltv_expiry_delta).sum();

    let mut incoming_custom_records = custom_records;

    // Tracks the hop index that we need to remove htlcs from on payment completion (both success and failure).
    // Given a payment from A to C, over the route A -- B -- C, this index has the following meanings:
    // - None: A could not add the outgoing HTLC to B, no action for payment failure.
    // - Some(0): A -- B added the HTLC but B could not forward the HTLC to C, so it only needs removing on A -- B.
    // - Some(1): A -- B and B -- C added the HTLC, so it should be removed from the full route.
    let mut fail_idx = None;
    let last_hop = route.hops.len() - 1;
    for (i, hop) in route.hops.iter().enumerate() {
        // Lock the node that we want to add the HTLC to next. We choose to lock one hop at a time (rather than for
        // the whole route) so that we can mimic the behavior of payments in the real network where the HTLCs in a
        // route don't all get to lock in in a row (they have interactions with other payments).
        let mut node_lock = nodes.lock().await;
        let scid = ShortChannelID::from(hop.short_channel_id);

        let (incoming_htlc, next_scid) = {
            let channel = node_lock
                .get_mut(&scid)
                .ok_or(CriticalError::ChannelNotFound(scid))?;

            let htlc_index = match channel.add_htlc(
                &outgoing_node,
                payment_hash,
                Htlc {
                    amount_msat: outgoing_amount,
                    cltv_expiry: outgoing_cltv,
                },
            )? {
                Ok(idx) => idx,
                // If we couldn't add to this HTLC, we only need to fail back from the preceding hop, so we don't
                // have to progress our fail_idx.
                Err(e) => return Ok(Err((fail_idx, e))),
            };

            // If the HTLC was successfully added, then we'll need to remove the HTLC from this channel if we fail,
            // so we progress our failure index to include this node.
            fail_idx = Some(i);

            // Once we've added the HTLC on this hop's channel, we want to check whether it has sufficient fee
            // and CLTV delta per the _next_ channel's policy (because fees and CLTV delta in LN are charged on
            // the outgoing link). We check the policy belonging to the node that we just forwarded to, which
            // represents the fee in that direction.
            //
            // TODO: add invoice-related checks (including final CTLV) if we support non-keysend payments.
            let mut next_scid = None;
            if i != last_hop {
                next_scid = Some(ShortChannelID::from(route.hops[i + 1].short_channel_id));

                if let Some(channel) = node_lock.get(&next_scid.unwrap()) {
                    if let Err(e) = channel.check_htlc_forward(
                        &hop.pubkey,
                        hop.cltv_expiry_delta,
                        outgoing_amount - hop.fee_msat,
                        hop.fee_msat,
                    )? {
                        // If we haven't met forwarding conditions for the next channel's policy, then we fail at
                        // the current index, because we've already added the HTLC as outgoing.
                        return Ok(Err((fail_idx, e)));
                    }
                }
            }
            let incoming_htlc = HtlcRef {
                channel_id: scid,
                index: htlc_index,
            };
            (incoming_htlc, next_scid)
        };

        // Before we continue on to the next hop, we'll call any interceptors registered to get external input on the
        // forwarding decision for this HTLC.
        //
        // We drop our node lock so that we can await our interceptors (which may choose to hold the HTLC for a long
        // time) without holding our entire graph hostage.
        drop(node_lock);

        // Trigger to be used only for interceptors.
        let interceptor_signal = triggered::trigger();
        let request = InterceptRequest {
            forwarding_node: hop.pubkey,
            payment_hash,
            incoming_htlc: incoming_htlc.clone(),
            incoming_custom_records,
            outgoing_channel_id: next_scid,
            incoming_amount_msat: outgoing_amount,
            outgoing_amount_msat: outgoing_amount - hop.fee_msat,
            incoming_expiry_height: outgoing_cltv,
            outgoing_expiry_height: outgoing_cltv - hop.cltv_expiry_delta,
            shutdown_listener: interceptor_signal.1,
        };

        let intercepted_res = handle_intercepted_htlc(
            request,
            &interceptors,
            interceptor_signal.0,
            shutdown_listener.clone(),
        )
        .await?;

        // Collect any custom records (if any) set by the interceptor(s) for the outgoing link.
        let attached_custom_records = match intercepted_res {
            Ok(records) => records,
            Err(fwd_err) => return Ok(Err((fail_idx, fwd_err))),
        };

        // Once we've taken the "hop" to the destination pubkey, it becomes the source of the next outgoing htlc and
        // any outgoing custom records set by the interceptor become the incoming custom records for the next hop.
        outgoing_node = hop.pubkey;
        outgoing_amount -= hop.fee_msat;
        outgoing_cltv -= hop.cltv_expiry_delta;
        incoming_custom_records = attached_custom_records;
    }

    Ok(Ok(()))
}

/// Removes htlcs from the simulation state from the index in the path provided (backwards).
///
/// Taking the example of a payment over A --> B --> C --> D where the payment was rejected by C because it did not
/// have enough liquidity to forward it, we will expect a failure index of 1 because the HTLC was successfully added
/// to A and B's outgoing channels, but not C.
///
/// This function will remove the HTLC one hop at a time, working backwards from the failure index, so in this
/// case B --> C and then B --> A. We lookup the HTLC on the incoming node because it will have tracked it in its
/// outgoing in-flight HTLCs.
async fn remove_htlcs(
    nodes: Arc<Mutex<HashMap<ShortChannelID, SimulatedChannel>>>,
    resolution_idx: usize,
    source: PublicKey,
    route: Path,
    payment_hash: PaymentHash,
    success: bool,
    interceptors: Vec<Arc<dyn Interceptor>>,
) -> Result<(), CriticalError> {
    let mut outgoing_channel_id = None;
    for (i, hop) in route.hops[0..=resolution_idx].iter().enumerate().rev() {
        // When we add HTLCs, we do so on the state of the node that sent the htlc along the channel so we need to
        // look up our incoming node so that we can remove it when we go backwards. For the first htlc, this is just
        // the sending node, otherwise it's the hop before.
        let incoming_node = if i == 0 {
            source
        } else {
            route.hops[i - 1].pubkey
        };

        // As with when we add HTLCs, we remove them one hop at a time (rather than locking for the whole route) to
        // mimic the behavior of payments in a real network.
        let mut node_lock = nodes.lock().await;
        let incoming_scid = ShortChannelID::from(hop.short_channel_id);
        let (_removed_htlc, index) = match node_lock.get_mut(&incoming_scid) {
            Some(channel) => channel.remove_htlc(&incoming_node, &payment_hash, success)?,
            None => {
                return Err(CriticalError::ChannelNotFound(ShortChannelID::from(
                    hop.short_channel_id,
                )))
            },
        };

        // We drop our node lock so that we can notify interceptors without blocking other payments processing.
        drop(node_lock);

        for interceptor in interceptors.iter() {
            log::trace!("Sending resolution to interceptor: {}", interceptor.name());

            interceptor
                .notify_resolution(InterceptResolution {
                    forwarding_node: hop.pubkey,
                    incoming_htlc: HtlcRef {
                        channel_id: incoming_scid,
                        index,
                    },
                    outgoing_channel_id,
                    success,
                })
                .await?;
        }

        outgoing_channel_id = Some(incoming_scid);
    }

    Ok(())
}

struct PropagatePaymentRequest {
    nodes: Arc<Mutex<HashMap<ShortChannelID, SimulatedChannel>>>,
    source: PublicKey,
    route: Path,
    payment_hash: PaymentHash,
    sender: Sender<Result<PaymentResult, LightningError>>,
    interceptors: Vec<Arc<dyn Interceptor>>,
    custom_records: CustomRecords,
    shutdown_signal: (Trigger, Listener),
}

/// Finds a payment path from the source to destination nodes provided, and propagates the appropriate htlcs through
/// the simulated network, notifying the sender channel provided of the payment outcome. If a critical error occurs,
/// ie a breakdown of our state machine, it will still notify the payment outcome and will use the shutdown trigger
/// to signal that we should exit.
async fn propagate_payment(request: PropagatePaymentRequest) {
    let notify_result = match add_htlcs(
        request.nodes.clone(),
        request.source,
        request.route.clone(),
        request.payment_hash,
        request.interceptors.clone(),
        request.custom_records,
        request.shutdown_signal.1,
    )
    .await
    {
        Ok(Ok(_)) => {
            // If we successfully added the htlc, go ahead and remove all the htlcs in the route with successful resolution.
            if let Err(e) = remove_htlcs(
                request.nodes,
                request.route.hops.len() - 1,
                request.source,
                request.route,
                request.payment_hash,
                true,
                request.interceptors,
            )
            .await
            {
                request.shutdown_signal.0.trigger();
                log::error!("Could not remove htlcs from channel: {e}.");
            }
            PaymentResult {
                htlc_count: 1,
                payment_outcome: PaymentOutcome::Success,
            }
        },
        Ok(Err((fail_idx, fwd_err))) => {
            // If we partially added HTLCs along the route, we need to fail them back to the source to clean up our partial
            // state. It's possible that we failed with the very first add, and then we don't need to clean anything up.
            if let Some(resolution_idx) = fail_idx {
                if remove_htlcs(
                    request.nodes,
                    resolution_idx,
                    request.source,
                    request.route,
                    request.payment_hash,
                    false,
                    request.interceptors,
                )
                .await
                .is_err()
                {
                    request.shutdown_signal.0.trigger();
                }
            }

            log::debug!(
                "Forwarding failure for simulated payment {}: {fwd_err}",
                hex::encode(request.payment_hash.0)
            );
            PaymentResult {
                htlc_count: 0,
                payment_outcome: PaymentOutcome::IndexFailure(fail_idx.unwrap_or(0)),
            }
        },
        Err(critical_err) => {
            request.shutdown_signal.0.trigger();
            log::debug!(
                "Critical error in simulated payment {}: {critical_err}",
                hex::encode(request.payment_hash.0)
            );
            PaymentResult {
                htlc_count: 0,
                payment_outcome: PaymentOutcome::Unknown,
            }
        },
    };

    if let Err(e) = request.sender.send(Ok(notify_result)) {
        log::error!("Could not notify payment result: {:?}.", e);
    }
}

/// WrappedLog implements LDK's logging trait so that we can provide pathfinding with a logger that uses our existing
/// logger.
pub struct WrappedLog {}

impl Logger for WrappedLog {
    fn log(&self, record: Record) {
        match record.level {
            Level::Gossip => log::trace!("{}", record.args),
            Level::Trace => log::trace!("{}", record.args),
            Level::Debug => log::debug!("{}", record.args),
            // LDK has quite noisy info logging for pathfinding, so we downgrade their info logging to our debug level.
            Level::Info => log::debug!("{}", record.args),
            Level::Warn => log::warn!("{}", record.args),
            Level::Error => log::error!("{}", record.args),
        }
    }
}

/// UtxoValidator is a mock utxo validator that just returns a fake output with the desired capacity for a channel.
struct UtxoValidator {
    amount_sat: u64,
    script: ScriptBuf,
}

impl UtxoLookup for UtxoValidator {
    fn get_utxo(&self, _genesis_hash: &ChainHash, _short_channel_id: u64) -> UtxoResult {
        UtxoResult::Sync(Ok(TxOut {
            value: self.amount_sat,
            script_pubkey: self.script.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::test_utils::get_random_keypair;
    use lightning::routing::router::build_route_from_hops;
    use lightning::routing::router::Route;
    use mockall::mock;
    use ntest::assert_true;
    use std::time::Duration;
    use tokio::time::{self, timeout};

    /// Creates a test channel policy with its maximum HTLC size set to half of the in flight limit of the channel.
    /// The minimum HTLC size is hardcoded to 2 so that we can fall beneath this value with a 1 msat htlc.
    fn create_test_policy(max_in_flight_msat: u64) -> ChannelPolicy {
        let (_, pk) = get_random_keypair();
        ChannelPolicy {
            pubkey: pk,
            alias: String::default(),
            max_htlc_count: 10,
            max_in_flight_msat,
            min_htlc_size_msat: 2,
            max_htlc_size_msat: max_in_flight_msat / 2,
            cltv_expiry_delta: 10,
            base_fee: 1000,
            fee_rate_prop: 5000,
        }
    }

    /// Creates a set of n simulated channels connected in a chain of channels, where the short channel ID of each
    /// channel is its index in the chain of channels and all capacity is on the side of the node that opened the
    /// channel.
    ///
    /// For example if n = 3 it will produce: node_1 -- node_2 -- node_3 -- node_4, connected by channels.
    fn create_simulated_channels(n: u64, capacity_msat: u64) -> Vec<SimulatedChannel> {
        let mut channels: Vec<SimulatedChannel> = vec![];
        let (_, first_node) = get_random_keypair();

        // Create channels in a ring so that we'll get long payment paths.
        let mut node_1 = first_node;
        for i in 0..n {
            // Generate a new random node pubkey.
            let (_, node_2) = get_random_keypair();

            let node_1_to_2 = ChannelPolicy {
                pubkey: node_1,
                alias: String::default(),
                max_htlc_count: 483,
                max_in_flight_msat: capacity_msat / 2,
                min_htlc_size_msat: 1,
                max_htlc_size_msat: capacity_msat / 2,
                cltv_expiry_delta: 40,
                base_fee: 1000 * i,
                fee_rate_prop: 1500 * i,
            };

            let node_2_to_1 = ChannelPolicy {
                pubkey: node_2,
                alias: String::default(),
                max_htlc_count: 483,
                max_in_flight_msat: capacity_msat / 2,
                min_htlc_size_msat: 1,
                max_htlc_size_msat: capacity_msat / 2,
                cltv_expiry_delta: 40 + 10 * i as u32,
                base_fee: 2000 * i,
                fee_rate_prop: i,
            };

            channels.push(SimulatedChannel {
                capacity_msat,
                // Unique channel ID per link.
                short_channel_id: ShortChannelID::from(i),
                node_1: ChannelState::new(node_1_to_2, capacity_msat),
                node_2: ChannelState::new(node_2_to_1, 0),
            });

            // Progress source ID to create a chain of nodes.
            node_1 = node_2;
        }

        channels
    }

    macro_rules! assert_channel_balances {
        ($channel_state:expr, $local_balance:expr, $in_flight_len:expr, $in_flight_total:expr) => {
            assert_eq!($channel_state.local_balance_msat, $local_balance);
            assert_eq!($channel_state.in_flight.len(), $in_flight_len);
            assert_eq!($channel_state.in_flight_total(), $in_flight_total);
        };
    }

    /// Tests state updates related to adding and removing HTLCs to a channel.
    #[test]
    fn test_channel_state_transitions() {
        let local_balance = 100_000_000;
        let mut channel_state =
            ChannelState::new(create_test_policy(local_balance / 2), local_balance);

        // Basic sanity check that we Initialize the channel correctly.
        assert_channel_balances!(channel_state, local_balance, 0, 0);

        // Add a few HTLCs to our internal state and assert that balances are as expected. We'll test
        // `check_outgoing_addition` in more detail in another test, so we just assert that we can add the htlc in
        // this test.
        let hash_1 = PaymentHash([1; 32]);
        let htlc_1 = Htlc {
            amount_msat: 1000,
            cltv_expiry: 40,
        };

        assert!(channel_state.add_outgoing_htlc(hash_1, htlc_1).is_ok());
        assert_channel_balances!(
            channel_state,
            local_balance - htlc_1.amount_msat,
            1,
            htlc_1.amount_msat
        );

        // Try to add a htlc with the same payment hash and assert that we fail because we enforce one htlc per hash
        // at present.
        assert!(matches!(
            channel_state.add_outgoing_htlc(hash_1, htlc_1),
            Err(CriticalError::PaymentHashExists(_))
        ));

        // Add a second, distinct htlc to our in-flight state.
        let hash_2 = PaymentHash([2; 32]);
        let htlc_2 = Htlc {
            amount_msat: 1000,
            cltv_expiry: 40,
        };

        assert!(channel_state.add_outgoing_htlc(hash_2, htlc_2).is_ok());
        assert_channel_balances!(
            channel_state,
            local_balance - htlc_1.amount_msat - htlc_2.amount_msat,
            2,
            htlc_1.amount_msat + htlc_2.amount_msat
        );

        // Remove our second htlc with a failure so that our in-flight drops and we return the balance.
        assert!(channel_state.remove_outgoing_htlc(&hash_2).is_ok());
        channel_state.settle_outgoing_htlc(htlc_2.amount_msat, false);
        assert_channel_balances!(
            channel_state,
            local_balance - htlc_1.amount_msat,
            1,
            htlc_1.amount_msat
        );

        // Try to remove the same htlc and assert that we fail because the htlc can't be found.
        assert!(matches!(
            channel_state.remove_outgoing_htlc(&hash_2),
            Err(CriticalError::PaymentHashNotFound(_))
        ));

        // Finally, remove our original htlc with success and assert that our local balance is accordingly updated.
        assert!(channel_state.remove_outgoing_htlc(&hash_1).is_ok());
        channel_state.settle_outgoing_htlc(htlc_1.amount_msat, true);
        assert_channel_balances!(channel_state, local_balance - htlc_1.amount_msat, 0, 0);
    }

    /// Tests policy checks applied when forwarding a htlc over a channel.
    #[test]
    fn test_htlc_forward() {
        let local_balance = 140_000;
        let channel_state = ChannelState::new(create_test_policy(local_balance / 2), local_balance);

        // CLTV delta insufficient (one less than required).
        assert!(matches!(
            channel_state.check_htlc_forward(channel_state.policy.cltv_expiry_delta - 1, 0, 0),
            Ok(Err(ForwardingError::InsufficientCltvDelta(_, _)))
        ));

        // Test insufficient fee.
        let htlc_amount = 1000;
        let htlc_fee = channel_state.policy.base_fee
            + (channel_state.policy.fee_rate_prop * htlc_amount) / 1e6 as u64;

        assert!(matches!(
            channel_state.check_htlc_forward(
                channel_state.policy.cltv_expiry_delta,
                htlc_amount,
                htlc_fee - 1
            ),
            Ok(Err(ForwardingError::InsufficientFee(_, _, _, _)))
        ));

        // Test exact and over-estimation of required policy.
        assert!(channel_state
            .check_htlc_forward(
                channel_state.policy.cltv_expiry_delta,
                htlc_amount,
                htlc_fee,
            )
            .is_ok());

        assert!(channel_state
            .check_htlc_forward(
                channel_state.policy.cltv_expiry_delta * 2,
                htlc_amount,
                htlc_fee * 3
            )
            .is_ok());
    }

    /// Test addition of outgoing htlc to local state.
    #[test]
    fn test_check_outgoing_addition() {
        // Create test channel with low local liquidity so that we run into failures.
        let local_balance = 100_000;
        let mut channel_state =
            ChannelState::new(create_test_policy(local_balance / 2), local_balance);

        let mut htlc = Htlc {
            amount_msat: channel_state.policy.max_htlc_size_msat + 1,
            cltv_expiry: channel_state.policy.cltv_expiry_delta,
        };
        // HTLC maximum size exceeded.
        assert!(matches!(
            channel_state.check_outgoing_addition(&htlc),
            Err(ForwardingError::MoreThanMaximum(_, _))
        ));

        // Beneath HTLC minimum size.
        htlc.amount_msat = channel_state.policy.min_htlc_size_msat - 1;
        assert!(matches!(
            channel_state.check_outgoing_addition(&htlc),
            Err(ForwardingError::LessThanMinimum(_, _))
        ));

        // Add two large htlcs so that we will start to run into our in-flight total amount limit.
        let hash_1 = PaymentHash([1; 32]);
        let htlc_1 = Htlc {
            amount_msat: channel_state.policy.max_in_flight_msat / 2,
            cltv_expiry: channel_state.policy.cltv_expiry_delta,
        };

        assert!(channel_state.check_outgoing_addition(&htlc_1).is_ok());
        assert!(channel_state.add_outgoing_htlc(hash_1, htlc_1).is_ok());

        let hash_2 = PaymentHash([2; 32]);
        let htlc_2 = Htlc {
            amount_msat: channel_state.policy.max_in_flight_msat / 2,
            cltv_expiry: channel_state.policy.cltv_expiry_delta,
        };

        assert!(channel_state.check_outgoing_addition(&htlc_2).is_ok());
        assert!(channel_state.add_outgoing_htlc(hash_2, htlc_2).is_ok());

        // Now, assert that we can't add even our smallest htlc size, because we're hit our in-flight amount limit.
        htlc.amount_msat = channel_state.policy.min_htlc_size_msat;
        assert!(matches!(
            channel_state.check_outgoing_addition(&htlc),
            Err(ForwardingError::ExceedsInFlightTotal(_, _))
        ));

        // Resolve both of the htlcs successfully so that the local liquidity is no longer available.
        assert!(channel_state.remove_outgoing_htlc(&hash_1).is_ok());
        channel_state.settle_outgoing_htlc(htlc_1.amount_msat, true);

        assert!(channel_state.remove_outgoing_htlc(&hash_2).is_ok());
        channel_state.settle_outgoing_htlc(htlc_2.amount_msat, true);

        // Now we're going to add many htlcs so that we hit our in-flight count limit (unique payment hash per htlc).
        for i in 0..channel_state.policy.max_htlc_count {
            let hash = PaymentHash([i.try_into().unwrap(); 32]);
            assert!(channel_state.check_outgoing_addition(&htlc).is_ok());
            assert!(channel_state.add_outgoing_htlc(hash, htlc).is_ok());
        }

        // Try to add one more htlc and we should be rejected.
        let htlc_3 = Htlc {
            amount_msat: channel_state.policy.min_htlc_size_msat,
            cltv_expiry: channel_state.policy.cltv_expiry_delta,
        };

        assert!(matches!(
            channel_state.check_outgoing_addition(&htlc_3),
            Err(ForwardingError::ExceedsInFlightCount(_, _))
        ));

        // Resolve all in-flight htlcs.
        for i in 0..channel_state.policy.max_htlc_count {
            let hash = PaymentHash([i.try_into().unwrap(); 32]);
            assert!(channel_state.remove_outgoing_htlc(&hash).is_ok());
            channel_state.settle_outgoing_htlc(htlc.amount_msat, true)
        }

        // Add and settle another htlc to move more liquidity away from our local balance.
        let hash_4 = PaymentHash([1; 32]);
        let htlc_4 = Htlc {
            amount_msat: channel_state.policy.max_htlc_size_msat,
            cltv_expiry: channel_state.policy.cltv_expiry_delta,
        };
        assert!(channel_state.check_outgoing_addition(&htlc_4).is_ok());
        assert!(channel_state.add_outgoing_htlc(hash_4, htlc_4).is_ok());
        assert!(channel_state.remove_outgoing_htlc(&hash_4).is_ok());
        channel_state.settle_outgoing_htlc(htlc_4.amount_msat, true);

        // Finally, assert that we don't have enough balance to forward our largest possible htlc (because of all the
        // htlcs that we've settled) and assert that we fail to a large htlc. The balance assertion here is just a
        // sanity check for the test, which will fail if we change the amounts settled/failed in the test.
        assert!(channel_state.local_balance_msat < channel_state.policy.max_htlc_size_msat);
        assert!(matches!(
            channel_state.check_outgoing_addition(&htlc_4),
            Err(ForwardingError::InsufficientBalance(_, _))
        ));
    }

    /// Tests basic functionality of a `SimulatedChannel` but does no endeavor to test the underlying
    /// `ChannelState`, as this is covered elsewhere in our tests.
    #[test]
    fn test_simulated_channel() {
        // Create a test channel with all balance available to node 1 as local liquidity, and none for node_2 to begin
        // with.
        let capacity_msat = 500_000_000;
        let node_1 = ChannelState::new(create_test_policy(capacity_msat / 2), capacity_msat);
        let node_2 = ChannelState::new(create_test_policy(capacity_msat / 2), 0);

        let mut simulated_channel = SimulatedChannel {
            capacity_msat,
            short_channel_id: ShortChannelID::from(123),
            node_1: node_1.clone(),
            node_2: node_2.clone(),
        };

        // Assert that we're not able to send a htlc over node_2 -> node_1 (no liquidity).
        let hash_1 = PaymentHash([1; 32]);
        let htlc_1 = Htlc {
            amount_msat: node_2.policy.min_htlc_size_msat,
            cltv_expiry: node_1.policy.cltv_expiry_delta,
        };

        assert!(matches!(
            simulated_channel.add_htlc(&node_2.policy.pubkey, hash_1, htlc_1),
            Ok(Err(ForwardingError::InsufficientBalance(_, _)))
        ));

        // Assert that we can send a htlc over node_1 -> node_2.
        let hash_2 = PaymentHash([1; 32]);
        let htlc_2 = Htlc {
            amount_msat: node_1.policy.max_htlc_size_msat,
            cltv_expiry: node_2.policy.cltv_expiry_delta,
        };
        assert!(simulated_channel
            .add_htlc(&node_1.policy.pubkey, hash_2, htlc_2)
            .is_ok());

        // Settle the htlc and then assert that we can send from node_2 -> node_2 because the balance has been shifted
        // across channels.
        assert!(simulated_channel
            .remove_htlc(&node_1.policy.pubkey, &hash_2, true)
            .is_ok());

        assert!(simulated_channel
            .add_htlc(&node_2.policy.pubkey, hash_2, htlc_2)
            .is_ok());

        // Finally, try to add/remove htlcs for a pubkey that is not participating in the channel and assert that we
        // fail.
        let (_, pk) = get_random_keypair();
        assert!(matches!(
            simulated_channel.add_htlc(&pk, hash_2, htlc_2),
            Err(CriticalError::NodeNotFound(_))
        ));

        assert!(matches!(
            simulated_channel.remove_htlc(&pk, &hash_2, true),
            Err(CriticalError::NodeNotFound(_))
        ));
    }

    mock! {
        Network{}

        impl SimNetwork for Network{
            fn dispatch_payment(
                &mut self,
                source: PublicKey,
                route: Route,
                custom_records: Option<CustomRecords>,
                payment_hash: PaymentHash,
                sender: Sender<Result<PaymentResult, LightningError>>,
            );

            fn lookup_node(&self, node: &PublicKey) -> Result<(NodeInfo, Vec<u64>), LightningError>;
            fn list_nodes(&self) -> Vec<NodeInfo>;
        }
    }

    /// Tests the functionality of a `SimNode`, mocking out the `SimNetwork` that is responsible for payment
    /// propagation to isolate testing to just the implementation of `LightningNode`.
    #[tokio::test]
    async fn test_simulated_node() {
        // Mock out our network and create a routing graph with 5 hops.
        let mock = MockNetwork::new();
        let sim_network = Arc::new(Mutex::new(mock));
        let channels = create_simulated_channels(5, 300000000);
        let graph = populate_network_graph(channels.clone(), Arc::new(SystemClock {})).unwrap();

        // Create a simulated node for the first channel in our network.
        let pk = channels[0].node_1.policy.pubkey;
        let node = SimNode::new(
            node_info(pk, String::default()),
            sim_network.clone(),
            Arc::new(graph),
            Arc::new(SystemClock {}),
            DefaultPathFinder
        )
        .unwrap();

        // Prime mock to return node info from lookup and assert that we get the pubkey we're expecting.
        let lookup_pk = channels[3].node_1.policy.pubkey;
        sim_network
            .lock()
            .await
            .expect_lookup_node()
            .returning(move |_| Ok((node_info(lookup_pk, String::default()), vec![1, 2, 3])));

        // Assert that we get three channels from the mock.
        let node_info = node.get_node_info(&lookup_pk).await.unwrap();
        assert_eq!(lookup_pk, node_info.pubkey);
        assert_eq!(node.list_channels().await.unwrap().len(), 3);

        // Next, we're going to test handling of in-flight payments. To do this, we'll mock out calls to our dispatch
        // function to send different results depending on the destination.
        let dest_1 = channels[2].node_1.policy.pubkey;
        let dest_2 = channels[4].node_1.policy.pubkey;

        sim_network
            .lock()
            .await
            .expect_dispatch_payment()
            .returning(
                move |_,
                      route: Route,
                      _: Option<CustomRecords>,
                      _,
                      sender: Sender<Result<PaymentResult, LightningError>>| {
                    // If we've reached dispatch, we must have at least one path, grab the last hop to match the
                    // receiver.
                    let receiver = route.paths[0].hops.last().unwrap().pubkey;
                    let result = if receiver == dest_1 {
                        PaymentResult {
                            htlc_count: 2,
                            payment_outcome: PaymentOutcome::Success,
                        }
                    } else if receiver == dest_2 {
                        PaymentResult {
                            htlc_count: 0,
                            payment_outcome: PaymentOutcome::InsufficientBalance,
                        }
                    } else {
                        panic!("unknown mocked receiver");
                    };

                    sender.send(Ok(result)).unwrap();
                },
            );

        // Dispatch payments to different destinations and assert that our track payment results are as expected.
        let hash_1 = node.send_payment(dest_1, 10_000).await.unwrap();
        let hash_2 = node.send_payment(dest_2, 15_000).await.unwrap();

        let (_, shutdown_listener) = triggered::trigger();

        let result_1 = node
            .track_payment(&hash_1, shutdown_listener.clone())
            .await
            .unwrap();
        assert!(matches!(result_1.payment_outcome, PaymentOutcome::Success));

        let result_2 = node
            .track_payment(&hash_2, shutdown_listener.clone())
            .await
            .unwrap();
        assert!(matches!(
            result_2.payment_outcome,
            PaymentOutcome::InsufficientBalance
        ));
    }

    #[tokio::test]
    async fn test_payment_failure_returns_correct_hop_index_of_failure() {
        let chan_capacity = 500_000_000;
        let test_kit =
            DispatchPaymentTestKit::new(chan_capacity, vec![], CustomRecords::default()).await;

        let mut node = SimNode::new(
            node_info(test_kit.nodes[0], String::default()),
            Arc::new(Mutex::new(test_kit.graph)),
            test_kit.routing_graph.clone(),
            Arc::new(SystemClock {}),
            DefaultPathFinder::new()        
        )
        .unwrap();

        let route = build_route_from_hops(
            &test_kit.nodes[0],
            &[test_kit.nodes[1], test_kit.nodes[2], test_kit.nodes[3]],
            &RouteParameters {
                payment_params: PaymentParameters::from_node_id(*test_kit.nodes.last().unwrap(), 0)
                    .with_max_total_cltv_expiry_delta(u32::MAX)
                    .with_max_path_count(1)
                    .with_max_channel_saturation_power_of_half(1),
                final_value_msat: 20_000,
                max_total_routing_fee_msat: None,
            },
            &test_kit.routing_graph,
            &WrappedLog {},
            &[0; 32],
        )
        .unwrap();

        // We are altering the route to make sure that we fail at each hop in turn.
        // We will assert that the failure index returned is the index of the hop that we altered.
        // This is done by setting the CLTV expiry delta to a value that will cause the payment to fail at that hop.
        // The last hop will always succeed, so we only test the first n-1 hops.
        for i in 0..route.paths[0].hops.len() - 1 {
            let mut route_clone = route.clone();
            // Alter route_clone to make payment fail on this hop.
            route_clone.paths[0].hops[i].cltv_expiry_delta = 39;

            let preimage = PaymentPreimage(rand::random());
            let payment_hash = preimage.into();
            let send_result = node.send_to_route(route_clone, payment_hash, None).await;

            assert!(send_result.is_ok());

            let (_, shutdown_listener) = triggered::trigger();
            let result = node
                .track_payment(&payment_hash, shutdown_listener)
                .await
                .unwrap();

            assert!(matches!(
                result.payment_outcome,
                PaymentOutcome::IndexFailure(idx) if idx == i
            ));
        }
    }

    mock! {
        #[derive(Debug)]
        TestInterceptor{}

        #[async_trait]
        impl Interceptor for TestInterceptor {
            async fn intercept_htlc(&self, req: InterceptRequest) -> Result<Result<CustomRecords, ForwardingError>, CriticalError>;
            async fn notify_resolution(
                &self,
                res: InterceptResolution,
            ) -> Result<(), CriticalError>;
            fn name(&self) -> String;
        }
    }

    /// Contains elements required to test dispatch_payment functionality.
    struct DispatchPaymentTestKit {
        graph: SimGraph,
        nodes: Vec<PublicKey>,
        routing_graph: Arc<LdkNetworkGraph>,
        shutdown: (Trigger, Listener),
        pathfinder: DefaultPathFinder,
    }

    impl DispatchPaymentTestKit {
        /// Creates a test graph with a set of nodes connected by three channels, with all the capacity of the channel
        /// on the side of the first node. For example, if called with capacity = 100 it will set up the following
        /// network:
        /// Alice (100) --- (0) Bob (100) --- (0) Carol (100) --- (0) Dave
        ///
        /// The nodes pubkeys in this chain of channels are provided in-order for easy access.
        async fn new(
            capacity: u64,
            interceptors: Vec<Arc<dyn Interceptor>>,
            custom_records: CustomRecords,
        ) -> Self {
            let shutdown_signal = triggered::trigger();
            let channels = create_simulated_channels(3, capacity);
            let routing_graph = Arc::new(
                populate_network_graph(channels.clone(), Arc::new(SystemClock {})).unwrap(),
            );

            // Collect pubkeys in-order, pushing the last node on separately because they don't have an outgoing
            // channel (they are not node_1 in any channel, only node_2).
            let mut nodes = channels
                .iter()
                .map(|c| c.node_1.policy.pubkey)
                .collect::<Vec<PublicKey>>();
            nodes.push(channels.last().unwrap().node_2.policy.pubkey);

            let shutdown_clone = shutdown_signal.clone();
            let kit = DispatchPaymentTestKit {
                graph: SimGraph::new(
                    channels.clone(),
                    TaskTracker::new(),
                    interceptors,
                    custom_records,
                    shutdown_signal,
                )
                .expect("could not create test graph"),
                nodes,
                routing_graph,
                shutdown: shutdown_clone,
                pathfinder: DefaultPathFinder::new(),
            };

            // Assert that our channel balance is all on the side of the channel opener when we start up.
            assert_eq!(
                kit.channel_balances().await,
                vec![(capacity, 0), (capacity, 0), (capacity, 0)]
            );

            kit
        }

        /// Returns a vector of local/remote channel balances for channels in the network.
        async fn channel_balances(&self) -> Vec<(u64, u64)> {
            let mut balances = vec![];

            // We can't iterate through our hashmap of channels in-order, so we take advantage of our short channel id
            // being the index in our chain of channels. This allows us to look up channels in-order.
            let chan_count = self.graph.channels.lock().await.len();

            for i in 0..chan_count {
                let chan_lock = self.graph.channels.lock().await;
                let channel = chan_lock.get(&ShortChannelID::from(i as u64)).unwrap();

                // Take advantage of our test setup, which always makes node_1 the channel initiator to get our
                // "in order" balances for the chain of channels.
                balances.push((
                    channel.node_1.local_balance_msat,
                    channel.node_2.local_balance_msat,
                ));
            }

            balances
        }

        // Sends a test payment from source to destination and waits for the payment to complete, returning the route
        // used and the payment result.
        async fn send_test_payment(
            &mut self,
            source: PublicKey,
            dest: PublicKey,
            amt: u64,
        ) -> (Route, Result<PaymentResult, LightningError>) {
            let route = self.pathfinder.find_route(
                &source,
                dest,
                amt,
                &self.routing_graph,
            ).unwrap();

            self.graph
                .dispatch_payment(source, route.clone(), None, PaymentHash([0; 32]), sender);

            (route, receiver.await.unwrap())
        }

        // Sets the balance on the channel to the tuple provided, used to arrange liquidity for testing.
        async fn set_channel_balance(&mut self, scid: &ShortChannelID, balance: (u64, u64)) {
            let mut channels_lock = self.graph.channels.lock().await;
            let channel = channels_lock.get_mut(scid).unwrap();

            channel.node_1.local_balance_msat = balance.0;
            channel.node_2.local_balance_msat = balance.1;

            assert!(channel.sanity_check().is_ok());
        }
    }

    /// Tests dispatch of a successfully settled payment across a test network of simulated channels:
    /// Alice --- Bob --- Carol --- Dave
    #[tokio::test]
    async fn test_successful_dispatch() {
        let chan_capacity = 500_000_000;
        let mut test_kit =
            DispatchPaymentTestKit::new(chan_capacity, vec![], CustomRecords::default()).await;

        // Send a payment that should succeed from Alice -> Dave.
        let mut amt = 20_000;
        let (route, _) = test_kit
            .send_test_payment(test_kit.nodes[0], test_kit.nodes[3], amt)
            .await;

        let route_total = amt + route.get_total_fees();
        let hop_1_amt = amt + route.paths[0].hops[1].fee_msat;

        // The sending node should have pushed the amount + total fee to the intermediary.
        let alice_to_bob = (chan_capacity - route_total, route_total);
        // The middle hop should include fees for the outgoing link.
        let mut bob_to_carol = (chan_capacity - hop_1_amt, hop_1_amt);
        // The receiving node should have the payment amount pushed to them.
        let carol_to_dave = (chan_capacity - amt, amt);

        let mut expected_balances = vec![alice_to_bob, bob_to_carol, carol_to_dave];
        assert_eq!(test_kit.channel_balances().await, expected_balances);

        // Next, we'll test the case where a payment fails on the first hop. This is an edge case in our state
        // machine, so we want to specifically hit it. To do this, we'll try to send double the amount that we just
        // pushed to Dave back to Bob, expecting a failure on Dave's outgoing link due to insufficient liquidity.
        let _ = test_kit
            .send_test_payment(test_kit.nodes[3], test_kit.nodes[1], amt * 2)
            .await;
        assert_eq!(test_kit.channel_balances().await, expected_balances);

        // Now, test a successful single-hop payment from Bob -> Carol. We'll do this twice, so that we can drain all
        // the liquidity on Bob's side (to prepare for a multi-hop failure test). Our pathfinding only allows us to
        // use 50% of the channel's capacity, so we need to do two payments.
        amt = bob_to_carol.0 / 2;
        let _ = test_kit
            .send_test_payment(test_kit.nodes[1], test_kit.nodes[2], amt)
            .await;

        bob_to_carol = (bob_to_carol.0 / 2, bob_to_carol.1 + amt);
        expected_balances = vec![alice_to_bob, bob_to_carol, carol_to_dave];
        assert_eq!(test_kit.channel_balances().await, expected_balances);

        // When we push this amount a second time, all the liquidity should be moved to Carol's end.
        let _ = test_kit
            .send_test_payment(test_kit.nodes[1], test_kit.nodes[2], amt)
            .await;
        bob_to_carol = (0, chan_capacity);
        expected_balances = vec![alice_to_bob, bob_to_carol, carol_to_dave];
        assert_eq!(test_kit.channel_balances().await, expected_balances);

        // Finally, we'll test a multi-hop failure by trying to send from Alice -> Dave. Since Bob's liquidity is
        // drained, we expect a failure and unchanged balances along the route.
        let _ = test_kit
            .send_test_payment(test_kit.nodes[0], test_kit.nodes[3], 20_000)
            .await;
        assert_eq!(test_kit.channel_balances().await, expected_balances);

        test_kit.shutdown.0.trigger();
        test_kit.graph.tasks.close();
        test_kit.graph.tasks.wait().await;
    }

    /// Tests successful dispatch of a multi-hop payment.
    #[tokio::test]
    async fn test_successful_multi_hop() {
        let chan_capacity = 500_000_000;
        let mut test_kit =
            DispatchPaymentTestKit::new(chan_capacity, vec![], CustomRecords::default()).await;

        // Send a payment that should succeed from Alice -> Dave.
        let amt = 20_000;
        let (route, _) = test_kit
            .send_test_payment(test_kit.nodes[0], test_kit.nodes[3], amt)
            .await;

        let route_total = amt + route.get_total_fees();
        let hop_1_amt = amt + route.paths[0].hops[1].fee_msat;

        let expected_balances = vec![
            // The sending node should have pushed the amount + total fee to the intermediary.
            (chan_capacity - route_total, route_total),
            // The middle hop should include fees for the outgoing link.
            (chan_capacity - hop_1_amt, hop_1_amt),
            // The receiving node should have the payment amount pushed to them.
            (chan_capacity - amt, amt),
        ];
        assert_eq!(test_kit.channel_balances().await, expected_balances);

        test_kit.shutdown.0.trigger();
        test_kit.graph.tasks.close();
        test_kit.graph.tasks.wait().await;
    }

    /// Tests success and failure for single hop payments, which are an edge case in our state machine.
    #[tokio::test]
    async fn test_single_hop_payments() {
        let chan_capacity = 500_000_000;
        let mut test_kit =
            DispatchPaymentTestKit::new(chan_capacity, vec![], CustomRecords::default()).await;

        // Send a single hop payment from Alice -> Bob, it will succeed because Alice has all the liquidity.
        let amt = 150_000;
        let _ = test_kit
            .send_test_payment(test_kit.nodes[0], test_kit.nodes[1], amt)
            .await;

        let expected_balances = vec![
            (chan_capacity - amt, amt),
            (chan_capacity, 0),
            (chan_capacity, 0),
        ];
        assert_eq!(test_kit.channel_balances().await, expected_balances);

        // Send a single hop payment from Dave -> Carol that will fail due to lack of liquidity, balances should be
        // unchanged.
        let _ = test_kit
            .send_test_payment(test_kit.nodes[3], test_kit.nodes[2], amt)
            .await;

        assert_eq!(test_kit.channel_balances().await, expected_balances);

        test_kit.shutdown.0.trigger();
        test_kit.graph.tasks.close();
        test_kit.graph.tasks.wait().await;
    }

    /// Tests failing back of multi-hop payments at various failure indexes.
    #[tokio::test]
    async fn test_multi_hop_faiulre() {
        let chan_capacity = 500_000_000;
        let mut test_kit =
            DispatchPaymentTestKit::new(chan_capacity, vec![], CustomRecords::default()).await;

        // Drain liquidity between Bob and Carol to force failures on Bob's outgoing linke.
        test_kit
            .set_channel_balance(&ShortChannelID::from(1), (0, chan_capacity))
            .await;

        let mut expected_balances =
            vec![(chan_capacity, 0), (0, chan_capacity), (chan_capacity, 0)];
        assert_eq!(test_kit.channel_balances().await, expected_balances);

        // Send a payment from Alice -> Dave which we expect to fail leaving balances unaffected.
        let amt = 150_000;
        let _ = test_kit
            .send_test_payment(test_kit.nodes[0], test_kit.nodes[3], amt)
            .await;

        assert_eq!(test_kit.channel_balances().await, expected_balances);

        // Push liquidity to Dave so that we can send a payment which will fail on Bob's outgoing link, leaving
        // balances unaffected.
        expected_balances[2] = (0, chan_capacity);
        test_kit
            .set_channel_balance(&ShortChannelID::from(2), (0, chan_capacity))
            .await;

        let _ = test_kit
            .send_test_payment(test_kit.nodes[3], test_kit.nodes[0], amt)
            .await;

        assert_eq!(test_kit.channel_balances().await, expected_balances);

        test_kit.shutdown.0.trigger();
        test_kit.graph.tasks.close();
        test_kit.graph.tasks.wait().await;
    }

    #[tokio::test]
    async fn test_send_and_track_payment_to_route() {
        let chan_capacity = 500_000_000;
        let test_kit =
            DispatchPaymentTestKit::new(chan_capacity, vec![], CustomRecords::default()).await;

        let mut node = SimNode::new(
            node_info(test_kit.nodes[0], String::default()),
            Arc::new(Mutex::new(test_kit.graph)),
            test_kit.routing_graph.clone(),
            Arc::new(SystemClock {}),
            DefaultPathFinder::new(),
        )
        .unwrap();

        let route = build_route_from_hops(
            &test_kit.nodes[0],
            &[test_kit.nodes[1], test_kit.nodes[2], test_kit.nodes[3]],
            &RouteParameters {
                payment_params: PaymentParameters::from_node_id(*test_kit.nodes.last().unwrap(), 0)
                    .with_max_total_cltv_expiry_delta(u32::MAX)
                    // TODO: set non-zero value to support MPP.
                    .with_max_path_count(1)
                    // Allow sending htlcs up to 50% of the channel's capacity.
                    .with_max_channel_saturation_power_of_half(1),
                final_value_msat: 20_000,
                max_total_routing_fee_msat: None,
            },
            &test_kit.routing_graph,
            &WrappedLog {},
            &[0; 32],
        )
        .unwrap();

        let preimage = PaymentPreimage(rand::random());
        let payment_hash = preimage.into();
        node.send_to_route(route, payment_hash, None).await.unwrap();

        let (_, shutdown_listener) = triggered::trigger();
        let result = node
            .track_payment(&payment_hash, shutdown_listener)
            .await
            .unwrap();

        assert!(matches!(result.payment_outcome, PaymentOutcome::Success));
    }

    fn create_intercept_request(shutdown_listener: Listener) -> InterceptRequest {
        let (_, pubkey) = get_random_keypair();
        InterceptRequest {
            forwarding_node: pubkey,
            payment_hash: PaymentHash([0; 32]),
            incoming_htlc: HtlcRef {
                channel_id: ShortChannelID(0),
                index: 0,
            },
            incoming_custom_records: CustomRecords::default(),
            outgoing_channel_id: None,
            incoming_amount_msat: 0,
            outgoing_amount_msat: 0,
            incoming_expiry_height: 0,
            outgoing_expiry_height: 0,
            shutdown_listener,
        }
    }

    /// Tests intercepted htlc failures.
    #[tokio::test]
    async fn test_intercepted_htlc_failure() {
        // Test with 2 interceptors where one of them returns a signal to fail the htlc.
        let mut mock_interceptor_1 = MockTestInterceptor::new();
        mock_interceptor_1
            .expect_intercept_htlc()
            .returning(|_| Ok(Ok(CustomRecords::default())));
        mock_interceptor_1
            .expect_notify_resolution()
            .returning(|_| Ok(()));

        let mut mock_interceptor_2 = MockTestInterceptor::new();
        mock_interceptor_2.expect_intercept_htlc().returning(|_| {
            Ok(Err(ForwardingError::InterceptorError(
                "failing from mock interceptor".into(),
            )))
        });
        mock_interceptor_2
            .expect_notify_resolution()
            .returning(|_| Ok(()));

        let (interceptor_trigger, interceptor_listener) = triggered::trigger();
        let mock_request = create_intercept_request(interceptor_listener);

        let mock_interceptor_1: Arc<dyn Interceptor> = Arc::new(mock_interceptor_1);
        let mock_interceptor_2: Arc<dyn Interceptor> = Arc::new(mock_interceptor_2);
        let interceptors = vec![mock_interceptor_1, mock_interceptor_2];
        let (_, shutdown_listener) = triggered::trigger();

        let response = handle_intercepted_htlc(
            mock_request.clone(),
            &interceptors,
            interceptor_trigger.clone(),
            shutdown_listener,
        )
        .await;

        // Test we got internal error back if there was a ForwardingError during interception.
        assert_true!(response.unwrap().is_err());

        let mut mock_interceptor_1 = MockTestInterceptor::new();
        mock_interceptor_1
            .expect_intercept_htlc()
            .returning(|_| Ok(Ok(CustomRecords::from([(1000, vec![1])]))));
        mock_interceptor_1
            .expect_notify_resolution()
            .returning(|_| Ok(()));

        // Try to set the a different value for the same key.
        let mut mock_interceptor_2 = MockTestInterceptor::new();
        mock_interceptor_2
            .expect_intercept_htlc()
            .returning(|_| Ok(Ok(CustomRecords::from([(1000, vec![5])]))));
        mock_interceptor_2
            .expect_notify_resolution()
            .returning(|_| Ok(()));

        let mock_interceptor_1: Arc<dyn Interceptor> = Arc::new(mock_interceptor_1);
        let mock_interceptor_2: Arc<dyn Interceptor> = Arc::new(mock_interceptor_2);

        let interceptors = vec![mock_interceptor_1, mock_interceptor_2];
        let (_, shutdown_listener) = triggered::trigger();

        let response = handle_intercepted_htlc(
            mock_request,
            &interceptors,
            interceptor_trigger,
            shutdown_listener,
        )
        .await;

        // Test conflicting records return outer CriticalError.
        assert_true!(response.is_err());
    }

    /// Tests intercepted htlc success.
    #[tokio::test]
    async fn test_intercepted_htlc_success() {
        let mut mock_interceptor_1 = MockTestInterceptor::new();
        mock_interceptor_1
            .expect_intercept_htlc()
            .returning(|_| Ok(Ok(CustomRecords::from([(1000, vec![1])]))));
        mock_interceptor_1
            .expect_notify_resolution()
            .returning(|_| Ok(()));

        let (interceptor_trigger, interceptor_listener) = triggered::trigger();
        let mock_request = create_intercept_request(interceptor_listener);

        let mock_interceptor_1: Arc<dyn Interceptor> = Arc::new(mock_interceptor_1);
        let interceptors = vec![mock_interceptor_1];
        let (_, shutdown_listener) = triggered::trigger();

        let response = handle_intercepted_htlc(
            mock_request,
            &interceptors,
            interceptor_trigger,
            shutdown_listener,
        )
        .await;

        // Test we got Ok response with custom records set by interceptor.
        assert!(response.unwrap().unwrap() == CustomRecords::from([(1000, vec![1])]));
    }

    /// Tests a long resolving interceptor gets correctly interrupted during a shutdown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_shutdown_intercepted_htlc() {
        let mut mock_interceptor_1 = MockTestInterceptor::new();

        mock_interceptor_1.expect_intercept_htlc().returning(|req| {
            futures::executor::block_on(async {
                tokio::select! {
                    _ = req.shutdown_listener => {},
                    // Set a long sleep to simulate a slow interceptor. We'll trigger a shutdown
                    // immediately so it should never take this long.
                    _ = time::sleep(Duration::from_secs(30)) => {}
                }
            });
            Ok(Ok(CustomRecords::default()))
        });

        let (interceptor_trigger, interceptor_listener) = triggered::trigger();
        let mock_request = create_intercept_request(interceptor_listener);

        let mock_1: Arc<dyn Interceptor> = Arc::new(mock_interceptor_1);
        let interceptors = vec![mock_1];

        // This should assert early instead of waiting for the sleep time since we triggered a
        // shutdown.
        let (trigger, listener) = triggered::trigger();
        trigger.trigger();
        assert_true!(timeout(Duration::from_secs(1), async {
            handle_intercepted_htlc(mock_request, &interceptors, interceptor_trigger, listener)
                .await
        })
        .await
        .is_ok());

        // Test forwarding error does not trigger simulation shutdown.
        let mut mock_interceptor_1 = MockTestInterceptor::new();
        mock_interceptor_1.expect_intercept_htlc().returning(|_| {
            Ok(Err(ForwardingError::InterceptorError(
                "failing from mock interceptor".into(),
            )))
        });
        mock_interceptor_1
            .expect_notify_resolution()
            .returning(|_| Ok(()));

        let mock_1 = Arc::new(mock_interceptor_1);
        let mut test_kit =
            DispatchPaymentTestKit::new(500_000_000, vec![mock_1], CustomRecords::default()).await;
        let (_, result) = test_kit
            .send_test_payment(test_kit.nodes[0], test_kit.nodes[3], 150_000_000)
            .await;

        assert!(matches!(
            result.unwrap().payment_outcome,
            PaymentOutcome::IndexFailure(0)
        ));

        // The interceptor returned a forwarding error, check that a simulation shutdown has not
        // been triggered.
        select! {
            _ = test_kit.shutdown.1 => { panic!("simulation shut down by interceptor") },
            _ = time::sleep(Duration::from_millis(150)) => {}
        }

        test_kit.shutdown.0.trigger();
        test_kit.graph.tasks.close();
        test_kit.graph.tasks.wait().await;
    }

    /// Tests custom records set for interceptors in multi-hop payment.
    #[tokio::test]
    async fn test_custom_records() {
        let custom_records = HashMap::from([(1000, vec![1])]);

        let mut mock_interceptor_1 = MockTestInterceptor::new();
        let custom_records_clone = custom_records.clone();
        mock_interceptor_1
            .expect_intercept_htlc()
            .withf(move |req: &InterceptRequest| {
                // Check custom records passed to interceptor are the default ones set.
                req.incoming_custom_records == custom_records_clone
            })
            .returning(|_| Ok(Ok(CustomRecords::default()))); // Set empty records for 2nd hop.
        mock_interceptor_1
            .expect_notify_resolution()
            .returning(|_| Ok(()))
            .times(2);

        mock_interceptor_1
            .expect_intercept_htlc()
            .withf(move |req: &InterceptRequest| {
                // On this 2nd hop, the custom records should be empty.
                req.incoming_custom_records == CustomRecords::default()
            })
            .returning(|_| Ok(Ok(CustomRecords::default())));

        let chan_capacity = 500_000_000;
        let mock_1: Arc<dyn Interceptor> = Arc::new(mock_interceptor_1);
        let mut test_kit =
            DispatchPaymentTestKit::new(chan_capacity, vec![mock_1], custom_records).await;
        let _ = test_kit
            .send_test_payment(test_kit.nodes[0], test_kit.nodes[2], 150_000)
            .await;
    }
}
