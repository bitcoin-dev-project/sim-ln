use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use csv::WriterBuilder;
use lightning::ln::features::NodeFeatures;
use lightning::ln::PaymentHash;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::marker::Send;
use std::time::UNIX_EPOCH;
use std::{collections::HashMap, sync::Arc, time::SystemTime};
use thiserror::Error;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::{select, time, time::Duration};
use triggered::{Listener, Trigger};

use self::random_activity::{NetworkGraphView, PaymentActivityGenerator};

pub mod cln;
pub mod lnd;
mod random_activity;
mod serializers;

/// The default expected payment amount for the simulation, around ~$10 at the time of writing.
pub const EXPECTED_PAYMENT_AMOUNT: u64 = 3_800_000;

/// The number of times over each node in the network sends its total deployed capacity in a calendar month.
pub const ACTIVITY_MULTIPLIER: f64 = 2.0;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum NodeConnection {
    #[serde(alias = "lnd", alias = "Lnd")]
    LND(LndConnection),
    #[serde(alias = "cln", alias = "Cln")]
    CLN(ClnConnection),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LndConnection {
    pub id: PublicKey,
    pub address: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub macaroon: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub cert: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClnConnection {
    pub id: PublicKey,
    pub address: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub ca_cert: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub client_cert: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub client_key: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub nodes: Vec<NodeConnection>,
    pub activity: Option<Vec<ActivityDefinition>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ActivityDefinition {
    // The source of the payment.
    pub source: PublicKey,
    // The destination of the payment.
    pub destination: PublicKey,
    // The interval of the event, as in every how many seconds the payment is performed.
    #[serde(alias = "interval_secs")]
    pub interval: u16,
    // The amount of m_sat to used in this payment.
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
    #[error("Random activity Error: {0}")]
    RandomActivityError(String),
}

// Phase 2: Event Queue
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
    pub network: String,
}

/// LightningNode represents the functionality that is required to execute events on a lightning node.
#[async_trait]
pub trait LightningNode {
    /// Get information about the node.
    fn get_info(&self) -> &NodeInfo;
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
    /// Gets the list of features of a given node
    async fn get_node_features(&mut self, node: PublicKey) -> Result<NodeFeatures, LightningError>;
    /// Lists all channels, at present only returns a vector of channel capacities in msat because no further
    /// information is required.
    async fn list_channels(&mut self) -> Result<Vec<u64>, LightningError>;
}

pub trait NetworkGenerator {
    // sample_node_by_capacity randomly picks a node within the network weighted by its capacity deployed to the
    // network in channels. It returns the node's public key and its capacity in millisatoshis.
    fn sample_node_by_capacity(&self, source: PublicKey) -> (PublicKey, u64);
}

pub trait PaymentGenerator {
    // Returns the number of seconds that a node should wait until firing its next payment.
    fn next_payment_wait(&self) -> time::Duration;

    // Returns a payment amount based on the capacity of the sending and receiving node.
    fn payment_amount(&self, destination_capacity: u64) -> Result<u64, SimulationError>;
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
            self.hash.map(|h| hex::encode(h.0)).unwrap_or(String::new()),
            self.dispatch_time.duration_since(UNIX_EPOCH).unwrap(),
            self.amount_msat,
            self.source,
            self.destination,
        )
    }
}

/// SimulationEvent describes the set of actions that the simulator can run on nodes that it has execution permissions
/// on.
#[derive(Clone, Copy)]
enum SimulationEvent {
    /// Dispatch a payment of the specified amount to the public key provided.
    /// Results in `SimulationOutput::SendPaymentSuccess` or `SimulationOutput::SendPaymentFailure`.
    SendPayment(PublicKey, u64),
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
    // The lightning node that is being simulated.
    nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>>,
    // The activity that are to be executed on the node.
    activity: Vec<ActivityDefinition>,
    // High level triggers used to manage simulation tasks and shutdown.
    shutdown_trigger: Trigger,
    shutdown_listener: Listener,
    // Total simulation time. The simulation will run forever if undefined.
    total_time: Option<time::Duration>,
    /// The number of activity results to batch before printing in CSV.
    print_batch_size: u32,
    /// The expected payment size for the network.
    expected_payment_msat: u64,
    /// The number of times that the network sends its total capacity in a month of operation when generating random
    /// activity.
    activity_multiplier: f64,
}

const DEFAULT_PRINT_BATCH_SIZE: u32 = 500;

impl Simulation {
    pub fn new(
        nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>>,
        activity: Option<Vec<ActivityDefinition>>,
        total_time: Option<u32>,
        print_batch_size: Option<u32>,
    ) -> Self {
        let (shutdown_trigger, shutdown_listener) = triggered::trigger();
        Self {
            nodes,
            activity: activity.unwrap_or_default(),
            shutdown_trigger,
            shutdown_listener,
            total_time: total_time.map(|x| Duration::from_secs(x as u64)),
            print_batch_size: print_batch_size.unwrap_or(DEFAULT_PRINT_BATCH_SIZE),
            expected_payment_msat: EXPECTED_PAYMENT_AMOUNT,
            activity_multiplier: ACTIVITY_MULTIPLIER,
        }
    }

    /// validate_activity validates that the user-provided activity description is achievable for the network that
    /// we're working with. If no activity description is provided, then it ensures that we have configured a network
    /// that is suitable for random activity generation.
    async fn validate_activity(&self) -> Result<(), LightningError> {
        if self.activity.is_empty() && self.nodes.len() <= 1 {
            return Err(LightningError::ValidationError(
                "At least two nodes required for random activity generation.".to_string(),
            ));
        }

        for payment_flow in self.activity.iter() {
            // We need every source node that is configured to execute some activity to be included in our set of
            // nodes so that we can execute events on it.
            let source_node =
                self.nodes
                    .get(&payment_flow.source)
                    .ok_or(LightningError::ValidationError(format!(
                        "Source node not found, {}",
                        payment_flow.source,
                    )))?;

            // Destinations must support keysend to be able to receive payments.
            // Note: validation should be update with a different check if an event is not a payment.
            let features = source_node
                .lock()
                .await
                .get_node_features(payment_flow.destination)
                .await
                .map_err(|e| {
                    log::debug!("{}", e);
                    LightningError::ValidationError(format!(
                        "Destination node unknown or invalid, {}",
                        payment_flow.destination,
                    ))
                })?;

            if !features.supports_keysend() {
                return Err(LightningError::ValidationError(format!(
                    "Destination node does not support keysend, {}",
                    payment_flow.destination,
                )));
            }
        }

        Ok(())
    }

    // it validates that the nodes are all on the same network and ensures that we're not running on mainnet.
    async fn validate_node_network(&self) -> Result<(), LightningError> {
        let mut valid_network = Option::None;
        let unsupported_networks: HashSet<String> = vec!["mainnet", "bitcoin"]
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        for node in self.nodes.values() {
            let network = node.lock().await.get_info().network.clone();
            if unsupported_networks.contains(&network) {
                return Err(LightningError::ValidationError(
                    "mainnet is not supported".to_string(),
                ));
            }

            valid_network = valid_network.take().or_else(|| Some(network.clone()));
            if let Some(valid) = &valid_network {
                if valid != &network {
                    return Err(LightningError::ValidationError(format!(
                        "nodes are not on the same network {}.",
                        network,
                    )));
                }
            }
        }

        log::info!(
            "Simulation is running on the network: {}.",
            valid_network.as_ref().unwrap()
        );

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

        // Create consumers for every source node when dealing with activity descriptions, or only for nodes with
        // sufficient capacity if generating random activity. Since we have to query the capacity of every node
        // in our network for picking random activity nodes, we cache this value here to be used later when we spin
        // up producers.
        let mut random_activity_nodes = HashMap::new();
        let collecting_nodes = if !self.activity.is_empty() {
            self.activity
                .iter()
                .map(|activity| activity.source)
                .collect()
        } else {
            random_activity_nodes.extend(self.random_activity_nodes().await?);
            random_activity_nodes.keys().cloned().collect()
        };

        let producer_senders =
            self.dispatch_consumers(collecting_nodes, event_sender.clone(), &mut tasks);

        // Next, we'll spin up our actual activity generator that will be responsible for triggering the activity that
        // has been configured (if any), passing in the channel that is used to notify data collection that events have
        // been generated. Alternatively, we'll generate random activity if there is no activity specified.
        if !self.activity.is_empty() {
            self.dispatch_activity_producers(producer_senders, &mut tasks)
                .await;
        } else {
            log::info!(
                "Generating random activity with multiplier: {}, average payment amount: {}.",
                self.activity_multiplier,
                self.expected_payment_msat
            );
            self.dispatch_random_producers(random_activity_nodes, producer_senders, &mut tasks)
                .await?;
        }

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

    // run_data_collection starts the tasks required for the simulation to report of the results of the activity that
    // it generates. The simulation should report outputs via the receiver that is passed in.
    fn run_data_collection(
        &self,
        output_receiver: Receiver<SimulationOutput>,
        tasks: &mut JoinSet<()>,
    ) {
        let listener = self.shutdown_listener.clone();
        let print_batch_size = self.print_batch_size;
        log::debug!("Setting up simulator data collection.");

        // Create a sender/receiver pair that will be used to report final results of simulation.
        let (results_sender, results_receiver) = channel(1);

        tasks.spawn(produce_simulation_results(
            self.nodes.clone(),
            output_receiver,
            results_sender,
            listener.clone(),
        ));

        tasks.spawn(consume_simulation_results(
            results_receiver,
            listener,
            print_batch_size,
        ));
        log::debug!("Simulator data collection set up.");
    }

    /// Returns the list of nodes that are eligible for generating random activity on. This is the subset of nodes
    /// that have sufficient capacity to generate payments of our expected payment amount.
    async fn random_activity_nodes(&self) -> Result<HashMap<PublicKey, u64>, SimulationError> {
        // Collect capacity of each node from its view of its own channels. Total capacity is divided by two to
        // avoid double counting capacity (as each node has a counterparty in the channel).
        let mut node_capacities = HashMap::new();
        for (pk, node) in self.nodes.iter() {
            let chan_capacity = node.lock().await.list_channels().await?.iter().sum::<u64>();

            if let Err(e) = PaymentActivityGenerator::validate_capacity(
                chan_capacity,
                self.expected_payment_msat,
            ) {
                log::warn!("Node: {} not eligible for activity generation: {e}.", *pk);
                continue;
            }

            node_capacities.insert(*pk, chan_capacity / 2);
        }

        Ok(node_capacities)
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

    /// Responsible for spinning up producers for a set of activity descriptions.
    async fn dispatch_activity_producers(
        &self,
        producer_channels: HashMap<PublicKey, Sender<SimulationEvent>>,
        tasks: &mut JoinSet<()>,
    ) {
        for description in self.activity.iter() {
            let sender_chan = producer_channels.get(&description.source).unwrap();
            tasks.spawn(produce_events(
                *description,
                sender_chan.clone(),
                self.shutdown_trigger.clone(),
                self.shutdown_listener.clone(),
            ));
        }
    }

    /// Responsible for spinning up producers for a set of activity descriptions. Requires that node capacities are
    /// provided for each node represented in producer channels.
    async fn dispatch_random_producers(
        &self,
        node_capacities: HashMap<PublicKey, u64>,
        producer_channels: HashMap<PublicKey, Sender<SimulationEvent>>,
        tasks: &mut JoinSet<()>,
    ) -> Result<(), SimulationError> {
        let network_generator =
            Arc::new(Mutex::new(NetworkGraphView::new(node_capacities.clone())?));

        log::info!(
            "Created network generator: {}.",
            network_generator.lock().await
        );

        for (pk, sender) in producer_channels.into_iter() {
            let source_capacity = match node_capacities.get(&pk) {
                Some(capacity) => *capacity,
                None => {
                    return Err(SimulationError::RandomActivityError(format!(
                        "Random activity generator run for: {} with unknown capacity.",
                        pk
                    )));
                }
            };

            let node_generator = PaymentActivityGenerator::new(
                source_capacity,
                self.expected_payment_msat,
                self.activity_multiplier,
            )?;

            tasks.spawn(produce_random_events(
                pk,
                network_generator.clone(),
                node_generator,
                sender.clone(),
                self.shutdown_trigger.clone(),
                self.shutdown_listener.clone(),
            ));
        }

        Ok(())
    }
}

// consume_events processes events that are crated for a lightning node that we can execute events on. Any output
// that is generated from the event being executed is piped into a channel to handle the result of the event. If it
// exits, it will use the trigger provided to trigger shutdown in other threads. If an error occurs elsewhere, we
// expect the senders corresponding to our receiver to be dropped, which will cause the receiver to error out and
// exit.
async fn consume_events(
    node: Arc<Mutex<dyn LightningNode + Send>>,
    mut receiver: Receiver<SimulationEvent>,
    sender: Sender<SimulationOutput>,
    shutdown: Trigger,
) {
    let node_id = node.lock().await.get_info().pubkey;
    log::debug!("Started consumer for {}.", node_id);

    while let Some(event) = receiver.recv().await {
        match event {
            SimulationEvent::SendPayment(dest, amt_msat) => {
                let mut node = node.lock().await;

                let mut payment = Payment {
                    source: node.get_info().pubkey,
                    hash: None,
                    amount_msat: amt_msat,
                    destination: dest,
                    dispatch_time: SystemTime::now(),
                };

                let outcome = match node.send_payment(dest, amt_msat).await {
                    Ok(payment_hash) => {
                        log::debug!(
                            "Send payment: {} -> {}: ({}).",
                            node_id,
                            dest,
                            hex::encode(payment_hash.0)
                        );
                        // We need to track the payment outcome using the payment hash that we have received.
                        payment.hash = Some(payment_hash);
                        SimulationOutput::SendPaymentSuccess(payment)
                    }
                    Err(e) => {
                        log::error!("Error while sending payment {} -> {}.", node_id, dest);

                        match e {
                            LightningError::PermanentError(s) => {
                                log::error!("Simulation terminated with error: {s}.");
                                shutdown.trigger();
                                break;
                            }
                            _ => SimulationOutput::SendPaymentFailure(
                                payment,
                                PaymentResult::not_dispatched(),
                            ),
                        }
                    }
                };

                match sender.send(outcome).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::error!("Error sending action outcome: {:?}.", e);
                        break;
                    }
                }
            }
        };
    }
}

// produce events generates events for the activity description provided. It accepts a shutdown listener so it can
// exit if other threads signal that they have errored out.
async fn produce_events(
    act: ActivityDefinition,
    sender: Sender<SimulationEvent>,
    shutdown: Trigger,
    listener: Listener,
) {
    let e: SimulationEvent = SimulationEvent::SendPayment(act.destination, act.amount_msat);
    let interval = time::Duration::from_secs(act.interval as u64);

    log::debug!(
        "Started producer for {} every {}s: {} -> {}.",
        act.amount_msat,
        act.interval,
        act.source,
        act.destination
    );

    loop {
        tokio::select! {
        biased;
        _ = time::sleep(interval) => {
            // Consumer was dropped
            if sender.send(e).await.is_err() {
                log::debug!(
                    "Stopped producer for {}: {} -> {}. Consumer cannot be reached.",
                    act.amount_msat,
                    act.source,
                    act.destination
                );
                break;
            }
        }
        _ = listener.clone() => {
            // Shutdown was signaled
            log::debug!(
                    "Stopped producer for {}: {} -> {}. Received shutdown signal.",
                    act.amount_msat,
                    act.source,
                    act.destination
            );
            break;
            }
        }
    }

    // On exit call our shutdown trigger to inform other threads that we have exited, and they need to shut down.
    shutdown.trigger();
}

async fn produce_random_events<N: NetworkGenerator, A: PaymentGenerator + Display>(
    source: PublicKey,
    network_generator: Arc<Mutex<N>>,
    node_generator: A,
    sender: Sender<SimulationEvent>,
    shutdown: Trigger,
    listener: Listener,
) {
    log::info!("Started random activity producer for {source}: {node_generator}.");

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
                let destination = network_generator.lock().await.sample_node_by_capacity(source);

                // Only proceed with a payment if the amount is non-zero, otherwise skip this round. If we can't get
                // a payment amount something has gone wrong (because we should have validated that we can always
                // generate amounts), so we exit.
                let amount = match node_generator.payment_amount(destination.1) {
                    Ok(amt) => {
                        if amt == 0 {
                            log::debug!("Skipping zero amount payment for {source} -> {}.", destination.0);
                            continue;
                        }
                        amt
                    },
                    Err(e) => {
                        log::error!("Could not get amount for {source} -> {}: {e}. Please report a bug!", destination.0);
                        break;
                    },
                };

                log::debug!("Generated random payment: {source} -> {}: {amount} msat.", destination.0);

                // Send the payment, exiting if we can no longer send to the consumer.
                let event = SimulationEvent::SendPayment(destination.0, amount);
                if let Err(e) = sender.send(event).await {
                    log::debug!(
                        "Stopped random producer for {amount}: {source} -> {}. Consumer error: {e}.", destination.0,
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
    receiver: Receiver<(Payment, PaymentResult)>,
    listener: Listener,
    print_batch_size: u32,
) {
    log::debug!("Simulation results consumer started.");

    if let Err(e) = write_payment_results(receiver, listener, print_batch_size).await {
        log::error!("Error while reporting payment results: {:?}.", e);
    }

    log::debug!("Simulation results consumer exiting.");
}

async fn write_payment_results(
    mut receiver: Receiver<(Payment, PaymentResult)>,
    listener: Listener,
    print_batch_size: u32,
) -> Result<(), SimulationError> {
    let mut writer = WriterBuilder::new().from_path(format!(
        "simulation_{:?}.csv",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    ))?;

    let mut result_logger = PaymentResultLogger::new();

    let mut counter = 1;
    loop {
        tokio::select! {
            biased;
            _ = listener.clone() => {
                log::debug!("Simulation results consumer received shutdown signal.");
                break writer.flush().map_err(|_| SimulationError::FileError)
            },
            payment_report = receiver.recv() => {
                match payment_report {
                    Some((details, result)) => {
                        result_logger.report_result(&details, &result);
                        log::trace!("Resolved dispatched payment: {} with: {}.", details, result);

                        writer.serialize((details, result)).map_err(|e| {
                            let _ = writer.flush();
                            SimulationError::CsvError(e)
                        })?;

                        if print_batch_size == counter {
                            writer.flush().map_err(|_| SimulationError::FileError)?;
                            counter = 1;
                        } else {
                            counter += 1;
                        }
                        continue;
                    },
                    None => {
                        break writer.flush().map_err(|_| SimulationError::FileError)
                    }
                }
            }
        }
    }
}

/// PaymentResultLogger is an aggregate logger that will report on a summary of the payments that have been reported
/// to it at regular intervals (defined by the log_interval it is created with).
#[derive(Default)]
struct PaymentResultLogger {
    success_payment: u64,
    failed_payment: u64,
    total_sent: u64,
    call_count: u8,
    log_interval: u8,
}

impl PaymentResultLogger {
    fn new() -> Self {
        PaymentResultLogger {
            // TODO: set the interval at which we log based on the number of payment we're expecting to log.
            log_interval: 10,
            ..Default::default()
        }
    }

    fn report_result(&mut self, details: &Payment, result: &PaymentResult) {
        match result.payment_outcome {
            PaymentOutcome::Success => self.success_payment += 1,
            _ => self.failed_payment += 1,
        }

        self.total_sent += details.amount_msat;
        self.call_count += 1;

        if self.call_count % self.log_interval == 0 || self.call_count == 0 {
            let total_payments = self.success_payment + self.failed_payment;
            log::info!(
                "Processed {} payments sending {} msat total with {}% success rate.",
                total_payments,
                self.total_sent,
                (self.success_payment * 100 / total_payments)
            );
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
    nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>>,
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
    node: Arc<Mutex<dyn LightningNode + Send>>,
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
                }
                Err(e) => {
                    log::error!("Track payment failed for {}: {e}.", hex::encode(hash.0));
                    PaymentResult::track_payment_failed()
                }
            }
        }
        // None means that the payment was not dispatched, so we cannot track it.
        None => {
            log::error!(
                "We cannot track a payment that has not been dispatched. Missing payment hash."
            );
            PaymentResult::not_dispatched()
        }
    };

    if results.clone().send((payment, res)).await.is_err() {
        log::debug!("Could not send payment result.");
    }

    log::trace!("Payment result tracker exiting.");
}
