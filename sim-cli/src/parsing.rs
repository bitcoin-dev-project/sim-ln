use anyhow::anyhow;
use bitcoin::secp256k1::PublicKey;
use clap::{builder::TypedValueParser, Parser};
use futures::future::BoxFuture;
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use simln_lib::{
    cln, cln::ClnNode, eclair, eclair::EclairNode, lnd, lnd::LndNode, serializers,
    ActivityDefinition, Amount, Interval, LightningError, LightningNode, NodeId, NodeInfo,
    Simulation, SimulationCfg, WriteResults,
};
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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SimParams {
    pub nodes: Vec<NodeConnection>,
    #[serde(default)]
    pub activity: Vec<ActivityParser>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
enum NodeConnection {
    Lnd(lnd::LndConnection),
    Cln(cln::ClnConnection),
    Eclair(eclair::EclairConnection),
}

/// Data structure used to parse information from the simulation file. It allows source and destination to be
/// [NodeId], which enables the use of public keys and aliases in the simulation description.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActivityParser {
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

/// Parses the cli options provided and creates a simulation to be run, connecting to lightning nodes and validating
/// any activity described in the simulation file.
pub async fn create_simulation(cli: &Cli) -> Result<Simulation, anyhow::Error> {
    let cfg: SimulationCfg = SimulationCfg::try_from(cli)?;

    let sim_path = read_sim_path(cli.data_dir.clone(), cli.sim_file.clone()).await?;
    let SimParams { nodes, activity } = serde_json::from_str(&std::fs::read_to_string(sim_path)?)
        .map_err(|e| {
        anyhow!(
            "Could not deserialize node connection data or activity description from simulation file (line {}, col {}, err: {}).",
            e.line(),
            e.column(),
            e.to_string()
        )
    })?;

    let (clients, clients_info) = get_clients(nodes).await?;
    // We need to be able to look up destination nodes in the graph, because we allow defined activities to send to
    // nodes that we do not control. To do this, we can just grab the first node in our map and perform the lookup.
    let get_node = |pk: &PublicKey| -> BoxFuture<'static, Result<NodeInfo, LightningError>> {
        let clients_clone = clients.clone(); // Clone to avoid moving
        let pk_owned = *pk;
        Box::pin(async move {
            if let Some(c) = clients_clone.values().next() {
                return c.lock().await.get_node_info(&pk_owned).await;
            }
            Err(LightningError::GetNodeInfoError(
                "no nodes for query".to_string(),
            ))
        })
    };

    let validated_activities = validate_activities(activity, &clients_info, get_node).await?;
    let tasks = TaskTracker::new();

    Ok(Simulation::new(cfg, clients, validated_activities, tasks))
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
async fn add_node_to_maps(
    nodes: &HashMap<PublicKey, NodeInfo>,
) -> Result<(HashMap<PublicKey, NodeInfo>, HashMap<String, NodeInfo>), LightningError> {
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

    Ok((pk_node_map, alias_node_map))
}

/// Validates a set of defined activities, cross-checking aliases and public keys against the set of clients that
/// have been configured.
async fn validate_activities(
    activity: Vec<ActivityParser>,
    nodes: &HashMap<PublicKey, NodeInfo>,
    get_node_info: impl Fn(&PublicKey) -> BoxFuture<'static, Result<NodeInfo, LightningError>>,
) -> Result<Vec<ActivityDefinition>, LightningError> {
    let mut validated_activities = vec![];
    let (pk_node_map, alias_node_map) = add_node_to_maps(nodes).await?;

    // Make all the activities identifiable by PK internally
    for act in activity.into_iter() {
        // We can only map aliases to nodes we control, so if either the source or destination alias
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
                if let Some(info) = alias_node_map.get(a) {
                    info.clone()
                } else {
                    return Err(LightningError::ValidationError(format!(
                        "unknown activity destination: {}.",
                        act.destination
                    )));
                }
            },
            NodeId::PublicKey(pk) => {
                if let Some(info) = pk_node_map.get(pk) {
                    info.clone()
                } else {
                    get_node_info(pk).await?
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
