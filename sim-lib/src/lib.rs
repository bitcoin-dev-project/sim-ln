use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use csv::WriterBuilder;
use lightning::ln::features::NodeFeatures;
use lightning::ln::PaymentHash;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::marker::Send;
use std::{collections::HashMap, sync::Arc, time::SystemTime};
use thiserror::Error;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time;
use tokio::time::Duration;
use triggered::{Listener, Trigger};

pub mod cln;
pub mod lnd;
mod serializers;

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
    pub activity: Vec<ActivityDefinition>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ActivityDefinition {
    // The source of the action.
    pub source: PublicKey,
    // The destination of the action.
    pub destination: PublicKey,
    // The interval of the action, as in every how many seconds the action is performed.
    #[serde(alias = "interval_secs")]
    pub interval: u16,
    // The amount of m_sat to used in this action.
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
}

// Phase 2: Event Queue
#[derive(Debug, Error)]
pub enum LightningError {
    #[error("Node connection error {0}")]
    ConnectionError(String),
    #[error("Get info error {0}")]
    GetInfoError(String),
    #[error("Send payment error {0}")]
    SendPaymentError(String),
    #[error("Track payment error {0}")]
    TrackPaymentError(String),
    #[error("Invalid payment hash")]
    InvalidPaymentHash,
    #[error("Get node info error {0}")]
    GetNodeInfoError(String),
    #[error("Config validation failed {0}")]
    ValidationError(String),
    #[error("RPC error: {0:?}")]
    RpcError(#[from] tonic_lnd::tonic::Status),
}

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub pubkey: PublicKey,
    pub alias: String,
    pub features: NodeFeatures,
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
}

#[derive(Clone, Copy)]
enum NodeAction {
    // Dispatch a payment of the specified amount to the public key provided.
    SendPayment(PublicKey, u64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentResult {
    pub htlc_count: usize,
    pub payment_outcome: PaymentOutcome,
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct DispatchedPayment {
    source: PublicKey,
    destination: PublicKey,
    #[serde(
        serialize_with = "serializers::serialize_payment_hash",
        deserialize_with = "serializers::deserialize_payment_hash"
    )]
    hash: PaymentHash,
    amount_msat: u64,
    #[serde(with = "serde_millis")]
    dispatch_time: SystemTime,
}

#[derive(Debug, Clone, Copy)]
enum ActionOutcome {
    // The payment hash that results from a SendPayment NodeAction being triggered.
    PaymentSent(DispatchedPayment),
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
}

const DEFAULT_PRINT_BATCH_SIZE: u32 = 500;

impl Simulation {
    pub fn new(
        nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>>,
        activity: Vec<ActivityDefinition>,
        total_time: Option<u32>,
        print_batch_size: Option<u32>,
    ) -> Self {
        let (shutdown_trigger, shutdown_listener) = triggered::trigger();
        Self {
            nodes,
            activity,
            shutdown_trigger,
            shutdown_listener,
            total_time: total_time.map(|x| Duration::from_secs(x as u64)),
            print_batch_size: print_batch_size.unwrap_or(DEFAULT_PRINT_BATCH_SIZE),
        }
    }

    /// validate_activity validates that the user-provided activity description is achievable for the network that
    /// we're working with.
    async fn validate_activity(&self) -> Result<(), LightningError> {
        for payment_flow in self.activity.iter() {
            // We need every source node that is configured to execute some activity to be included in our set of
            // nodes so that we can execute actions on it.
            let source_node =
                self.nodes
                    .get(&payment_flow.source)
                    .ok_or(LightningError::ValidationError(format!(
                        "source node not found {}",
                        payment_flow.source,
                    )))?;

            // Destinations must support keysend to be able to receive payments.
            // Note: validation should be update with a different check if an action is not a payment.
            let features = source_node
                .lock()
                .await
                .get_node_features(payment_flow.destination)
                .await
                .map_err(|err| LightningError::GetNodeInfoError(err.to_string()))?;

            if !features.supports_keysend() {
                return Err(LightningError::ValidationError(format!(
                    "destination node does not support keysend {}",
                    payment_flow.destination,
                )));
            }
        }

        Ok(())
    }

    pub async fn run(&self) -> Result<(), SimulationError> {
        if let Some(total_time) = self.total_time {
            log::info!("Running the simulation for {}s", total_time.as_secs());
        } else {
            log::info!("Running the simulation forever");
        }

        self.validate_activity().await?;

        log::info!(
            "Simulating {} activity on {} nodes",
            self.activity.len(),
            self.nodes.len()
        );
        let mut tasks = JoinSet::new();

        // Before we start the simulation up, start tasks that will be responsible for gathering simulation data.
        // The action channels are shared across our functionality:
        // - Action Sender: used by the simulation to inform data reporting that it needs to start tracking the
        //   final outcome of the action that it has taken.
        // - Action Receiver: used by data reporting to receive events that have been simulated that need to be
        //   tracked and recorded.
        let (action_sender, action_receiver) = channel(1);
        self.run_data_collection(action_receiver, &mut tasks);

        // Next, we'll spin up our actual activity generator that will be responsible for triggering the activity that
        // has been configured, passing in the channel that is used to notify data collection that actions  have been
        // generated.
        self.generate_activity(action_sender, &mut tasks).await?;

        if let Some(total_time) = self.total_time {
            let t = self.shutdown_trigger.clone();
            let l = self.shutdown_listener.clone();

            tasks.spawn(async move {
                if time::timeout(total_time, l).await.is_err() {
                    log::info!(
                        "Simulation run for {}s. Shutting down",
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
                log::error!("Task exited with error: {e}");
                success = false;
            }
        }

        success.then_some(()).ok_or(SimulationError::TaskError)
    }

    pub fn shutdown(&self) {
        self.shutdown_trigger.trigger()
    }

    // run_data_collection starts the tasks required for the simulation to report of the outcomes of the activity that
    // it generates. The simulation should report actions via the receiver that is passed in.
    fn run_data_collection(
        &self,
        action_receiver: Receiver<ActionOutcome>,
        tasks: &mut JoinSet<()>,
    ) {
        let listener = self.shutdown_listener.clone();
        let print_batch_size = self.print_batch_size;
        log::debug!("Simulator data recording starting.");

        // Create a sender/receiver pair that will be used to report final results of action outcomes.
        let (results_sender, results_receiver) = channel(1);

        tasks.spawn(produce_simulation_results(
            self.nodes.clone(),
            action_receiver,
            results_sender,
            listener.clone(),
        ));

        tasks.spawn(consume_simulation_results(
            results_receiver,
            listener,
            print_batch_size,
        ));
        log::debug!("Simulator data recording exiting.");
    }

    async fn generate_activity(
        &self,
        executed_actions: Sender<ActionOutcome>,
        tasks: &mut JoinSet<()>,
    ) -> Result<(), SimulationError> {
        let shutdown = self.shutdown_trigger.clone();
        let listener = self.shutdown_listener.clone();

        // Before we start the simulation, we'll spin up the infrastructure that we need to record data:
        // We only need to spin up producers for nodes that are contained in our activity description, as there will be
        // no events for nodes that are not source nodes.
        let mut producer_channels = HashMap::new();

        for (id, node) in self.nodes.iter().filter(|(pk, _)| {
            self.activity
                .iter()
                .map(|a| a.source)
                .collect::<HashSet<PublicKey>>()
                .contains(pk)
        }) {
            // For each active node, we'll create a sender and receiver channel to produce and consumer
            // events. We do not buffer channels as we expect events to clear quickly.
            let (sender, receiver) = channel(1);

            // Generate a consumer for the receiving end of the channel. It takes the event receiver that it'll pull
            // events from and the results sender to report the events it has triggered for further monitoring.
            tasks.spawn(consume_events(
                node.clone(),
                receiver,
                executed_actions.clone(),
            ));

            // Add the producer channel to our map so that various activity descriptions can use it. We may have multiple
            // activity descriptions that have the same source node.
            producer_channels.insert(id, sender);
        }

        for description in self.activity.iter() {
            let sender_chan = producer_channels.get(&description.source).unwrap();
            tasks.spawn(produce_events(
                *description,
                sender_chan.clone(),
                shutdown.clone(),
                listener.clone(),
            ));
        }

        Ok(())
    }
}

// consume_events processes events that are crated for a lightning node that we can execute actions on. Any output
// that is generated from the action being taken is piped into a channel to handle the result of the action. If it
// exits, it will use the trigger provided to trigger shutdown in other threads. If an error occurs elsewhere, we
// expect the senders corresponding to our receiver to be dropped, which will cause the receiver to error out and
// exit.
async fn consume_events(
    node: Arc<Mutex<dyn LightningNode + Send>>,
    mut receiver: Receiver<NodeAction>,
    sender: Sender<ActionOutcome>,
) {
    let node_id = node.lock().await.get_info().pubkey;
    log::debug!("Started consumer for {}", node_id);

    while let Some(action) = receiver.recv().await {
        match action {
            NodeAction::SendPayment(dest, amt_msat) => {
                let mut node = node.lock().await;
                let payment = node.send_payment(dest, amt_msat);

                match payment.await {
                    Ok(payment_hash) => {
                        log::debug!(
                            "Send payment: {} -> {}: ({})",
                            node_id,
                            dest,
                            hex::encode(payment_hash.0)
                        );

                        log::debug!("Sending action for {}", hex::encode(payment_hash.0));
                        let outcome = ActionOutcome::PaymentSent(DispatchedPayment {
                            source: node.get_info().pubkey,
                            hash: payment_hash,
                            amount_msat: amt_msat,
                            destination: dest,
                            dispatch_time: SystemTime::now(),
                        });

                        match sender.send(outcome).await {
                            Ok(_) => {}
                            Err(e) => {
                                log::error!("Error sending action outcome: {:?}", e);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        log::error!(
                            "Error while sending payment {} -> {}. Terminating consumer. {}",
                            node_id,
                            dest,
                            e
                        );
                        break;
                    }
                };
            }
        };
    }
}

// produce events generates events for the activity description provided. It accepts a shutdown listener so it can
// exit if other threads signal that they have errored out.
async fn produce_events(
    act: ActivityDefinition,
    sender: Sender<NodeAction>,
    shutdown: Trigger,
    listener: Listener,
) {
    let e: NodeAction = NodeAction::SendPayment(act.destination, act.amount_msat);
    let interval = time::Duration::from_secs(act.interval as u64);

    log::debug!(
        "Started producer for {} every {}s: {} -> {}",
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
                    "Stopped producer for {}: {} -> {}. Consumer cannot be reached",
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
                    "Stopped producer for {}: {} -> {}. Received shutdown signal",
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

async fn consume_simulation_results(
    receiver: Receiver<(DispatchedPayment, PaymentResult)>,
    listener: Listener,
    print_batch_size: u32,
) {
    log::debug!("Simulation results consumer started.");

    if let Err(e) = write_payment_results(receiver, listener, print_batch_size).await {
        log::error!("Error while reporting payment results: {:?}", e);
    }

    log::debug!("Simulation results consumer exiting");
}

async fn write_payment_results(
    mut receiver: Receiver<(DispatchedPayment, PaymentResult)>,
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
                        log::trace!("Resolved payment received: ({:?}, {:?})", details, result);

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

    fn report_result(&mut self, details: &DispatchedPayment, result: &PaymentResult) {
        match result.payment_outcome {
            PaymentOutcome::Success => self.success_payment += 1,
            _ => self.failed_payment += 1,
        }

        self.total_sent += details.amount_msat;
        self.call_count += 1;

        if self.call_count % self.log_interval == 0 || self.call_count == 0 {
            let total_payments = self.success_payment + self.failed_payment;
            log::info!(
                "Processed {} payments sending {} msat total with {}% success rate",
                total_payments,
                self.total_sent,
                (self.success_payment * 100 / total_payments)
            );
        }
    }
}

/// produce_results is responsible for receiving the outcomes of actions that the simulator has taken and
/// spinning up a producer that will report the results to our main result consumer. We handle each outcome
/// separately because they can take a long time to resolve (eg, a payment that ends up on chain will take a long
/// time to resolve).
///
/// Note: this producer does not accept a shutdown trigger because it only expects to be dispatched once. In the single
/// producer case exit will drop the only sending channel and the receiving channel provided to the consumer will error
/// out. In the multiple-producer case, a single producer shutting down does not drop *all* sending channels so the
/// consumer will not exit and a trigger is required.
async fn produce_simulation_results(
    nodes: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>>,
    mut outcomes: Receiver<ActionOutcome>,
    results: Sender<(DispatchedPayment, PaymentResult)>,
    shutdown: Listener,
) {
    log::debug!("Simulation results producer started.");

    let mut set = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.clone() => break,
            outcome = outcomes.recv() => {
                match outcome{
                    Some(action_outcome) => {
                        match action_outcome{
                            ActionOutcome::PaymentSent(dispatched_payment) => {
                                let source_node = nodes.get(&dispatched_payment.source).unwrap().clone();

                                log::debug!("Tracking payment outcome for: {}", hex::encode(dispatched_payment.hash.0));
                                set.spawn(track_outcome(
                                    source_node,results.clone(),action_outcome, shutdown.clone(),
                                ));
                            },
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
            log::error!("Simulation results producer task exited with error: {e}");
        }
    }
}

async fn track_outcome(
    node: Arc<Mutex<dyn LightningNode + Send>>,
    results: Sender<(DispatchedPayment, PaymentResult)>,
    outcome: ActionOutcome,
    shutdown: Listener,
) {
    log::trace!("Outcome tracker starting.");

    let mut node = node.lock().await;

    match outcome {
        ActionOutcome::PaymentSent(payment) => {
            let track_payment = node.track_payment(payment.hash, shutdown.clone());

            match track_payment.await {
                Ok(res) => {
                    if results.clone().send((payment, res)).await.is_err() {
                        log::debug!("Could not send payment result for {:?}.", payment.hash);
                    }
                }
                Err(e) => log::error!(
                    "Track payment failed for {}: {e}",
                    hex::encode(payment.hash.0)
                ),
            }
        }
    }

    log::trace!("Outcome tracker exiting.");
}
