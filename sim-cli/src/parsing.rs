use anyhow::anyhow;
use bitcoin::secp256k1::PublicKey;
use clap::{builder::TypedValueParser, Parser};
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use simln_lib::clock::SimulationClock;
use simln_lib::sim_node::{
    ln_node_from_graph, populate_network_graph, ChannelPolicy, CustomRecords, Interceptor,
    SimGraph, SimulatedChannel,
};
use simln_lib::{
    cln, cln::ClnNode, eclair, eclair::EclairNode, lnd, lnd::LndNode, serializers,
    ActivityDefinition, Amount, Interval, LightningError, LightningNode, NodeId, NodeInfo,
    Simulation, SimulationCfg, WriteResults,
};
use simln_lib::{ShortChannelID, SimulationError};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;

/// The default directory where the simulation files are stored and where the results will be written to.
pub const DEFAULT_DATA_DIR: &str = ".";

/// The default simulation file to be used by the simulator.
pub const DEFAULT_SIM_FILE: &str = "sim.json";

/// The default expected payment amount for the simulation, around ~$10 at the time of writing.
pub const DEFAULT_EXPECTED_PAYMENT_AMOUNT: u64 = 3_800_000;

/// The number of times over each node in the network sends its total deployed capacity in a calendar month.
pub const DEFAULT_ACTIVITY_MULTIPLIER: f64 = 2.0;

/// Default batch size to flush result data to disk
const DEFAULT_PRINT_BATCH_SIZE: u32 = 500;

/// Deserializes a f64 as long as it is positive and greater than 0.
fn deserialize_f64_greater_than_zero(x: String) -> Result<f64, String> {
    match x.parse::<f64>() {
        Ok(x) => {
            if x > 0.0 {
                Ok(x)
            } else {
                Err(format!(
                    "capacity_multiplier must be higher than 0. {x} received."
                ))
            }
        },
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Parser)]
#[command(version, about)]
pub struct Cli {
    /// Path to a directory containing simulation files, and where simulation results will be stored
    #[clap(long, short, default_value = DEFAULT_DATA_DIR)]
    pub data_dir: PathBuf,
    /// Path to the simulation file to be used by the simulator
    /// This can either be an absolute path, or relative path with respect to data_dir
    #[clap(long, short, default_value = DEFAULT_SIM_FILE)]
    pub sim_file: PathBuf,
    /// Total time the simulator will be running
    #[clap(long, short)]
    pub total_time: Option<u32>,
    /// Number of activity results to batch together before printing to csv file [min: 1]
    #[clap(long, short, default_value_t = DEFAULT_PRINT_BATCH_SIZE, value_parser = clap::builder::RangedU64ValueParser::<u32>::new().range(1..u32::MAX as u64))]
    pub print_batch_size: u32,
    /// Level of verbosity of the messages displayed by the simulator.
    /// Possible values: [off, error, warn, info, debug, trace]
    #[clap(long, short, verbatim_doc_comment, default_value = "info")]
    pub log_level: LevelFilter,
    /// Expected payment amount for the random activity generator
    #[clap(long, short, default_value_t = DEFAULT_EXPECTED_PAYMENT_AMOUNT, value_parser = clap::builder::RangedU64ValueParser::<u64>::new().range(1..u64::MAX))]
    pub expected_pmt_amt: u64,
    /// Multiplier of the overall network capacity used by the random activity generator
    #[clap(long, short, default_value_t = DEFAULT_ACTIVITY_MULTIPLIER, value_parser = clap::builder::StringValueParser::new().try_map(deserialize_f64_greater_than_zero))]
    pub capacity_multiplier: f64,
    /// Do not create an output file containing the simulations results
    #[clap(long, default_value_t = false)]
    pub no_results: bool,
    /// Seed to run random activity generator deterministically
    #[clap(long, short)]
    pub fix_seed: Option<u64>,
    /// A multiplier to wall time to speed up the simulation's clock. Only available when when running on a network of
    /// simulated nodes.
    #[clap(long)]
    pub speedup_clock: Option<u16>,
    /// Latency to optionally introduce for payments in a simulated network expressed in
    /// milliseconds.
    #[clap(long)]
    pub latency_ms: Option<u32>,
}

impl Cli {
    pub fn validate(&self, sim_params: &SimParams) -> Result<(), anyhow::Error> {
        // Validate that nodes and sim_graph are exclusively set
        if !sim_params.nodes.is_empty() && !sim_params.sim_network.is_empty() {
            return Err(anyhow!(
                "Simulation file cannot contain {} nodes and {} sim_graph entries, 
                simulation can only be run with real or simulated nodes not both.",
                sim_params.nodes.len(),
                sim_params.sim_network.len()
            ));
        }
        if sim_params.nodes.is_empty() && sim_params.sim_network.is_empty() {
            return Err(anyhow!(
                "Simulation file must contain nodes to run with real lightning 
                nodes or sim_graph to run with simulated nodes"
            ));
        }
        if !sim_params.nodes.is_empty() && self.speedup_clock.is_some() {
            return Err(anyhow!(
                "Clock speedup is only allowed when running on a simulated network"
            ));
        }

        if !sim_params.nodes.is_empty() && self.latency_ms.is_some() {
            return Err(anyhow!(
                "Latency for payments is only allowed when running on a simulated network"
            ));
        }

        if !sim_params.exclude.is_empty() {
            if sim_params.sim_network.is_empty() {
                return Err(anyhow!(
                    "List of nodes to exclude from sending/receiving
                    in random activity is only allowed on a simulated network"
                ));
            }

            if !sim_params.activity.is_empty() {
                return Err(anyhow!(
                    "List of nodes to exclude from sending/receiving is only allowed on random activity"
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SimParams {
    #[serde(default)]
    nodes: Vec<NodeConnection>,
    #[serde(default)]
    pub sim_network: Vec<NetworkParser>,
    #[serde(default)]
    pub activity: Vec<ActivityParser>,
    #[serde(default)]
    pub exclude: Vec<PublicKey>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum NodeConnection {
    Lnd(lnd::LndConnection),
    Cln(cln::ClnConnection),
    Eclair(eclair::EclairConnection),
}

/// Data structure that is used to parse information from the simulation file. It is used to
/// create a mocked network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkParser {
    pub scid: ShortChannelID,
    pub capacity_msat: u64,
    pub node_1: ChannelPolicy,
    pub node_2: ChannelPolicy,
}

impl From<NetworkParser> for SimulatedChannel {
    fn from(network_parser: NetworkParser) -> Self {
        SimulatedChannel::new(
            network_parser.capacity_msat,
            network_parser.scid,
            network_parser.node_1,
            network_parser.node_2,
        )
    }
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
    /// The time in the simulation to start the payment.
    pub start_secs: Option<u16>,
    /// The number of payments to send over the course of the simulation.
    #[serde(default)]
    pub count: Option<u64>,
    /// The interval of the event, as in every how many seconds the payment is performed.
    #[serde(with = "serializers::serde_value_or_range")]
    pub interval_secs: Interval,
    /// The amount of m_sat to used in this payment.
    #[serde(with = "serializers::serde_value_or_range")]
    pub amount_msat: Amount,
}

struct ActivityValidationParams {
    pk_node_map: HashMap<PublicKey, NodeInfo>,
    alias_node_map: HashMap<String, NodeInfo>,
    graph_nodes_by_pk: HashMap<PublicKey, NodeInfo>,
    // Store graph nodes' information keyed by their alias.
    // An alias can be mapped to multiple nodes because it is not a unique identifier.
    graph_nodes_by_alias: HashMap<String, Vec<NodeInfo>>,
}

impl TryFrom<&Cli> for SimulationCfg {
    type Error = anyhow::Error;

    fn try_from(cli: &Cli) -> Result<Self, Self::Error> {
        Ok(SimulationCfg::new(
            cli.total_time,
            cli.expected_pmt_amt,
            cli.capacity_multiplier,
            if !cli.no_results {
                Some(WriteResults {
                    results_dir: mkdir(cli.data_dir.join("results"))?,
                    batch_size: cli.print_batch_size,
                })
            } else {
                None
            },
            cli.fix_seed,
        ))
    }
}

struct NodeMapping {
    pk_node_map: HashMap<PublicKey, NodeInfo>,
    alias_node_map: HashMap<String, NodeInfo>,
}

pub async fn create_simulation_with_network(
    cli: &Cli,
    sim_params: &SimParams,
    tasks: TaskTracker,
    interceptors: Vec<Arc<dyn Interceptor>>,
    custom_records: CustomRecords,
) -> Result<(Simulation<SimulationClock>, Vec<ActivityDefinition>), anyhow::Error> {
    let cfg: SimulationCfg = SimulationCfg::try_from(cli)?;
    let SimParams {
        nodes: _,
        sim_network,
        activity: _activity,
        exclude,
    } = sim_params;

    // Convert nodes representation for parsing to SimulatedChannel
    let channels = sim_network
        .clone()
        .into_iter()
        .map(SimulatedChannel::from)
        .collect::<Vec<SimulatedChannel>>();

    let mut nodes_info = HashMap::new();
    for channel in &channels {
        let (node_1_info, node_2_info) = channel.create_simulated_nodes();
        nodes_info.insert(node_1_info.pubkey, node_1_info);
        nodes_info.insert(node_2_info.pubkey, node_2_info);
    }

    let (shutdown_trigger, shutdown_listener) = triggered::trigger();

    // Setup a simulation graph that will handle propagation of payments through the network
    let simulation_graph = Arc::new(Mutex::new(
        SimGraph::new(
            channels.clone(),
            tasks.clone(),
            interceptors,
            custom_records,
            (shutdown_trigger.clone(), shutdown_listener.clone()),
        )
        .map_err(|e| SimulationError::SimulatedNetworkError(format!("{:?}", e)))?,
    ));

    let clock = Arc::new(SimulationClock::new(cli.speedup_clock.unwrap_or(1))?);

    // Copy all simulated channels into a read-only routing graph, allowing to pathfind for
    // individual payments without locking th simulation graph (this is a duplication of the channels,
    // but the performance tradeoff is worthwhile for concurrent pathfinding).
    let routing_graph = Arc::new(
        populate_network_graph(channels, clock.clone())
            .map_err(|e| SimulationError::SimulatedNetworkError(format!("{:?}", e)))?,
    );

    let mut nodes = ln_node_from_graph(simulation_graph.clone(), routing_graph).await;
    for pk in exclude {
        nodes.remove(pk);
    }

    let validated_activities =
        get_validated_activities(&nodes, nodes_info, sim_params.activity.clone()).await?;

    Ok((
        Simulation::new(
            cfg,
            nodes,
            tasks,
            clock,
            shutdown_trigger,
            shutdown_listener,
        ),
        validated_activities,
    ))
}

/// Parses the cli options provided and creates a simulation to be run, connecting to lightning nodes and validating
/// any activity described in the simulation file.
pub async fn create_simulation(
    cli: &Cli,
    sim_params: &SimParams,
    tasks: TaskTracker,
) -> Result<(Simulation<SimulationClock>, Vec<ActivityDefinition>), anyhow::Error> {
    let cfg: SimulationCfg = SimulationCfg::try_from(cli)?;
    let SimParams {
        nodes,
        sim_network: _sim_network,
        activity: _activity,
        exclude: _,
    } = sim_params;

    let (clients, clients_info) = get_clients(nodes.to_vec()).await?;
    let (shutdown_trigger, shutdown_listener) = triggered::trigger();

    let validated_activities =
        get_validated_activities(&clients, clients_info, sim_params.activity.clone()).await?;

    Ok((
        Simulation::new(
            cfg,
            clients,
            tasks,
            // When running on a real network, the underlying node may use wall time so we always use a clock with no
            // speedup.
            Arc::new(SimulationClock::new(1)?),
            shutdown_trigger,
            shutdown_listener,
        ),
        validated_activities,
    ))
}

/// Connects to the set of nodes providing, returning a map of node public keys to LightningNode implementations and
/// a map of public key to node info to be used for validation.
async fn get_clients(
    nodes: Vec<NodeConnection>,
) -> Result<
    (
        HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
        HashMap<PublicKey, NodeInfo>,
    ),
    LightningError,
> {
    let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> = HashMap::new();
    let mut clients_info: HashMap<PublicKey, NodeInfo> = HashMap::new();

    for connection in nodes {
        // TODO: Feels like there should be a better way of doing this without having to Arc<Mutex<T>>> it at this time.
        // Box sort of works, but we won't know the size of the dyn LightningNode at compile time so the compiler will
        // scream at us when trying to create the Arc<Mutex>> later on while adding the node to the clients map
        let node: Arc<Mutex<dyn LightningNode>> = match connection {
            NodeConnection::Lnd(c) => Arc::new(Mutex::new(LndNode::new(c).await?)),
            NodeConnection::Cln(c) => Arc::new(Mutex::new(ClnNode::new(c).await?)),
            NodeConnection::Eclair(c) => Arc::new(Mutex::new(EclairNode::new(c).await?)),
        };

        let node_info = node.lock().await.get_info().clone();

        clients.insert(node_info.pubkey, node);
        clients_info.insert(node_info.pubkey, node_info);
    }

    Ok((clients, clients_info))
}

/// Adds a lightning node to a client map and tracking maps used to lookup node pubkeys and aliases for activity
/// validation.
fn add_node_to_maps(nodes: &HashMap<PublicKey, NodeInfo>) -> Result<NodeMapping, LightningError> {
    let mut pk_node_map = HashMap::new();
    let mut alias_node_map = HashMap::new();

    for node_info in nodes.values() {
        log::info!(
            "Connected to {} - Node ID: {}.",
            node_info.alias,
            node_info.pubkey
        );

        if pk_node_map.contains_key(&node_info.pubkey) {
            return Err(LightningError::ValidationError(format!(
                "duplicated node: {}.",
                node_info.pubkey
            )));
        }

        if !node_info.alias.is_empty() {
            if alias_node_map.contains_key(&node_info.alias) {
                return Err(LightningError::ValidationError(format!(
                    "duplicated alias: {}.",
                    node_info.alias
                )));
            }

            alias_node_map.insert(node_info.alias.clone(), node_info.clone());
        }

        pk_node_map.insert(node_info.pubkey, node_info.clone());
    }

    Ok(NodeMapping {
        pk_node_map,
        alias_node_map,
    })
}

/// Validates a set of defined activities, cross-checking aliases and public keys against the set of clients that
/// have been configured.
async fn validate_activities(
    activity: Vec<ActivityParser>,
    activity_validation_params: ActivityValidationParams,
) -> Result<Vec<ActivityDefinition>, LightningError> {
    let mut validated_activities = vec![];

    let ActivityValidationParams {
        pk_node_map,
        alias_node_map,
        graph_nodes_by_pk,
        graph_nodes_by_alias,
    } = activity_validation_params;

    // Make all the activities identifiable by PK internally
    for act in activity.into_iter() {
        // We can only map source aliases to nodes we control, so if the source alias
        // is not in alias_node_map, we fail
        let source = if let Some(source) = match &act.source {
            NodeId::PublicKey(pk) => pk_node_map.get(pk),
            NodeId::Alias(a) => alias_node_map.get(a),
        } {
            source.clone()
        } else {
            return Err(LightningError::ValidationError(format!(
                "activity source {} not found in nodes.",
                act.source
            )));
        };

        let destination = match &act.destination {
            NodeId::Alias(a) => {
                if let Some(node_info) = alias_node_map.get(a) {
                    node_info.clone()
                } else if let Some(node_infos) = graph_nodes_by_alias.get(a) {
                    if node_infos.len() > 1 {
                        let pks: Vec<PublicKey> = node_infos
                            .iter()
                            .map(|node_info| node_info.pubkey)
                            .collect();
                        return Err(LightningError::ValidationError(format!(
                            "Multiple nodes in the graph have the same destination alias - {}. 
                            Use one of these public keys as the destination instead - {:?}",
                            a, pks
                        )));
                    }
                    node_infos[0].clone()
                } else {
                    return Err(LightningError::ValidationError(format!(
                        "unknown activity destination: {}.",
                        act.destination
                    )));
                }
            },
            NodeId::PublicKey(pk) => {
                if let Some(node_info) = pk_node_map.get(pk) {
                    node_info.clone()
                } else if let Some(node_info) = graph_nodes_by_pk.get(pk) {
                    node_info.clone()
                } else {
                    return Err(LightningError::ValidationError(format!(
                        "unknown activity destination: {}.",
                        act.destination
                    )));
                }
            },
        };

        validated_activities.push(ActivityDefinition {
            source,
            destination,
            interval_secs: act.interval_secs,
            start_secs: act.start_secs,
            count: act.count,
            amount_msat: act.amount_msat,
        });
    }

    Ok(validated_activities)
}

async fn read_sim_path(data_dir: PathBuf, sim_file: PathBuf) -> anyhow::Result<PathBuf> {
    if sim_file.exists() {
        Ok(sim_file)
    } else if sim_file.is_relative() {
        let sim_path = data_dir.join(sim_file);
        if sim_path.exists() {
            Ok(sim_path)
        } else {
            log::info!("Simulation file '{}' does not exist.", sim_path.display());
            select_sim_file(data_dir).await
        }
    } else {
        log::info!("Simulation file '{}' does not exist.", sim_file.display());
        select_sim_file(data_dir).await
    }
}

pub async fn select_sim_file(data_dir: PathBuf) -> anyhow::Result<PathBuf> {
    let sim_files = std::fs::read_dir(data_dir.clone())?
        .filter_map(|f| {
            f.ok().and_then(|f| {
                if f.path().extension()?.to_str()? == "json" {
                    f.file_name().into_string().ok()
                } else {
                    None
                }
            })
        })
        .collect::<Vec<_>>();

    if sim_files.is_empty() {
        anyhow::bail!(
            "no simulation files found in {}.",
            data_dir.canonicalize()?.display()
        );
    }

    let selection = dialoguer::Select::new()
        .with_prompt(format!(
            "Select a simulation file. Found these in {}",
            data_dir.canonicalize()?.display()
        ))
        .items(&sim_files)
        .default(0)
        .interact()?;

    Ok(data_dir.join(sim_files[selection].clone()))
}

fn mkdir(dir: PathBuf) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub async fn parse_sim_params(cli: &Cli) -> anyhow::Result<SimParams> {
    let sim_path = read_sim_path(cli.data_dir.clone(), cli.sim_file.clone()).await?;
    let sim_params = serde_json::from_str(&std::fs::read_to_string(sim_path)?).map_err(|e| {
        anyhow!(
            "Could not deserialize node connection data or activity description from simulation file (line {}, col {}, err: {}).",
            e.line(),
            e.column(),
            e.to_string()
        )
        })?;
    Ok(sim_params)
}

pub async fn get_validated_activities(
    clients: &HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>>,
    nodes_info: HashMap<PublicKey, NodeInfo>,
    activity: Vec<ActivityParser>,
) -> Result<Vec<ActivityDefinition>, LightningError> {
    // We need to be able to look up destination nodes in the graph, because we allow defined activities to send to
    // nodes that we do not control. To do this, we can just grab the first node in our map and perform the lookup.
    let graph = match clients.values().next() {
        Some(client) => client
            .lock()
            .await
            .get_graph()
            .await
            .map_err(|e| LightningError::GetGraphError(format!("Error getting graph {:?}", e))),
        None => Err(LightningError::GetGraphError("Graph is empty".to_string())),
    }?;
    let mut graph_nodes_by_alias: HashMap<String, Vec<NodeInfo>> = HashMap::new();

    for node in &graph.nodes_by_pk {
        graph_nodes_by_alias
            .entry(node.1.alias.clone())
            .or_default()
            .push(node.1.clone());
    }

    let NodeMapping {
        pk_node_map,
        alias_node_map,
    } = add_node_to_maps(&nodes_info)?;

    let activity_validation_params = ActivityValidationParams {
        pk_node_map,
        alias_node_map,
        graph_nodes_by_pk: graph.nodes_by_pk,
        graph_nodes_by_alias,
    };

    validate_activities(activity.to_vec(), activity_validation_params).await
}
