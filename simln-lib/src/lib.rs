#![deny(rustdoc::broken_intra_doc_links)]

use self::batched_writer::BatchedWriter;
use self::clock::Clock;
use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use lightning::ln::features::NodeFeatures;
use lightning::ln::PaymentHash;
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use random_activity::RandomActivityError;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::marker::Send;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::time::{SystemTimeError, UNIX_EPOCH};
use std::{collections::HashMap, sync::Arc, time::SystemTime};
use thiserror::Error;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::{select, time, time::Duration};
use tokio_util::task::TaskTracker;
use triggered::{Listener, Trigger};

use self::defined_activity::DefinedPaymentActivity;
use self::random_activity::{NetworkGraphView, RandomPaymentActivity};

pub mod batched_writer;
pub mod cln;
pub mod clock;
mod defined_activity;
pub mod eclair;
pub mod latency_interceptor;
pub mod lnd;
mod random_activity;
pub mod serializers;
pub mod sim_node;
mod test_utils;

/// Represents a node id, either by its public key or alias.
#[derive(Serialize, Debug, Clone)]
pub enum NodeId {
    /// The node's public key.
    PublicKey(PublicKey),
    /// The node's alias (human-readable name).
    Alias(String),
}

impl NodeId {
    /// Validates that the provided node id matches the one returned by the backend. If the node id is an alias,
    /// it will be updated to the one returned by the backend if there is a mismatch.
    pub fn validate(&self, node_id: &PublicKey, alias: &mut String) -> Result<(), LightningError> {
        match self {
            crate::NodeId::PublicKey(pk) => {
                if pk != node_id {
                    return Err(LightningError::ValidationError(format!(
                        "The provided node id does not match the one returned by the backend ({} != {}).",
                        pk, node_id
                    )));
                }
            },
            crate::NodeId::Alias(a) => {
                if alias != a {
                    log::warn!(
                        "The provided alias does not match the one returned by the backend ({} != {}).",
                        a,
                        alias
                    )
                }
                *alias = a.to_string();
            },
        }
        Ok(())
    }

    /// Returns the public key of the node if it is a public key node id.
    pub fn get_pk(&self) -> Result<&PublicKey, String> {
        if let NodeId::PublicKey(pk) = self {
            Ok(pk)
        } else {
            Err("NodeId is not a PublicKey".to_string())
        }
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                NodeId::PublicKey(pk) => pk.to_string(),
                NodeId::Alias(a) => a.to_owned(),
            }
        )
    }
}

/// Represents a short channel ID, expressed as a struct so that we can implement display for the trait.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy, Serialize, Deserialize)]
pub struct ShortChannelID(u64);

/// Utility function to easily convert from u64 to `ShortChannelID`.
impl From<u64> for ShortChannelID {
    fn from(value: u64) -> Self {
        ShortChannelID(value)
    }
}

/// Utility function to easily convert `ShortChannelID` into u64.
impl From<ShortChannelID> for u64 {
    fn from(scid: ShortChannelID) -> Self {
        scid.0
    }
}

/// See <https://github.com/lightning/bolts/blob/60de4a09727c20dea330f9ee8313034de6e50594/07-routing-gossip.md#definition-of-short_channel_id>
impl std::fmt::Display for ShortChannelID {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            (self.0 >> 40) as u32,
            ((self.0 >> 16) & 0xFFFFFF) as u32,
            (self.0 & 0xFFFF) as u16,
        )
    }
}

/// Either a value or a range parsed from the simulation file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ValueOrRange<T> {
    /// A single fixed value.
    Value(T),
    /// A range [min, max) from which values are randomly sampled.
    Range(T, T),
}

impl<T> ValueOrRange<T>
where
    T: std::cmp::PartialOrd + rand_distr::uniform::SampleUniform + Copy,
{
    /// Get the enclosed value. If value is defined as a range, sample from it uniformly at random.
    pub fn value(&self) -> T {
        match self {
            ValueOrRange::Value(x) => *x,
            ValueOrRange::Range(x, y) => {
                let mut rng = rand::thread_rng();
                rng.gen_range(*x..*y)
            },
        }
    }
}

impl<T> Display for ValueOrRange<T>
where
    T: Display,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ValueOrRange::Value(x) => write!(f, "{x}"),
            ValueOrRange::Range(x, y) => write!(f, "({x}-{y})"),
        }
    }
}

/// The payment amount in msat. Either a value or a range.
pub type Amount = ValueOrRange<u64>;
/// The interval of seconds between payments. Either a value or a range.
pub type Interval = ValueOrRange<u16>;

/// Data structure used internally by the simulator. Both source and destination are represented as [PublicKey] here.
/// This is constructed during activity validation and passed along to the [Simulation].
#[derive(Debug, Clone)]
pub struct ActivityDefinition {
    /// The source of the payment.
    pub source: NodeInfo,
    /// The destination of the payment.
    pub destination: NodeInfo,
    /// The time in the simulation to start the payment.
    pub start_secs: Option<u16>,
    /// The number of payments to send over the course of the simulation.
    pub count: Option<u64>,
    /// The interval of the event, as in every how many seconds the payment is performed.
    pub interval_secs: Interval,
    /// The amount of m_sat to used in this payment.
    pub amount_msat: Amount,
}

/// Represents errors that can occur during simulation execution.
#[derive(Debug, Error)]
pub enum SimulationError {
    /// Error that occurred during Lightning Network operations.
    #[error("Lightning Error: {0:?}")]
    LightningError(#[from] LightningError),
    /// Error that occurred during task execution.
    #[error("TaskError")]
    TaskError,
    /// Error that occurred while writing CSV data.
    #[error("CSV Error: {0:?}")]
    CsvError(#[from] csv::Error),
    /// Error that occurred during file operations.
    #[error("File Error")]
    FileError,
    /// Error that occurred during random activity generation.
    #[error("{0}")]
    RandomActivityError(RandomActivityError),
    /// Error that occurred in the simulated network.
    #[error("Simulated Network Error: {0}")]
    SimulatedNetworkError(String),
    /// Error that occurred while accessing system time.
    #[error("System Time Error: {0}")]
    SystemTimeError(#[from] SystemTimeError),
    /// Error that occurred when a required node was not found.
    #[error("Missing Node Error: {0}")]
    MissingNodeError(String),
    /// Error that occurred in message passing channels.
    #[error("Mpsc Channel Error: {0}")]
    MpscChannelError(String),
    /// Error that occurred while generating payment parameters.
    #[error("Payment Generation Error: {0}")]
    PaymentGenerationError(PaymentGenerationError),
    /// Error that occurred while generating destination nodes.
    #[error("Destination Generation Error: {0}")]
    DestinationGenerationError(DestinationGenerationError),
}

/// Represents errors that can occur during Lightning Network operations.
#[derive(Debug, Error)]
pub enum LightningError {
    /// Error that occurred while connecting to a Lightning node.
    #[error("Node connection error: {0}")]
    ConnectionError(String),
    /// Error that occurred while retrieving node information.
    #[error("Get info error: {0}")]
    GetInfoError(String),
    /// Error that occurred while sending a payment.
    #[error("Send payment error: {0}")]
    SendPaymentError(String),
    /// Error that occurred while tracking a payment.
    #[error("Track payment error: {0}")]
    TrackPaymentError(String),
    /// Error that occurred when a payment hash is invalid.
    #[error("Invalid payment hash")]
    InvalidPaymentHash,
    /// Error that occurred while retrieving information about a specific node.
    #[error("Get node info error: {0}")]
    GetNodeInfoError(String),
    /// Error that occurred during configuration validation.
    #[error("Config validation failed: {0}")]
    ValidationError(String),
    /// Error that represents a permanent failure condition.
    #[error("Permanent error: {0:?}")]
    PermanentError(String),
    /// Error that occurred while listing channels.
    #[error("List channels error: {0}")]
    ListChannelsError(String),
    /// Error that occurred while getting graph.
    #[error("Get graph error: {0}")]
    GetGraphError(String),
    /// Error that occured when getting clock info.
    #[error("SystemTime conversion error: {0}")]
    SystemTimeConversionError(#[from] SystemTimeError),
}

/// Information about a Lightning Network node.
/// - Alias: A human-readable name for the node.
/// - Features: The node's supported protocol features and capabilities,
///   used to determine compatibility and available
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// The node's public key.
    pub pubkey: PublicKey,
    /// A human-readable name for the node (may be empty).
    pub alias: String,
    /// The node's supported protocol features and capabilities.
    pub features: NodeFeatures,
}

impl Display for NodeInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pk = self.pubkey.to_string();
        let pk_summary = format!("{}...{}", &pk[..6], &pk[pk.len() - 6..]);
        if self.alias.is_empty() {
            write!(f, "{}", pk_summary)
        } else {
            write!(f, "{}({})", self.alias, pk_summary)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub channel_id: ShortChannelID,
    pub capacity_msat: u64,
}

#[derive(Debug, Clone)]
/// Graph represents the network graph of the simulated network and is useful for efficient lookups.
pub struct Graph {
    // Store nodes' information keyed by their public key.
    pub nodes_by_pk: HashMap<PublicKey, NodeInfo>,
}

impl Graph {
    pub fn new() -> Self {
        Graph {
            nodes_by_pk: HashMap::new(),
        }
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

/// LightningNode represents the functionality that is required to execute events on a lightning node.
#[async_trait]
pub trait LightningNode: Send {
    /// Get information about the node.
    fn get_info(&self) -> &NodeInfo;
    /// Get the network this node is running at.
    fn get_network(&self) -> Network;
    /// Keysend payment worth `amount_msat` from a source node to the destination node.
    async fn send_payment(
        &self,
        dest: PublicKey,
        amount_msat: u64,
    ) -> Result<PaymentHash, LightningError>;
    /// Track a payment with the specified hash.
    async fn track_payment(
        &self,
        hash: &PaymentHash,
        shutdown: Listener,
    ) -> Result<PaymentResult, LightningError>;
    /// Gets information on a specific node.
    async fn get_node_info(&self, node_id: &PublicKey) -> Result<NodeInfo, LightningError>;
    /// Lists all channels, at present only returns a vector of channel capacities in msat because no further
    /// information is required.
    async fn list_channels(&self) -> Result<Vec<u64>, LightningError>;
    /// Get the network graph from the point of view of a given node.
    async fn get_graph(&self) -> Result<Graph, LightningError>;
}

/// Represents an error that occurs when generating a destination for a payment.
#[derive(Debug, Error)]
#[error("Destination generation error: {0}")]
pub struct DestinationGenerationError(String);

/// A trait for selecting destination nodes for payments in the Lightning Network.
pub trait DestinationGenerator: Send {
    /// choose_destination picks a destination node within the network, returning the node's information and its
    /// capacity (if available).
    fn choose_destination(
        &self,
        source: PublicKey,
    ) -> Result<(NodeInfo, Option<u64>), DestinationGenerationError>;
}

/// Represents an error that occurs when generating payments.
#[derive(Debug, Error)]
#[error("Payment generation error: {0}")]
pub struct PaymentGenerationError(String);

/// A trait for generating payment parameters in the Lightning Network.
pub trait PaymentGenerator: Display + Send {
    /// Returns the time that the payments should start.
    fn payment_start(&self) -> Option<Duration>;

    /// Returns the number of payments that should be made.
    /// Returns `Some(n)` if there's a limit on the number of payments to dispatch, or `None` otherwise.
    fn payment_count(&self) -> Option<u64>;

    /// Returns the number of seconds that a node should wait until firing its next payment.
    fn next_payment_wait(&self) -> Result<time::Duration, PaymentGenerationError>;

    /// Returns a payment amount based, with a destination capacity optionally provided to inform the amount picked.
    fn payment_amount(
        &self,
        destination_capacity: Option<u64>,
    ) -> Result<u64, PaymentGenerationError>;
}

/// Represents the result of a payment attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentResult {
    /// The number of HTLCs (Hash Time Locked Contracts) used in the payment attempt.
    /// Multiple HTLCs may be used for a single payment when using techniques like multi-part payments or when
    /// retrying failed payment paths.
    pub htlc_count: usize,
    /// The final outcome of the payment attempt, indicating whether it succeeded or failed
    /// (and if failed, the reason for failure).
    pub payment_outcome: PaymentOutcome,
}

impl PaymentResult {
    /// Creates a new PaymentResult indicating that the payment was never dispatched. This is used when there was an
    /// error during the initial payment dispatch attempt (e.g., insufficient balance, invalid destination) with
    /// [`PaymentOutcome::NotDispatched`].
    pub fn not_dispatched() -> Self {
        PaymentResult {
            htlc_count: 0,
            payment_outcome: PaymentOutcome::NotDispatched,
        }
    }

    /// Creates a new PaymentResult indicating that tracking the payment failed. This is used when the payment was
    /// dispatched but the system was unable to determine its final outcome (e.g. due to connection issues or timeouts)
    /// with [`PaymentOutcome::TrackPaymentFailed`].
    pub fn track_payment_failed() -> Self {
        PaymentResult {
            htlc_count: 0,
            payment_outcome: PaymentOutcome::TrackPaymentFailed,
        }
    }
}

impl Display for PaymentResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Payment outcome: {:?} with {} htlcs",
            self.payment_outcome, self.htlc_count
        )
    }
}

/// Represents all possible outcomes of a Lightning Network payment attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PaymentOutcome {
    /// Payment completed successfully, reaching its intended recipient.
    Success,
    /// The recipient rejected the payment.
    RecipientRejected,
    /// The payment was cancelled by the sending user before completion.
    UserAbandoned,
    /// The payment failed after exhausting all retry attempts.
    RetriesExhausted,
    /// The payment expired before it could complete (e.g., HTLC timeout).
    PaymentExpired,
    /// No viable route could be found to the destination node.
    RouteNotFound,
    /// An unexpected error occurred during payment processing.
    UnexpectedError,
    /// The payment failed due to incorrect payment details (e.g., wrong invoice amount).
    IncorrectPaymentDetails,
    /// The sending node has insufficient balance to complete/dispatch the payment.
    InsufficientBalance,
    /// The payment failed for an unknown reason.
    Unknown,
    /// The payment was never dispatched due to an error during initial sending.
    NotDispatched,
    /// The payment was dispatched but its final status could not be determined.
    TrackPaymentFailed,
    /// The payment failed at the provided index in the path.
    IndexFailure(usize),
}

/// Describes a payment from a source node to a destination node.
#[derive(Debug, Clone, Copy, Serialize)]
struct Payment {
    /// Pubkey of the source node dispatching the payment.
    source: PublicKey,
    /// Pubkey of the destination node receiving the payment.
    destination: PublicKey,
    /// Amount of the payment in msat.
    amount_msat: u64,
    /// Hash of the payment if it has been successfully dispatched.
    #[serde(with = "serializers::serde_option_payment_hash")]
    hash: Option<PaymentHash>,
    /// Time at which the payment was dispatched.
    #[serde(with = "serde_millis")]
    dispatch_time: SystemTime,
}

impl Display for Payment {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let dispatch_time = self
            .dispatch_time
            .duration_since(UNIX_EPOCH)
            .expect("Failed to compute duration since unix epoch.");

        write!(
            f,
            "Payment {} dispatched at {:?} sending {} msat from {} -> {}.",
            self.hash.map(|h| hex::encode(h.0)).unwrap_or_default(),
            dispatch_time,
            self.amount_msat,
            self.source,
            self.destination,
        )
    }
}

/// SimulationEvent describes the set of actions that the simulator can run on nodes that it has execution permissions
/// on.
#[derive(Clone, Debug)]
enum SimulationEvent {
    /// Dispatch a payment of the specified amount to the public key provided.
    /// Results in `SimulationOutput::SendPaymentSuccess` or `SimulationOutput::SendPaymentFailure`.
    SendPayment(NodeInfo, u64),
}

/// SimulationOutput provides the output of a simulation event.
#[derive(Debug, Clone)]
enum SimulationOutput {
    /// Intermediate output for when simulator has successfully dispatched a payment.
    /// We need to track the result of the payment to report on it.
    SendPaymentSuccess(Payment),
    /// Final output for when simulator has failed to dispatch a payment.
    /// Report this as the final result of simulation event.
    SendPaymentFailure(Payment, PaymentResult),
}

/// MutRngType is a convenient type alias for any random number generator (RNG) type that
/// allows shared and exclusive access. This is necessary because a single RNG
/// is to be shared across multiple `DestinationGenerator`s and `PaymentGenerator`s
/// for deterministic outcomes.
///
/// **Note**: `StdMutex`, i.e. (`std::sync::Mutex`), is used here to avoid making the traits
/// `DestinationGenerator` and `PaymentGenerator` async.
type MutRngType = Arc<StdMutex<dyn RngCore + Send>>;

/// Newtype for `MutRngType` to encapsulate and hide implementation details for
/// creating new `MutRngType` types. Provides convenient API for the same purpose.
#[derive(Clone)]
struct MutRng(MutRngType);

impl MutRng {
    /// Creates a new MutRng given an optional `u64` seed and optional pubkey. If `seed_opt` is `Some`, random activity
    ///  generation in the simulator occurs near-deterministically.
    /// If it is `None`, activity generation is truly random, and based on a non-deterministic source of entropy.
    /// If a pubkey is provided, it will be used to salt the seed to ensure each node gets a deterministic but
    /// different RNG sequence.
    pub fn new(seed_opt: Option<(u64, Option<&PublicKey>)>) -> Self {
        let seed = match seed_opt {
            Some((seed, Some(pubkey))) => {
                let mut hasher = DefaultHasher::new();
                let mut combined = pubkey.serialize().to_vec();
                combined.extend_from_slice(&seed.to_le_bytes());
                combined.hash(&mut hasher);
                hasher.finish()
            },
            Some((seed, None)) => seed,
            None => rand::random(),
        };
        Self(Arc::new(StdMutex::new(ChaCha8Rng::seed_from_u64(seed))))
    }
}

/// Contains the configuration options for our simulation.
#[derive(Clone)]
pub struct SimulationCfg {
    /// Total simulation time. The simulation will run forever if undefined.
    total_time: Option<time::Duration>,
    /// The expected payment size for the network.
    expected_payment_msat: u64,
    /// The number of times that the network sends its total capacity in a month of operation when generating random
    /// activity.
    activity_multiplier: f64,
    /// Configurations for printing results to CSV. Results are not written if this option is None.
    write_results: Option<WriteResults>,
    /// Optional seed for deterministic random number generation.
    seed: Option<u64>,
}

impl SimulationCfg {
    pub fn new(
        total_time: Option<u32>,
        expected_payment_msat: u64,
        activity_multiplier: f64,
        write_results: Option<WriteResults>,
        seed: Option<u64>,
    ) -> Self {
        Self {
            total_time: total_time.map(|x| Duration::from_secs(x as u64)),
            expected_payment_msat,
            activity_multiplier,
            write_results,
            seed,
        }
    }
}

/// A Lightning Network payment simulator that manages payment flows between nodes.
/// The simulator can execute both predefined payment patterns and generate random payment activity
/// based on configuration parameters.
#[derive(Clone)]
pub struct Simulation<C: Clock> {
    /// Config for the simulation itself.
    cfg: SimulationCfg,
    /// The lightning node that is being simulated.
    nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
    /// Results logger that holds the simulation statistics.
    results: Arc<Mutex<PaymentResultLogger>>,
    /// Track all tasks spawned for use in the simulation. When used in the `run` method, it will wait for
    /// these tasks to complete before returning.
    tasks: TaskTracker,
    /// High level triggers used to manage simulation tasks and shutdown.
    shutdown_trigger: Trigger,
    shutdown_listener: Listener,
    /// Clock for the simulation.
    clock: Arc<C>,
}

/// Configuration for writing simulation results to CSV files.
#[derive(Clone)]
pub struct WriteResults {
    /// Data directory where CSV result files are written.
    pub results_dir: PathBuf,
    /// The number of activity results to batch before printing in CSV.
    pub batch_size: u32,
}

/// ExecutorKit contains the components required to spin up an activity configured by the user, to be used to
/// spin up the appropriate producers and consumers for the activity.
struct ExecutorKit {
    source_info: NodeInfo,
    /// We use an arc mutex here because some implementations of the trait will be very expensive to clone.
    /// See [NetworkGraphView] for details.
    network_generator: Arc<Mutex<dyn DestinationGenerator>>,
    payment_generator: Box<dyn PaymentGenerator>,
}

impl<C: Clock + 'static> Simulation<C> {
    pub fn new(
        cfg: SimulationCfg,
        nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
        tasks: TaskTracker,
        clock: Arc<C>,
        shutdown_trigger: Trigger,
        shutdown_listener: Listener,
    ) -> Self {
        Self {
            cfg,
            nodes,
            results: Arc::new(Mutex::new(PaymentResultLogger::new())),
            tasks,
            shutdown_trigger,
            shutdown_listener,
            clock,
        }
    }

    /// validate_activity validates that the user-provided activity description is achievable for the network that
    /// we're working with. If no activity description is provided, then it ensures that we have configured a network
    /// that is suitable for random activity generation.
    async fn validate_activity(
        &self,
        activity: &[ActivityDefinition],
    ) -> Result<(), LightningError> {
        // For now, empty activity signals random activity generation
        if activity.is_empty() {
            if self.nodes.len() <= 1 {
                return Err(LightningError::ValidationError(
                    "At least two nodes required for random activity generation.".to_string(),
                ));
            } else {
                for node in self.nodes.values() {
                    let node = node.lock().await;
                    if !node.get_info().features.supports_keysend() {
                        return Err(LightningError::ValidationError(format!(
                            "All nodes eligible for random activity generation must support keysend, {} does not",
                            node.get_info()
                        )));
                    }
                }
            }
        }

        for payment_flow in activity.iter() {
            if payment_flow.amount_msat.value() == 0 {
                return Err(LightningError::ValidationError(
                    "We do not allow defined activity amount_msat with zero values".to_string(),
                ));
            }
            // We need every source node that is configured to execute some activity to be included in our set of
            // nodes so that we can execute events on it.
            self.nodes
                .get(&payment_flow.source.pubkey)
                .ok_or(LightningError::ValidationError(format!(
                    "Source node not found, {}",
                    payment_flow.source,
                )))?;

            // Destinations must support keysend to be able to receive payments.
            // Note: validation should be update with a different check if an event is not a payment.
            if !payment_flow.destination.features.supports_keysend() {
                return Err(LightningError::ValidationError(format!(
                    "Destination node does not support keysend, {}",
                    payment_flow.destination,
                )));
            }
        }

        Ok(())
    }

    /// validates that the nodes are all on the same network and ensures that we're not running on mainnet.
    async fn validate_node_network(&self) -> Result<(), LightningError> {
        if self.nodes.is_empty() {
            return Err(LightningError::ValidationError(
                "we don't control any nodes. Specify at least one node in your config file"
                    .to_string(),
            ));
        }
        let mut running_network = Option::None;

        for node in self.nodes.values() {
            let network = node.lock().await.get_network();
            if network == Network::Bitcoin {
                return Err(LightningError::ValidationError(
                    "mainnet is not supported".to_string(),
                ));
            }

            running_network = running_network.take().or(Some(network));
            if running_network != Some(network) {
                return Err(LightningError::ValidationError(format!(
                    "nodes are not on the same network {}.",
                    network,
                )));
            }
        }

        log::info!(
            "Simulation is running on {}.",
            running_network.expect("Network not provided.")
        );

        Ok(())
    }

    /// run until the simulation completes or we hit an error.
    /// Note that it will wait for the tasks in self.tasks to complete
    /// before returning.
    pub async fn run(&self, activity: &[ActivityDefinition]) -> Result<(), SimulationError> {
        self.internal_run(activity).await?;
        // Close our TaskTracker and wait for any background tasks
        // spawned during internal_run to complete.
        self.tasks.close();
        self.tasks.wait().await;
        Ok(())
    }

    async fn internal_run(&self, activity: &[ActivityDefinition]) -> Result<(), SimulationError> {
        if let Some(total_time) = self.cfg.total_time {
            log::info!("Running the simulation for {}s.", total_time.as_secs());
        } else {
            log::info!("Running the simulation forever.");
        }

        self.validate_node_network().await?;
        self.validate_activity(activity).await?;

        log::info!(
            "Simulating {} activity on {} nodes.",
            activity.len(),
            self.nodes.len()
        );

        // Before we start the simulation up, start tasks that will be responsible for gathering simulation data.
        // The event channels are shared across our functionality:
        // - Event Sender: used by the simulation to inform data reporting that it needs to start tracking the
        //   final result of the event that it has taken.
        // - Event Receiver: used by data reporting to receive events that have been simulated that need to be
        //   tracked and recorded.
        let (event_sender, event_receiver) = channel(1);
        self.run_data_collection(event_receiver, &self.tasks);

        // Get an execution kit per activity that we need to generate and spin up consumers for each source node.
        let activities = match self.activity_executors(activity).await {
            Ok(a) => a,
            Err(e) => {
                // If we encounter an error while setting up the activity_executors,
                // we need to shutdown and return.
                self.shutdown();
                return Err(e);
            },
        };
        let consumer_channels = self.dispatch_consumers(
            activities
                .iter()
                .map(|generator| generator.source_info.pubkey)
                .collect(),
            event_sender.clone(),
            &self.tasks,
        );

        // Next, we'll spin up our actual producers that will be responsible for triggering the configured activity.
        // The producers will use their own TaskTracker so that the simulation can be shutdown if they all finish.
        let producer_tasks = TaskTracker::new();
        match self
            .dispatch_producers(activities, consumer_channels, &producer_tasks)
            .await
        {
            Ok(_) => {},
            Err(e) => {
                // If we encounter an error in dispatch_producers, we need to shutdown and return.
                self.shutdown();
                return Err(e);
            },
        }

        // Start a task that waits for the producers to finish.
        // If all producers finish, then there is nothing left to do and the simulation can be shutdown.
        let producer_trigger = self.shutdown_trigger.clone();
        self.tasks.spawn(async move {
            producer_tasks.close();
            producer_tasks.wait().await;
            log::info!("All producers finished. Shutting down.");
            producer_trigger.trigger()
        });

        // Start a task that will shutdown the simulation if the total_time is met.
        if let Some(total_time) = self.cfg.total_time {
            let shutdown = self.shutdown_trigger.clone();
            let listener = self.shutdown_listener.clone();
            let clock = self.clock.clone();

            self.tasks.spawn(async move {
                select! {
                    biased;
                    _ = listener.clone() => {
                        log::debug!("Timeout task exited on listener signal");
                    }

                    _ = clock.sleep(total_time) => {
                        log::info!(
                            "Simulation run for {}s. Shutting down.",
                            total_time.as_secs()
                        );
                        shutdown.trigger()
                    }
                }
            });
        }

        Ok(())
    }

    pub fn shutdown(&self) {
        self.shutdown_trigger.trigger()
    }

    pub async fn get_total_payments(&self) -> u64 {
        self.results.lock().await.total_attempts()
    }

    pub async fn get_success_rate(&self) -> f64 {
        self.results.lock().await.success_rate()
    }

    /// run_data_collection starts the tasks required for the simulation to report of the results of the activity that
    /// it generates. The simulation should report outputs via the receiver that is passed in.
    fn run_data_collection(
        &self,
        output_receiver: Receiver<SimulationOutput>,
        tasks: &TaskTracker,
    ) {
        let listener = self.shutdown_listener.clone();
        let shutdown = self.shutdown_trigger.clone();
        log::debug!("Setting up simulator data collection.");

        // Create a sender/receiver pair that will be used to report final results of simulation.
        let (results_sender, results_receiver) = channel(1);

        let nodes = self.nodes.clone();
        // psr: produce simulation results
        let psr_listener = listener.clone();
        let psr_shutdown = shutdown.clone();
        let psr_tasks = tasks.clone();
        tasks.spawn(async move {
            log::debug!("Starting simulation results producer.");
            if let Err(e) = produce_simulation_results(
                nodes,
                output_receiver,
                results_sender,
                psr_listener,
                &psr_tasks,
            )
            .await
            {
                psr_shutdown.trigger();
                log::error!("Produce simulation results exited with error: {e:?}.");
            } else {
                log::debug!("Produce simulation results received shutdown signal.");
            }
        });

        let result_logger = self.results.clone();

        let result_logger_clone = result_logger.clone();
        let result_logger_listener = listener.clone();
        let clock = self.clock.clone();
        tasks.spawn(async move {
            log::debug!("Starting results logger.");
            run_results_logger(
                result_logger_listener,
                result_logger_clone,
                Duration::from_secs(60),
                clock,
            )
            .await;
            log::debug!("Exiting results logger.");
        });

        // csr: consume simulation results
        let csr_write_results = self.cfg.write_results.clone();
        let clock = self.clock.clone();
        tasks.spawn(async move {
            log::debug!("Starting simulation results consumer.");
            if let Err(e) = consume_simulation_results(
                result_logger,
                clock,
                results_receiver,
                listener,
                csr_write_results,
            )
            .await
            {
                shutdown.trigger();
                log::error!("Consume simulation results exited with error: {e:?}.");
            } else {
                log::debug!("Consume simulation result received shutdown signal.");
            }
        });

        log::debug!("Simulator data collection set up.");
    }

    async fn activity_executors(
        &self,
        activity: &[ActivityDefinition],
    ) -> Result<Vec<ExecutorKit>, SimulationError> {
        let mut generators = Vec::new();

        // Note: when we allow configuring both defined and random activity, this will no longer be an if/else, we'll
        // just populate with each type as configured.
        if !activity.is_empty() {
            for description in activity.iter() {
                let activity_generator = DefinedPaymentActivity::new(
                    description.destination.clone(),
                    description
                        .start_secs
                        .map(|start| Duration::from_secs(start.into())),
                    description.count,
                    description.interval_secs,
                    description.amount_msat,
                );

                generators.push(ExecutorKit {
                    source_info: description.source.clone(),
                    // Defined activities have very simple generators, so the traits required are implemented on
                    // a single struct which we just cheaply clone.
                    network_generator: Arc::new(Mutex::new(activity_generator.clone())),
                    payment_generator: Box::new(activity_generator),
                });
            }
        } else {
            generators = self.random_activity_nodes().await?;
        }

        Ok(generators)
    }

    /// Returns the list of nodes that are eligible for generating random activity on. This is the subset of nodes
    /// that have sufficient capacity to generate payments of our expected payment amount.
    async fn random_activity_nodes(&self) -> Result<Vec<ExecutorKit>, SimulationError> {
        // Collect capacity of each node from its view of its own channels. Total capacity is divided by two to
        // avoid double counting capacity (as each node has a counterparty in the channel).
        let mut generators = Vec::new();
        let mut active_nodes = HashMap::new();

        // Do a first pass to get the capacity of each node which we need to be able to create a network generator.
        // While we're at it, we get the node info and store it with capacity to create activity generators in our
        // second pass.
        for (pk, node) in self.nodes.iter() {
            let chan_capacity = node.lock().await.list_channels().await?.iter().sum::<u64>();

            if let Err(e) = RandomPaymentActivity::validate_capacity(
                chan_capacity,
                self.cfg.expected_payment_msat,
            ) {
                log::warn!("Node: {} not eligible for activity generation: {e}.", *pk);
                continue;
            }

            // Don't double count channel capacity because each channel reports the total balance between counter
            // parities. Track capacity separately to be used for our network generator.
            let capacity = chan_capacity / 2;
            let node_info = node.lock().await.get_node_info(pk).await?;
            active_nodes.insert(node_info.pubkey, (node_info, capacity));
        }

        // Create a network generator with a shared RNG for all nodes.
        let network_generator = Arc::new(Mutex::new(
            NetworkGraphView::new(
                active_nodes.values().cloned().collect(),
                MutRng::new(self.cfg.seed.map(|seed| (seed, None))),
            )
            .map_err(SimulationError::RandomActivityError)?,
        ));

        log::info!(
            "Created network generator: {}.",
            network_generator.lock().await
        );

        for (node_info, capacity) in active_nodes.values() {
            // Create a salted RNG for this node based on its pubkey.
            let seed_opt = self.cfg.seed.map(|seed| (seed, Some(&node_info.pubkey)));
            let salted_rng = MutRng::new(seed_opt);
            generators.push(ExecutorKit {
                source_info: node_info.clone(),
                network_generator: network_generator.clone(),
                payment_generator: Box::new(
                    RandomPaymentActivity::new(
                        *capacity,
                        self.cfg.expected_payment_msat,
                        self.cfg.activity_multiplier,
                        salted_rng,
                    )
                    .map_err(SimulationError::RandomActivityError)?,
                ),
            });
        }

        Ok(generators)
    }

    /// Responsible for spinning up consumer tasks for each node specified in consuming_nodes. Assumes that validation
    /// has already ensured that we have execution on every nodes listed in consuming_nodes.
    fn dispatch_consumers(
        &self,
        consuming_nodes: HashSet<PublicKey>,
        output_sender: Sender<SimulationOutput>,
        tasks: &TaskTracker,
    ) -> HashMap<PublicKey, Sender<SimulationEvent>> {
        let mut channels = HashMap::new();

        for (id, node) in self
            .nodes
            .iter()
            .filter(|(id, _)| consuming_nodes.contains(id))
        {
            // For each node we have execution on, we'll create a sender and receiver channel to produce and consumer
            // events and insert producer in our tracking map. We do not buffer channels as we expect events to clear
            // quickly.
            let (sender, receiver) = channel(1);
            channels.insert(*id, sender.clone());

            // Generate a consumer for the receiving end of the channel. It takes the event receiver that it'll pull
            // events from and the results sender to report the events it has triggered for further monitoring.
            // ce: consume event.
            let ce_listener = self.shutdown_listener.clone();
            let ce_shutdown = self.shutdown_trigger.clone();
            let ce_output_sender = output_sender.clone();
            let ce_node = node.clone();
            let clock = self.clock.clone();
            tasks.spawn(async move {
                let node_info = ce_node.lock().await.get_info().clone();
                log::debug!("Starting events consumer for {}.", node_info);
                if let Err(e) =
                    consume_events(ce_node, clock, receiver, ce_output_sender, ce_listener).await
                {
                    ce_shutdown.trigger();
                    log::error!("Event consumer for node {node_info} exited with error: {e:?}.");
                } else {
                    log::debug!("Event consumer for node {node_info} completed successfully.");
                }
            });
        }

        channels
    }

    /// Responsible for spinning up producers for a set of activities. Requires that a consumer channel is present
    /// for every source node in the set of executors.
    async fn dispatch_producers(
        &self,
        executors: Vec<ExecutorKit>,
        producer_channels: HashMap<PublicKey, Sender<SimulationEvent>>,
        tasks: &TaskTracker,
    ) -> Result<(), SimulationError> {
        for executor in executors {
            let sender = producer_channels.get(&executor.source_info.pubkey).ok_or(
                SimulationError::RandomActivityError(RandomActivityError::ValueError(format!(
                    "Activity producer for: {} not found.",
                    executor.source_info.pubkey,
                ))),
            )?;

            // pe: produce events
            let pe_shutdown = self.shutdown_trigger.clone();
            let pe_listener = self.shutdown_listener.clone();
            let pe_sender = sender.clone();
            let clock = self.clock.clone();

            tasks.spawn(async move {
                let source = executor.source_info.clone();

                log::info!(
                    "Starting activity producer for {}: {}.",
                    source,
                    executor.payment_generator
                );

                if let Err(e) = produce_events(
                    executor.source_info,
                    executor.network_generator,
                    executor.payment_generator,
                    clock,
                    pe_sender,
                    pe_listener,
                )
                .await
                {
                    pe_shutdown.trigger();
                    log::debug!("Activity producer for {source} exited with error {e}.");
                } else {
                    log::debug!("Activity producer for {source} completed successfully.");
                }
            });
        }

        Ok(())
    }
}

/// events that are crated for a lightning node that we can execute events on. Any output that is generated from the
/// event being executed is piped into a channel to handle the result of the event.
async fn consume_events(
    node: Arc<Mutex<dyn LightningNode>>,
    clock: Arc<dyn Clock>,
    mut receiver: Receiver<SimulationEvent>,
    sender: Sender<SimulationOutput>,
    listener: Listener,
) -> Result<(), SimulationError> {
    loop {
        select! {
            biased;
            _ = listener.clone() => {
                return Ok(());
            },
            simulation_event = receiver.recv() => {
                if let Some(event) = simulation_event {
                    match event {
                        SimulationEvent::SendPayment(dest, amt_msat) => {
                            let node = node.lock().await;

                            let mut payment = Payment {
                                source: node.get_info().pubkey,
                                hash: None,
                                amount_msat: amt_msat,
                                destination: dest.pubkey,
                                dispatch_time: clock.now(),
                            };

                            let outcome = match node.send_payment(dest.pubkey, amt_msat).await {
                                Ok(payment_hash) => {
                                    log::debug!(
                                        "Send payment: {} -> {}: ({}).",
                                        node.get_info(),
                                        dest,
                                        hex::encode(payment_hash.0)
                                    );
                                    // We need to track the payment outcome using the payment hash that we have received.
                                    payment.hash = Some(payment_hash);
                                    SimulationOutput::SendPaymentSuccess(payment)
                                }
                                Err(e) => {
                                    log::error!(
                                        "Error while sending payment {} -> {}.",
                                        node.get_info(),
                                        dest
                                    );

                                    match e {
                                        LightningError::PermanentError(s) => {
                                            return Err(SimulationError::LightningError(LightningError::PermanentError(s)));
                                        }
                                        _ => SimulationOutput::SendPaymentFailure(
                                            payment,
                                            PaymentResult::not_dispatched(),
                                        ),
                                    }
                                }
                            };

                            select!{
                                biased;
                                _ = listener.clone() => {
                                    return Ok(())
                                }
                                send_result = sender.send(outcome.clone()) => {
                                    if send_result.is_err() {
                                        return Err(SimulationError::MpscChannelError(
                                                format!("Error sending simulation output {outcome:?}.")));
                                    }
                                }
                            }
                        }
                    }
                } else {
                    return Ok(())
                }
            }
        }
    }
}

/// produce events generates events for the activity description provided. It accepts a shutdown listener so it can
/// exit if other threads signal that they have errored out.
async fn produce_events<N: DestinationGenerator + ?Sized, A: PaymentGenerator + ?Sized>(
    source: NodeInfo,
    network_generator: Arc<Mutex<N>>,
    node_generator: Box<A>,
    clock: Arc<dyn Clock>,
    sender: Sender<SimulationEvent>,
    listener: Listener,
) -> Result<(), SimulationError> {
    let mut current_count = 0;
    loop {
        if let Some(c) = node_generator.payment_count() {
            if c == current_count {
                log::info!(
                    "Payment count has been met for {source}: {c} payments. Stopping the activity."
                );
                return Ok(());
            }
        }

        let wait = get_payment_delay(current_count, &source, node_generator.as_ref())?;

        select! {
            biased;
            _ = listener.clone() => {
                return Ok(());
            },
            // Wait until our time to next payment has elapsed then execute a random amount payment to a random
            // destination.
            _ = clock.sleep(wait) => {
                let (destination, capacity) = network_generator.lock().await.choose_destination(source.pubkey).map_err(SimulationError::DestinationGenerationError)?;

                // Only proceed with a payment if the amount is non-zero, otherwise skip this round. If we can't get
                // a payment amount something has gone wrong (because we should have validated that we can always
                // generate amounts), so we exit.
                let amount = match node_generator.payment_amount(capacity) {
                    Ok(amt) => {
                        if amt == 0 {
                            log::debug!("Skipping zero amount payment for {source} -> {destination}.");
                            continue;
                        }
                        amt
                    },
                    Err(e) => {
                        return Err(SimulationError::PaymentGenerationError(e));
                    },
                };

                log::debug!("Generated payment: {source} -> {}: {amount} msat.", destination);

                // Send the payment, exiting if we can no longer send to the consumer.
                let event = SimulationEvent::SendPayment(destination.clone(), amount);
                select!{
                    biased;
                    _ = listener.clone() => {
                        return Ok(());
                    },
                    send_result = sender.send(event.clone()) => {
                        if send_result.is_err(){
                            return Err(SimulationError::MpscChannelError(
                                    format!("Stopped activity producer for {amount}: {source} -> {destination}.")));
                        }
                    },
                }

                current_count += 1;
            },
        }
    }
}

/// Gets the wait time for the next payment. If this is the first payment being generated, and a specific start delay
/// was set we return a once-off delay. Otherwise, the interval between payments is used.
fn get_payment_delay<A: PaymentGenerator + ?Sized>(
    call_count: u64,
    source: &NodeInfo,
    node_generator: &A,
) -> Result<Duration, SimulationError> {
    // Note: we can't check if let Some() && call_count (syntax not allowed) so we add an additional branch in here.
    // The alternative is to call payment_start twice (which is _technically_ fine because it always returns the same
    // value), but this approach only costs us a few extra lines so we go for the more verbose approach so that we
    // don't have to make any assumptions about the underlying operation of payment_start.
    if call_count != 0 {
        let wait = node_generator
            .next_payment_wait()
            .map_err(SimulationError::PaymentGenerationError)?;
        log::debug!("Next payment for {source} in {:?}.", wait);
        Ok(wait)
    } else if let Some(start) = node_generator.payment_start() {
        log::debug!(
            "First payment for {source} will be after a start delay of {:?}.",
            start
        );
        Ok(start)
    } else {
        let wait = node_generator
            .next_payment_wait()
            .map_err(SimulationError::PaymentGenerationError)?;
        log::debug!("First payment for {source} in {:?}.", wait);
        Ok(wait)
    }
}

async fn consume_simulation_results(
    logger: Arc<Mutex<PaymentResultLogger>>,
    clock: Arc<dyn Clock>,
    mut receiver: Receiver<(Payment, PaymentResult)>,
    listener: Listener,
    write_results: Option<WriteResults>,
) -> Result<(), SimulationError> {
    let mut writer = None;
    if let Some(w) = write_results {
        // File name contains time that simulation was started.
        let file_name = format!(
            "simulation_{:?}.csv",
            clock
                .now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs()
        );

        writer = Some(BatchedWriter::new(w.results_dir, file_name, w.batch_size)?);
    }

    loop {
        select! {
            biased;
            _ = listener.clone() => {
                return writer.map_or(Ok(()), |mut w| w.write(true));
            },
            payment_result = receiver.recv() => {
                match payment_result {
                    Some((details, result)) => {
                        logger.lock().await.report_result(&details, &result);
                        log::trace!("Resolved dispatched payment: {} with: {}.", details, result);

                        if let Some(ref mut w) = writer{
                            w.queue((details, result))?;
                        }
                    },
                    None => return writer.map_or(Ok(()), |mut w| w.write(true)),
                }
            }
        }
    }
}

/// PaymentResultLogger is an aggregate logger that will report on a summary of the payments that have been reported.
#[derive(Default)]
struct PaymentResultLogger {
    success_payment: u64,
    failed_payment: u64,
    total_sent: u64,
}

impl PaymentResultLogger {
    fn new() -> Self {
        PaymentResultLogger {
            ..Default::default()
        }
    }

    fn report_result(&mut self, details: &Payment, result: &PaymentResult) {
        match result.payment_outcome {
            PaymentOutcome::Success => self.success_payment += 1,
            _ => self.failed_payment += 1,
        }

        self.total_sent += details.amount_msat;
    }

    fn total_attempts(&self) -> u64 {
        self.success_payment + self.failed_payment
    }

    fn success_rate(&self) -> f64 {
        (self.success_payment as f64 / self.total_attempts() as f64) * 100.0
    }
}

impl Display for PaymentResultLogger {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Processed {} payments sending {} msat total with {:.2}% success rate.",
            self.total_attempts(),
            self.total_sent,
            self.success_rate()
        )
    }
}

/// Reports a summary of payment results at a duration specified by `interval`
/// Note that `run_results_logger` does not error in any way, thus it has no
/// trigger. It listens for triggers to ensure clean exit.
async fn run_results_logger(
    listener: Listener,
    logger: Arc<Mutex<PaymentResultLogger>>,
    interval: Duration,
    clock: Arc<dyn Clock>,
) {
    log::info!("Summary of results will be reported every {:?}.", interval);

    loop {
        select! {
            biased;
            _ = listener.clone() => {
                break
            }

            _ = clock.sleep(interval) => {
                log::info!("{}", logger.lock().await)
            }
        }
    }
}

/// produce_results is responsible for receiving the outputs of events that the simulator has taken and
/// spinning up a producer that will report the results to our main result consumer. We handle each output
/// separately because they can take a long time to resolve (eg, a payment that ends up on chain will take a long
/// time to resolve).
///
/// Note: this producer does not accept a shutdown trigger because it only expects to be dispatched once. In the single
/// producer case exit will drop the only sending channel and the receiving channel provided to the consumer will error
/// out. In the multiple-producer case, a single producer shutting down does not drop *all* sending channels so the
/// consumer will not exit and a trigger is required.
async fn produce_simulation_results(
    nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
    mut output_receiver: Receiver<SimulationOutput>,
    results: Sender<(Payment, PaymentResult)>,
    listener: Listener,
    tasks: &TaskTracker,
) -> Result<(), SimulationError> {
    let result = loop {
        tokio::select! {
            biased;
            _ = listener.clone() => {
                break Ok(())
            },
            output = output_receiver.recv() => {
                match output {
                    Some(simulation_output) => {
                        match simulation_output{
                            SimulationOutput::SendPaymentSuccess(payment) => {
                                if let Some(source_node) = nodes.get(&payment.source) {
                                    tasks.spawn(track_payment_result(
                                        source_node.clone(), results.clone(), payment, listener.clone()
                                    ));
                                } else {
                                    break Err(SimulationError::MissingNodeError(format!("Source node with public key: {} unavailable.", payment.source)));
                                }
                            },
                            SimulationOutput::SendPaymentFailure(payment, result) => {
                                select!{
                                    _ = listener.clone() => {
                                        return Ok(());
                                    },
                                    send_result = results.send((payment, result.clone())) => {
                                        if send_result.is_err(){
                                            break Err(SimulationError::MpscChannelError(
                                                format!("Failed to send payment result: {result} for payment {:?} dispatched at {:?}.",
                                                        payment.hash, payment.dispatch_time),
                                            ));
                                        }
                                    },
                                }
                            }
                        };
                    },
                    None => break Ok(())
                }
            }
        }
    };

    log::debug!("Simulation results producer exiting.");

    result
}

async fn track_payment_result(
    node: Arc<Mutex<dyn LightningNode>>,
    results: Sender<(Payment, PaymentResult)>,
    payment: Payment,
    listener: Listener,
) -> Result<(), SimulationError> {
    log::trace!("Payment result tracker starting.");

    let node = node.lock().await;

    let res = match payment.hash {
        Some(hash) => {
            log::debug!("Tracking payment outcome for: {}.", hex::encode(hash.0));
            let track_payment = node.track_payment(&hash, listener.clone());

            match track_payment.await {
                Ok(res) => {
                    log::debug!(
                        "Track payment {} result: {:?}.",
                        hex::encode(hash.0),
                        res.payment_outcome
                    );
                    res
                },
                Err(e) => {
                    log::error!("Track payment failed for {}: {e}.", hex::encode(hash.0));
                    PaymentResult::track_payment_failed()
                },
            }
        },
        // None means that the payment was not dispatched, so we cannot track it.
        None => {
            log::error!(
                "We cannot track a payment that has not been dispatched. Missing payment hash."
            );
            PaymentResult::not_dispatched()
        },
    };

    select! {
        biased;
        _ = listener.clone() => {
            log::debug!("Track payment result received a shutdown signal.");
        },
        send_payment_result = results.send((payment, res.clone())) => {
            if send_payment_result.is_err() {
                return Err(SimulationError::MpscChannelError(
                        format!("Failed to send payment result {res} for payment {payment}.")))
            }
        }
    }

    log::trace!("Result tracking complete. Payment result tracker exiting.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{
        get_payment_delay, test_utils, test_utils::LightningTestNodeBuilder, LightningError,
        LightningNode, MutRng, PaymentGenerationError, PaymentGenerator,
    };
    use bitcoin::secp256k1::PublicKey;
    use bitcoin::Network;
    use mockall::mock;
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    #[test]
    fn create_seeded_mut_rng() {
        let seeds = vec![u64::MIN, u64::MAX];

        for seed in seeds {
            let mut_rng_1 = MutRng::new(Some((seed, None)));
            let mut_rng_2 = MutRng::new(Some((seed, None)));

            let mut rng_1 = mut_rng_1.0.lock().unwrap();
            let mut rng_2 = mut_rng_2.0.lock().unwrap();

            assert_eq!(rng_1.next_u64(), rng_2.next_u64())
        }
    }

    #[test]
    fn create_unseeded_mut_rng() {
        let mut_rng_1 = MutRng::new(None);
        let mut_rng_2 = MutRng::new(None);

        let mut rng_1 = mut_rng_1.0.lock().unwrap();
        let mut rng_2 = mut_rng_2.0.lock().unwrap();

        assert_ne!(rng_1.next_u64(), rng_2.next_u64())
    }

    #[test]
    fn create_salted_mut_rng() {
        let (_, pk1) = test_utils::get_random_keypair();
        let (_, pk2) = test_utils::get_random_keypair();

        let salted_rng_1 = MutRng::new(Some((42, Some(&pk1))));
        let salted_rng_2 = MutRng::new(Some((42, Some(&pk2))));

        let mut seq1 = Vec::new();
        let mut seq2 = Vec::new();

        let mut rng1 = salted_rng_1.0.lock().unwrap();
        let mut rng2 = salted_rng_2.0.lock().unwrap();

        for _ in 0..10 {
            seq1.push(rng1.next_u64());
            seq2.push(rng2.next_u64());
        }

        assert_ne!(seq1, seq2);

        let salted_rng1_again = MutRng::new(Some((42, Some(&pk1))));
        let mut rng1_again = salted_rng1_again.0.lock().unwrap();
        let mut seq1_again = Vec::new();

        for _ in 0..10 {
            seq1_again.push(rng1_again.next_u64());
        }

        assert_eq!(seq1, seq1_again);
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

    #[test]
    fn test_no_payment_delay() {
        let node = test_utils::create_nodes(1, 100_000)
            .first()
            .unwrap()
            .0
            .clone();

        // Setup mocked generator to have no start time and send payments every 5 seconds.
        let mut mock_generator = MockGenerator::new();
        mock_generator.expect_payment_start().return_once(|| None);
        let payment_interval = Duration::from_secs(5);
        mock_generator
            .expect_next_payment_wait()
            .returning(move || Ok(payment_interval));

        assert_eq!(
            get_payment_delay(0, &node, &mock_generator).unwrap(),
            payment_interval
        );
        assert_eq!(
            get_payment_delay(1, &node, &mock_generator).unwrap(),
            payment_interval
        );
    }

    #[test]
    fn test_payment_delay() {
        let node = test_utils::create_nodes(1, 100_000)
            .first()
            .unwrap()
            .0
            .clone();

        // Setup mocked generator to have a start delay and payment interval with different values.
        let mut mock_generator = MockGenerator::new();
        let start_delay = Duration::from_secs(10);
        mock_generator
            .expect_payment_start()
            .return_once(move || Some(start_delay));
        let payment_interval = Duration::from_secs(5);
        mock_generator
            .expect_next_payment_wait()
            .returning(move || Ok(payment_interval));

        assert_eq!(
            get_payment_delay(0, &node, &mock_generator).unwrap(),
            start_delay
        );
        assert_eq!(
            get_payment_delay(1, &node, &mock_generator).unwrap(),
            payment_interval
        );
    }

    /// Verifies that an empty activity (for random generation) is valid when two nodes with keysend
    /// support are present, expecting an `Ok` result.
    #[tokio::test]
    async fn test_validate_activity_empty_with_sufficient_nodes() {
        let (_, clients) = LightningTestNodeBuilder::new(3).build_full();
        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_activity(&[]).await;
        assert!(result.is_ok());
    }

    /// Verifies that an empty activity fails validation with only one keysend-enabled node,
    /// expecting a `ValidationError` with "At least two nodes required".
    #[tokio::test]
    async fn test_validate_activity_empty_with_insufficient_nodes() {
        let (_, clients) = LightningTestNodeBuilder::new(1).build_full();
        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_activity(&[]).await;

        assert!(result.is_err());
        assert!(
            matches!(result, Err(LightningError::ValidationError(msg)) if msg.contains("At least two nodes required"))
        );
    }

    /// Verifies that an empty activity fails when one of two nodes doesn’t support keysend,
    /// expecting a `ValidationError` with "must support keysend".
    #[tokio::test]
    async fn test_validate_activity_empty_with_non_keysend_node() {
        let (_, clients) = LightningTestNodeBuilder::new(2)
            .with_keysend_nodes(vec![0])
            .build_full();
        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_activity(&[]).await;

        assert!(result.is_err());
        assert!(
            matches!(result, Err(LightningError::ValidationError(msg)) if msg.contains("must support keysend"))
        );
    }

    /// Verifies that an activity fails when the source node isn’t in the clients map,
    /// expecting a `ValidationError` with "Source node not found".
    #[tokio::test]
    async fn test_validate_activity_with_missing_source_node() {
        let (nodes, clients) = LightningTestNodeBuilder::new(1).build_full();
        let missing_nodes = test_utils::create_nodes(1, 100_000);
        let missing_node = missing_nodes.first().unwrap().0.clone();
        let dest_node = nodes[0].clone();

        let activity = test_utils::create_activity(missing_node, dest_node, 1000);
        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_activity(&vec![activity]).await;

        assert!(result.is_err());
        assert!(
            matches!(result, Err(LightningError::ValidationError(msg)) if msg.contains("Source node not found"))
        );
    }

    /// Verifies that an activity fails when the destination lacks keysend support,
    /// expecting a `ValidationError` with "does not support keysend".
    #[tokio::test]
    async fn test_validate_activity_with_non_keysend_destination() {
        let (nodes, clients) = LightningTestNodeBuilder::new(1).build_full();
        let dest_nodes = test_utils::create_nodes(1, 100_000);
        let dest_node = dest_nodes.first().unwrap().0.clone();

        let activity = test_utils::create_activity(nodes[0].clone(), dest_node, 1000);
        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_activity(&vec![activity]).await;

        assert!(result.is_err());
        assert!(
            matches!(result, Err(LightningError::ValidationError(msg)) if msg.contains("does not support keysend"))
        );
    }

    /// Verifies that an activity with a non-zero amount between two keysend-enabled nodes
    /// passes validation, expecting an `Ok` result.
    #[tokio::test]
    async fn test_validate_activity_valid_payment_flow() {
        let (nodes, clients) = LightningTestNodeBuilder::new(1).build_full();
        let dest_nodes = test_utils::create_nodes(1, 100_000);
        let mut dest_node = dest_nodes.first().unwrap().0.clone();
        dest_node.features.set_keysend_optional();

        let activity = test_utils::create_activity(nodes[0].clone(), dest_node, 1000);
        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_activity(&vec![activity]).await;

        assert!(result.is_ok());
    }

    /// Verifies that an activity with a zero amount between two keysend-enabled nodes fails,
    /// expecting a `ValidationError` with "zero values".
    #[tokio::test]
    async fn test_validate_zero_amount_no_valid() {
        let (nodes, clients) = LightningTestNodeBuilder::new(2).build_full();

        let activity = test_utils::create_activity(nodes[0].clone(), nodes[1].clone(), 0);
        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_activity(&vec![activity]).await;

        assert!(result.is_err());
        assert!(
            matches!(result, Err(LightningError::ValidationError(msg)) if msg.contains("zero values"))
        );
    }

    /// Verifies that validation fails with no nodes, expecting a `ValidationError` with
    /// "we don't control any nodes".
    #[tokio::test]
    async fn test_validate_node_network_empty_nodes() {
        let empty_nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> = HashMap::new();

        let simulation = test_utils::create_simulation(empty_nodes);
        let result = simulation.validate_node_network().await;

        assert!(result.is_err());
        assert!(
            matches!(result, Err(LightningError::ValidationError(msg)) if msg.contains("we don't control any nodes"))
        );
    }

    /// Verifies that a node on Bitcoin mainnet fails validation, expecting a `ValidationError`
    /// with "mainnet is not supported".
    #[tokio::test]
    async fn test_validate_node_network_mainnet_not_supported() {
        let clients = LightningTestNodeBuilder::new(1)
            .with_networks(vec![Network::Bitcoin])
            .build_clients_only();

        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_node_network().await;

        assert!(result.is_err());
        assert!(
            matches!(result, Err(LightningError::ValidationError(msg)) if msg.contains("mainnet is not supported"))
        );
    }

    /// Verifies that nodes on Testnet and Regtest fail validation, expecting a
    /// `ValidationError` with "nodes are not on the same network".
    #[tokio::test]
    async fn test_validate_node_network_mixed_networks() {
        let clients = LightningTestNodeBuilder::new(2)
            .with_networks(vec![Network::Testnet, Network::Regtest])
            .build_clients_only();

        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_node_network().await;

        assert!(result.is_err());
        assert!(
            matches!(result, Err(LightningError::ValidationError(msg)) if msg.contains("nodes are not on the same network"))
        );
    }

    /// Verifies that three Testnet nodes pass validation, expecting an `Ok` result.
    #[tokio::test]
    async fn test_validate_node_network_multiple_nodes_same_network() {
        let clients = LightningTestNodeBuilder::new(3)
            .with_networks(vec![Network::Testnet, Network::Testnet, Network::Testnet])
            .build_clients_only();

        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_node_network().await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_validate_node_network_single_node_valid_network() {
        let clients = LightningTestNodeBuilder::new(1)
            .with_networks(vec![Network::Testnet])
            .build_clients_only();

        let simulation = test_utils::create_simulation(clients);
        let result = simulation.validate_node_network().await;

        assert!(result.is_ok());
    }
}
