use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use csv::WriterBuilder;
use lightning::ln::features::NodeFeatures;
use lightning::ln::PaymentHash;
use random_activity::RandomActivityError;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::marker::Send;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;
use std::{collections::HashMap, sync::Arc, time::SystemTime};
use thiserror::Error;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::{select, time, time::Duration};
use triggered::{Listener, Trigger};

use self::defined_activity::DefinedPaymentActivity;
use self::random_activity::{NetworkGraphView, RandomPaymentActivity};

pub mod cln;
mod defined_activity;
pub mod lnd;
mod random_activity;
mod serializers;
pub mod sim_node;
#[cfg(test)]
mod test_utils;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum NodeConnection {
    LND(lnd::LndConnection),
    CLN(cln::ClnConnection),
}

#[derive(Serialize, Debug, Clone)]
pub enum NodeId {
    PublicKey(PublicKey),
    Alias(String),
}

impl NodeId {
    pub fn validate(&self, node_id: &PublicKey, alias: &mut String) -> Result<(), LightningError> {
        match self {
            crate::NodeId::PublicKey(pk) => {
                if pk != node_id {
                    return Err(LightningError::ValidationError(format!(
                    "the provided node id does not match the one returned by the backend ({} != {}).", pk, node_id)));
                }
            },
            crate::NodeId::Alias(a) => {
                if alias != a {
                    log::warn!("The provided alias does not match the one returned by the backend ({} != {}).", a, alias)
                }
                *alias = a.to_string();
            },
        }
        Ok(())
    }

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
pub struct ShortChannelID(u64);

/// Utility function to easily convert from u64 to `ShortChannelID`
impl From<u64> for ShortChannelID {
    fn from(value: u64) -> Self {
        ShortChannelID(value)
    }
}

/// Utility function to easily convert `ShortChannelID` into u64
impl From<ShortChannelID> for u64 {
    fn from(scid: ShortChannelID) -> Self {
        scid.0
    }
}

/// See https://github.com/lightning/bolts/blob/60de4a09727c20dea330f9ee8313034de6e50594/07-routing-gossip.md#definition-of-short_channel_id.
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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SimParams {
    pub nodes: Vec<NodeConnection>,
    #[serde(default)]
    pub activity: Vec<ActivityParser>,
}

/// Data structure used to parse information from the simulation file. It allows source and destination to be
/// [NodeId], which enables the use of public keys and aliases in the simulation description.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityParser {
    /// The source of the payment.
    #[serde(with = "serializers::serde_node_id")]
    pub source: NodeId,
    /// The destination of the payment.
    #[serde(with = "serializers::serde_node_id")]
    pub destination: NodeId,
    /// The interval of the event, as in every how many seconds the payment is performed.
    pub interval_secs: u16,
    /// The amount of m_sat to used in this payment.
    pub amount_msat: u64,
}

/// Data structure used internally by the simulator. Both source and destination are represented as [PublicKey] here.
/// This is constructed during activity validation and passed along to the [Simulation].
#[derive(Debug, Clone)]
pub struct ActivityDefinition {
    /// The source of the payment.
    pub source: NodeInfo,
    /// The destination of the payment.
    pub destination: NodeInfo,
    /// The interval of the event, as in every how many seconds the payment is performed.
    pub interval_secs: u16,
    /// The amount of m_sat to used in this payment.
    pub amount_msat: u64,
}

#[derive(Debug, Error)]
pub enum SimulationError {
    #[error("Lightning Error: {0:?}")]
    LightningError(#[from] LightningError),
    #[error("TaskError")]
    TaskError,
    #[error("CSV Error: {0:?}")]
    CsvError(#[from] csv::Error),
    #[error("File Error")]
    FileError,
    #[error("{0}")]
    RandomActivityError(RandomActivityError),
    #[error("Simulated Network Error: {0}")]
    SimulatedNetworkError(String),
}

#[derive(Debug, Error)]
pub enum LightningError {
    #[error("Node connection error: {0}")]
    ConnectionError(String),
    #[error("Get info error: {0}")]
    GetInfoError(String),
    #[error("Send payment error: {0}")]
    SendPaymentError(String),
    #[error("Track payment error: {0}")]
    TrackPaymentError(String),
    #[error("Invalid payment hash")]
    InvalidPaymentHash,
    #[error("Get node info error: {0}")]
    GetNodeInfoError(String),
    #[error("Config validation failed: {0}")]
    ValidationError(String),
    #[error("Permanent error: {0:?}")]
    PermanentError(String),
    #[error("List channels error: {0}")]
    ListChannelsError(String),
}

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub pubkey: PublicKey,
    pub alias: String,
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

/// LightningNode represents the functionality that is required to execute events on a lightning node.
#[async_trait]
pub trait LightningNode: Send {
    /// Get information about the node.
    fn get_info(&self) -> &NodeInfo;
    /// Get the network this node is running at
    async fn get_network(&mut self) -> Result<Network, LightningError>;
    /// Keysend payment worth `amount_msat` from a source node to the destination node.
    async fn send_payment(
        &mut self,
        dest: PublicKey,
        amount_msat: u64,
    ) -> Result<PaymentHash, LightningError>;
    /// Track a payment with the specified hash.
    async fn track_payment(
        &mut self,
        hash: PaymentHash,
        shutdown: Listener,
    ) -> Result<PaymentResult, LightningError>;
    /// Gets information on a specific node
    async fn get_node_info(&mut self, node_id: &PublicKey) -> Result<NodeInfo, LightningError>;
    /// Lists all channels, at present only returns a vector of channel capacities in msat because no further
    /// information is required.
    async fn list_channels(&mut self) -> Result<Vec<u64>, LightningError>;
}

pub trait DestinationGenerator: Send {
    /// choose_destination picks a destination node within the network, returning the node's information and its
    /// capacity (if available).
    fn choose_destination(&self, source: PublicKey) -> (NodeInfo, Option<u64>);
}

#[derive(Debug, Error)]
#[error("Payment generation error: {0}")]
pub struct PaymentGenerationError(String);

pub trait PaymentGenerator: Display + Send {
    /// Returns the number of seconds that a node should wait until firing its next payment.
    fn next_payment_wait(&self) -> time::Duration;

    /// Returns a payment amount based, with a destination capacity optionally provided to inform the amount picked.
    fn payment_amount(
        &self,
        destination_capacity: Option<u64>,
    ) -> Result<u64, PaymentGenerationError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentResult {
    pub htlc_count: usize,
    pub payment_outcome: PaymentOutcome,
}

impl PaymentResult {
    pub fn not_dispatched() -> Self {
        PaymentResult {
            htlc_count: 0,
            payment_outcome: PaymentOutcome::NotDispatched,
        }
    }

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaymentOutcome {
    Success,
    RecipientRejected,
    UserAbandoned,
    RetriesExhausted,
    PaymentExpired,
    RouteNotFound,
    UnexpectedError,
    IncorrectPaymentDetails,
    InsufficientBalance,
    Unknown,
    NotDispatched,
    TrackPaymentFailed,
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
        write!(
            f,
            "Payment {} dispatched at {:?} sending {} msat from {} -> {}",
            self.hash.map(|h| hex::encode(h.0)).unwrap_or_default(),
            self.dispatch_time.duration_since(UNIX_EPOCH).unwrap(),
            self.amount_msat,
            self.source,
            self.destination,
        )
    }
}

/// SimulationEvent describes the set of actions that the simulator can run on nodes that it has execution permissions
/// on.
#[derive(Clone)]
enum SimulationEvent {
    /// Dispatch a payment of the specified amount to the public key provided.
    /// Results in `SimulationOutput::SendPaymentSuccess` or `SimulationOutput::SendPaymentFailure`.
    SendPayment(NodeInfo, u64),
}

/// SimulationOutput provides the output of a simulation event.
enum SimulationOutput {
    /// Intermediate output for when simulator has successfully dispatched a payment.
    /// We need to track the result of the payment to report on it.
    SendPaymentSuccess(Payment),
    /// Final output for when simulator has failed to dispatch a payment.
    /// Report this as the final result of simulation event.
    SendPaymentFailure(Payment, PaymentResult),
}

#[derive(Clone)]
pub struct Simulation {
    /// The lightning node that is being simulated.
    nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
    /// The activity that are to be executed on the node.
    activity: Vec<ActivityDefinition>,
    /// High level triggers used to manage simulation tasks and shutdown.
    shutdown_trigger: Trigger,
    shutdown_listener: Listener,
    /// Total simulation time. The simulation will run forever if undefined.
    total_time: Option<time::Duration>,
    /// The expected payment size for the network.
    expected_payment_msat: u64,
    /// The number of times that the network sends its total capacity in a month of operation when generating random
    /// activity.
    activity_multiplier: f64,
    /// Configurations for printing results to CSV. Results are not written if this option is None.
    write_results: Option<WriteResults>,
}

#[derive(Clone)]
pub struct WriteResults {
    /// Data directory where CSV result files are written.
    pub results_dir: PathBuf,
    /// The number of activity results to batch before printing in CSV.
    pub batch_size: u32,
}
///
/// ExecutorKit contains the components required to spin up an activity configured by the user, to be used to
/// spin up the appropriate producers and consumers for the activity.
struct ExecutorKit {
    source_info: NodeInfo,
    /// We use an arc mutex here because some implementations of the trait will be very expensive to clone.
    /// See [NetworkGraphView] for details.
    network_generator: Arc<Mutex<dyn DestinationGenerator>>,
    payment_generator: Box<dyn PaymentGenerator>,
}

impl Simulation {
    pub fn new(
        nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
        activity: Vec<ActivityDefinition>,
        total_time: Option<u32>,
        expected_payment_msat: u64,
        activity_multiplier: f64,
        write_results: Option<WriteResults>,
    ) -> Self {
        let (shutdown_trigger, shutdown_listener) = triggered::trigger();
        Self {
            nodes,
            activity,
            shutdown_trigger,
            shutdown_listener,
            total_time: total_time.map(|x| Duration::from_secs(x as u64)),
            expected_payment_msat,
            activity_multiplier,
            write_results,
        }
    }

    /// validate_activity validates that the user-provided activity description is achievable for the network that
    /// we're working with. If no activity description is provided, then it ensures that we have configured a network
    /// that is suitable for random activity generation.
    async fn validate_activity(&self) -> Result<(), LightningError> {
        // For now, empty activity signals random activity generation
        if self.activity.is_empty() {
            if self.nodes.len() <= 1 {
                return Err(LightningError::ValidationError(
                    "At least two nodes required for random activity generation.".to_string(),
                ));
            } else {
                for node in self.nodes.values() {
                    let node = node.lock().await;
                    if !node.get_info().features.supports_keysend() {
                        return Err(LightningError::ValidationError(format!("All nodes eligible for random activity generation must support keysend, {} does not", node.get_info())));
                    }
                }
            }
        }

        for payment_flow in self.activity.iter() {
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
            let network = node.lock().await.get_network().await?;
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

        log::info!("Simulation is running on {}.", running_network.unwrap());

        Ok(())
    }

    pub async fn run(&self) -> Result<(), SimulationError> {
        if let Some(total_time) = self.total_time {
            log::info!("Running the simulation for {}s.", total_time.as_secs());
        } else {
            log::info!("Running the simulation forever.");
        }

        self.validate_node_network().await?;
        self.validate_activity().await?;

        log::info!(
            "Simulating {} activity on {} nodes.",
            self.activity.len(),
            self.nodes.len()
        );
        let mut tasks = JoinSet::new();

        // Before we start the simulation up, start tasks that will be responsible for gathering simulation data.
        // The event channels are shared across our functionality:
        // - Event Sender: used by the simulation to inform data reporting that it needs to start tracking the
        //   final result of the event that it has taken.
        // - Event Receiver: used by data reporting to receive events that have been simulated that need to be
        //   tracked and recorded.
        let (event_sender, event_receiver) = channel(1);
        self.run_data_collection(event_receiver, &mut tasks);

        // Get an execution kit per activity that we need to generate and spin up consumers for each source node.
        let activities = self.activity_executors().await?;
        let consumer_channels = self.dispatch_consumers(
            activities
                .iter()
                .map(|generator| generator.source_info.pubkey)
                .collect(),
            event_sender.clone(),
            &mut tasks,
        );

        // Next, we'll spin up our actual producers that will be responsible for triggering the configured activity.
        self.dispatch_producers(activities, consumer_channels, &mut tasks)
            .await?;

        if let Some(total_time) = self.total_time {
            let t = self.shutdown_trigger.clone();
            let l = self.shutdown_listener.clone();

            tasks.spawn(async move {
                if time::timeout(total_time, l).await.is_err() {
                    log::info!(
                        "Simulation run for {}s. Shutting down.",
                        total_time.as_secs()
                    );
                    t.trigger()
                }
            });
        }

        // We always want to wait ofr all threads to exit, so we wait for all of them to exit and track any errors
        // that surface. It's okay if there are multiple and one is overwritten, we just want to know whether we
        // exited with an error or not.
        let mut success = true;
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                log::error!("Task exited with error: {e}.");
                success = false;
            }
        }

        success.then_some(()).ok_or(SimulationError::TaskError)
    }

    pub fn shutdown(&self) {
        self.shutdown_trigger.trigger()
    }

    /// run_data_collection starts the tasks required for the simulation to report of the results of the activity that
    /// it generates. The simulation should report outputs via the receiver that is passed in.
    fn run_data_collection(
        &self,
        output_receiver: Receiver<SimulationOutput>,
        tasks: &mut JoinSet<()>,
    ) {
        let listener = self.shutdown_listener.clone();
        log::debug!("Setting up simulator data collection.");

        // Create a sender/receiver pair that will be used to report final results of simulation.
        let (results_sender, results_receiver) = channel(1);

        tasks.spawn(produce_simulation_results(
            self.nodes.clone(),
            output_receiver,
            results_sender,
            listener.clone(),
        ));

        let result_logger = Arc::new(Mutex::new(PaymentResultLogger::new()));

        tasks.spawn(run_results_logger(
            listener.clone(),
            result_logger.clone(),
            Duration::from_secs(60),
        ));

        tasks.spawn(consume_simulation_results(
            result_logger,
            results_receiver,
            listener,
            self.write_results.clone(),
        ));
        log::debug!("Simulator data collection set up.");
    }

    async fn activity_executors(&self) -> Result<Vec<ExecutorKit>, SimulationError> {
        let mut generators = Vec::new();

        // Note: when we allow configuring both defined and random activity, this will no longer be an if/else, we'll
        // just populate with each type as configured.
        if !self.activity.is_empty() {
            for description in self.activity.iter() {
                let activity_generator = DefinedPaymentActivity::new(
                    description.destination.clone(),
                    Duration::from_secs(description.interval_secs.into()),
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

            if let Err(e) =
                RandomPaymentActivity::validate_capacity(chan_capacity, self.expected_payment_msat)
            {
                log::warn!("Node: {} not eligible for activity generation: {e}.", *pk);
                continue;
            }

            // Don't double count channel capacity because each channel reports the total balance between counter
            // parities. Track capacity separately to be used for our network generator.
            let capacity = chan_capacity / 2;
            let node_info = node.lock().await.get_node_info(pk).await?;
            active_nodes.insert(node_info.pubkey, (node_info, capacity));
        }

        let network_generator = Arc::new(Mutex::new(
            NetworkGraphView::new(active_nodes.values().cloned().collect())
                .map_err(SimulationError::RandomActivityError)?,
        ));

        log::info!(
            "Created network generator: {}.",
            network_generator.lock().await
        );

        for (node_info, capacity) in active_nodes.values() {
            generators.push(ExecutorKit {
                source_info: node_info.clone(),
                network_generator: network_generator.clone(),
                payment_generator: Box::new(
                    RandomPaymentActivity::new(
                        *capacity,
                        self.expected_payment_msat,
                        self.activity_multiplier,
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
        tasks: &mut JoinSet<()>,
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
            tasks.spawn(consume_events(
                node.clone(),
                receiver,
                output_sender.clone(),
                self.shutdown_trigger.clone(),
            ));
        }

        channels
    }

    /// Responsible for spinning up producers for a set of activities. Requires that a consumer channel is present
    /// for every source node in the set of executors.
    async fn dispatch_producers(
        &self,
        executors: Vec<ExecutorKit>,
        producer_channels: HashMap<PublicKey, Sender<SimulationEvent>>,
        tasks: &mut JoinSet<()>,
    ) -> Result<(), SimulationError> {
        for executor in executors {
            let sender = producer_channels.get(&executor.source_info.pubkey).ok_or(
                SimulationError::RandomActivityError(RandomActivityError::ValueError(format!(
                    "Activity producer for: {} not found.",
                    executor.source_info.pubkey,
                ))),
            )?;

            tasks.spawn(produce_events(
                executor.source_info,
                executor.network_generator,
                executor.payment_generator,
                sender.clone(),
                self.shutdown_trigger.clone(),
                self.shutdown_listener.clone(),
            ));
        }

        Ok(())
    }
}

/// events that are crated for a lightning node that we can execute events on. Any output that is generated from the
/// event being executed is piped into a channel to handle the result of the event. If it exits, it will use the
/// trigger provided to trigger shutdown in other threads. If an error occurs elsewhere, we expect the senders
/// corresponding to our receiver to be dropped, which will cause the receiver to error out and
/// exit.
async fn consume_events(
    node: Arc<Mutex<dyn LightningNode>>,
    mut receiver: Receiver<SimulationEvent>,
    sender: Sender<SimulationOutput>,
    shutdown: Trigger,
) {
    log::debug!("Started consumer for {}.", node.lock().await.get_info());

    while let Some(event) = receiver.recv().await {
        match event {
            SimulationEvent::SendPayment(dest, amt_msat) => {
                let mut node = node.lock().await;

                let mut payment = Payment {
                    source: node.get_info().pubkey,
                    hash: None,
                    amount_msat: amt_msat,
                    destination: dest.pubkey,
                    dispatch_time: SystemTime::now(),
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
                    },
                    Err(e) => {
                        log::error!(
                            "Error while sending payment {} -> {}.",
                            node.get_info(),
                            dest
                        );

                        match e {
                            LightningError::PermanentError(s) => {
                                log::error!("Simulation terminated with error: {s}.");
                                shutdown.trigger();
                                break;
                            },
                            _ => SimulationOutput::SendPaymentFailure(
                                payment,
                                PaymentResult::not_dispatched(),
                            ),
                        }
                    },
                };

                match sender.send(outcome).await {
                    Ok(_) => {},
                    Err(e) => {
                        log::error!("Error sending action outcome: {:?}.", e);
                        break;
                    },
                }
            },
        };
    }
}

/// produce events generates events for the activity description provided. It accepts a shutdown listener so it can
/// exit if other threads signal that they have errored out.
async fn produce_events<N: DestinationGenerator + ?Sized, A: PaymentGenerator + ?Sized>(
    source: NodeInfo,
    network_generator: Arc<Mutex<N>>,
    node_generator: Box<A>,
    sender: Sender<SimulationEvent>,
    shutdown: Trigger,
    listener: Listener,
) {
    log::info!("Started activity producer for {source}: {node_generator}.");

    loop {
        let wait = node_generator.next_payment_wait();
        log::debug!("Next payment for {source} in {:?} seconds.", wait);

        select! {
            biased;
            _ = listener.clone() => {
                log::debug!("Random activity generator for {source} received signal to shut down.");
                break;
            },
            // Wait until our time to next payment has elapsed then execute a random amount payment to a random
            // destination.
            _ = time::sleep(wait) => {
                let (destination, capacity) = network_generator.lock().await.choose_destination(source.pubkey);

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
                        log::error!("Could not get amount for {source} -> {destination}: {e}. Please report a bug!");
                        break;
                    },
                };

                log::debug!("Generated random payment: {source} -> {}: {amount} msat.", destination);

                // Send the payment, exiting if we can no longer send to the consumer.
                let event = SimulationEvent::SendPayment(destination.clone(), amount);
                if let Err(e) = sender.send(event).await {
                    log::debug!(
                        "Stopped random producer for {amount}: {source} -> {destination}. Consumer error: {e}.",
                    );
                    break;
                }
            },
        }
    }

    log::debug!("Stopped random activity producer {source}.");
    shutdown.trigger();
}

async fn consume_simulation_results(
    logger: Arc<Mutex<PaymentResultLogger>>,
    receiver: Receiver<(Payment, PaymentResult)>,
    listener: Listener,
    write_results: Option<WriteResults>,
) {
    log::debug!("Simulation results consumer started.");

    if let Err(e) = write_payment_results(logger, receiver, listener, write_results).await {
        log::error!("Error while reporting payment results: {:?}.", e);
    }

    log::debug!("Simulation results consumer exiting.");
}

async fn write_payment_results(
    logger: Arc<Mutex<PaymentResultLogger>>,
    mut receiver: Receiver<(Payment, PaymentResult)>,
    listener: Listener,
    write_results: Option<WriteResults>,
) -> Result<(), SimulationError> {
    let mut writer = write_results.and_then(|write_result| {
        let file = write_result.results_dir.join(format!(
            "simulation_{:?}.csv",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        ));
        let writer = WriterBuilder::new().from_path(file).ok()?;
        Some((writer, write_result.batch_size))
    });

    let mut counter = 1;
    loop {
        tokio::select! {
            biased;
            _ = listener.clone() => {
                log::debug!("Simulation results consumer received shutdown signal.");
                return writer.map_or(Ok(()), |(ref mut w, _)| w.flush().map_err(|_| SimulationError::FileError))
            },
            payment_report = receiver.recv() => {
                match payment_report {
                    Some((details, result)) => {
                        logger.lock().await.report_result(&details, &result);
                        log::trace!("Resolved dispatched payment: {} with: {}.", details, result);

                        if let Some((ref mut w, batch_size)) = writer {
                            w.serialize((details, result)).map_err(|e| {
                                let _ = w.flush();
                                SimulationError::CsvError(e)
                            })?;
                            counter = counter % batch_size + 1;
                            if batch_size == counter {
                                w.flush().map_err(|_| SimulationError::FileError)?;
                            }
                        }
                    },
                    None => return writer.map_or(Ok(()), |(ref mut w, _)| w.flush().map_err(|_| SimulationError::FileError)),
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
}

impl Display for PaymentResultLogger {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let total_payments = self.success_payment + self.failed_payment;
        write!(
            f,
            "Processed {} payments sending {} msat total with {:.2}% success rate.",
            total_payments,
            self.total_sent,
            (self.success_payment as f64 / total_payments as f64) * 100.0
        )
    }
}

async fn run_results_logger(
    listener: Listener,
    logger: Arc<Mutex<PaymentResultLogger>>,
    interval: Duration,
) {
    log::debug!("Results logger started.");
    log::info!("Summary of results will be reported every {:?}.", interval);

    loop {
        select! {
            biased;
            _ = listener.clone() => {
                break
            }

            _ = time::sleep(interval) => {
                log::info!("{}", logger.lock().await)
            }
        }
    }

    log::debug!("Results logger stopped.")
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
    shutdown: Listener,
) {
    log::debug!("Simulation results producer started.");

    let mut set = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.clone() => break,
            output = output_receiver.recv() => {
                match output{
                    Some(simulation_output) => {
                        match simulation_output{
                            SimulationOutput::SendPaymentSuccess(payment) => {
                                let source_node = nodes.get(&payment.source).unwrap().clone();
                                set.spawn(track_payment_result(
                                    source_node, results.clone(), payment, shutdown.clone(),
                                ));
                            },
                            SimulationOutput::SendPaymentFailure(payment, result) => {
                                if results.clone().send((payment, result)).await.is_err() {
                                    log::debug!("Could not send payment result.");
                                }
                            }
                        };

                    },
                    None => {
                        return
                    }
                }
            }
        }
    }

    log::debug!("Simulation results producer exiting.");
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            log::error!("Simulation results producer task exited with error: {e}.");
        }
    }
}

async fn track_payment_result(
    node: Arc<Mutex<dyn LightningNode>>,
    results: Sender<(Payment, PaymentResult)>,
    payment: Payment,
    shutdown: Listener,
) {
    log::trace!("Payment result tracker starting.");

    let mut node = node.lock().await;

    let res = match payment.hash {
        Some(hash) => {
            log::debug!("Tracking payment outcome for: {}.", hex::encode(hash.0));
            let track_payment = node.track_payment(hash, shutdown.clone());

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

    if results.clone().send((payment, res)).await.is_err() {
        log::debug!("Could not send payment result.");
    }

    log::trace!("Payment result tracker exiting.");
}
