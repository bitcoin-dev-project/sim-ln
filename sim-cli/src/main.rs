use bitcoin::secp256k1::PublicKey;
use config::{Config, File};
use flexi_logger::{LogSpecBuilder, Logger};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::{collections::HashMap, str::FromStr};
use tokio::sync::Mutex;

use anyhow::anyhow;
use clap::builder::TypedValueParser;
use clap::Parser;
use log::LevelFilter;
use sim_lib::{
    cln::ClnNode, lnd::LndNode, ActivityDefinition, LightningError, LightningNode, NodeConnection,
    NodeId, SimParams, Simulation, SimulationConfig, WriteResults,
};

/// The default directory where the simulation files are stored and where the results will be written to.
pub const DEFAULT_DATA_DIR: &str = ".";

/// The default simulation file to be used by the simulator.
pub const DEFAULT_SIM_FILE: &str = "sim.json";

/// The default expected payment amount for the simulation, around ~$10 at the time of writing.
pub const EXPECTED_PAYMENT_AMOUNT: u64 = 3_800_000;

/// The number of times over each node in the network sends its total deployed capacity in a calendar month.
pub const ACTIVITY_MULTIPLIER: f64 = 2.0;

/// Default batch size to flush result data to disk
const DEFAULT_PRINT_BATCH_SIZE: u32 = 500;

/// Default configuration file
const DEFAULT_CONFIGURATION_FILE: &str = "conf.ini";

/// Default total time
const DEFAULT_TOTAL_TIME: Option<u32> = None;

/// Default log level
const DEFAULT_LOG_LEVEL: &str = "info";

/// Default no results
const DEFAULT_NO_RESULTS: bool = false;

/// Default log interval
const DEFAULT_LOG_INTERVAL: u32 = 60;

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

/// Custom deserialization function for `LevelFilter`. Required because `LevelFilter` does
/// not implement deserialize
pub fn deserialize_log_level<'de, D>(deserializer: D) -> Result<LevelFilter, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let level_filter_str = String::deserialize(deserializer)?;

    let level_filter = LevelFilter::from_str(&level_filter_str).map_err(|e| {
        serde::de::Error::custom(format!("Failed to deserialize LevelFilter from &str: {e}"))
    })?;

    Ok(level_filter)
}

/// Custom deserialization function for total time. This method is required because
/// the `config` crate is unable to parse null values in `.ini` files to an `Option<String>::None`
pub fn deserialize_total_time<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let total_time_str = String::deserialize(deserializer)?;

    if total_time_str.is_empty() {
        return Ok(None);
    }

    // Parse string value to u32
    let total_time = total_time_str
        .parse::<u32>()
        .map_err(|e| serde::de::Error::custom(format!("Failed to parse u32 from &str: {e}")))?;

    Ok(Some(total_time))
}

#[derive(Parser, Deserialize)]
#[command(version, about)]
struct Cli {
    /// Path to a directory containing simulation files, and where simulation results will be stored
    #[clap(long, short, default_value = DEFAULT_DATA_DIR)]
    data_dir: PathBuf,
    /// Path to the simulation file to be used by the simulator
    /// This can either be an absolute path, or relative path with respect to data_dir
    #[clap(long, short, default_value = DEFAULT_SIM_FILE)]
    sim_file: PathBuf,
    /// Total time the simulator will be running
    #[clap(long, short)]
    total_time: Option<u32>,
    /// Number of activity results to batch together before printing to csv file [min: 1]
    #[clap(long, short, default_value_t = DEFAULT_PRINT_BATCH_SIZE, value_parser = clap::builder::RangedU64ValueParser::<u32>::new().range(1..u32::MAX as u64))]
    print_batch_size: u32,
    /// Level of verbosity of the messages displayed by the simulator.
    /// Possible values: [off, error, warn, info, debug, trace]
    #[clap(long, short, verbatim_doc_comment, default_value = DEFAULT_LOG_LEVEL)]
    #[serde(deserialize_with = "deserialize_log_level")]
    log_level: LevelFilter,
    /// Expected payment amount for the random activity generator
    #[clap(long, short, default_value_t = EXPECTED_PAYMENT_AMOUNT, value_parser = clap::builder::RangedU64ValueParser::<u64>::new().range(1..u64::MAX))]
    expected_pmt_amt: u64,
    /// Multiplier of the overall network capacity used by the random activity generator
    #[clap(long, short, default_value_t = ACTIVITY_MULTIPLIER, value_parser = clap::builder::StringValueParser::new().try_map(deserialize_f64_greater_than_zero))]
    capacity_multiplier: f64,
    /// Do not create an output file containing the simulations results
    #[clap(long, default_value_t = DEFAULT_NO_RESULTS)]
    no_results: bool,
    /// Sets a custom configuration (INI) file
    #[clap(long, short = 'C', value_name = "CONFIG_FILE", default_value = DEFAULT_CONFIGURATION_FILE)]
    config: PathBuf,
    #[clap(long, short = 'L', default_value_t = DEFAULT_LOG_INTERVAL, value_parser = clap::builder::RangedU64ValueParser::<u32>::new().range(1..u32::MAX as u64))]
    log_interval: u32,
}

/// Implementation of Cli with default values
impl Default for Cli {
    fn default() -> Self {
        Cli {
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            sim_file: PathBuf::from(DEFAULT_SIM_FILE),
            total_time: DEFAULT_TOTAL_TIME,
            print_batch_size: DEFAULT_PRINT_BATCH_SIZE,
            log_level: LevelFilter::Info,
            expected_pmt_amt: EXPECTED_PAYMENT_AMOUNT,
            capacity_multiplier: ACTIVITY_MULTIPLIER,
            no_results: DEFAULT_NO_RESULTS,
            config: PathBuf::from(DEFAULT_CONFIGURATION_FILE),
            log_interval: DEFAULT_LOG_INTERVAL,
        }
    }
}

impl Cli {
    /// Creates Cli from a configuration if provided, else it defaults to
    /// CLI default values
    fn from_config_file(file_path: PathBuf) -> anyhow::Result<Self> {
        let default_cli = Cli::default();

        let config = Config::builder()
            .add_source(File::with_name(
                file_path.as_os_str().to_string_lossy().as_ref(),
            ))
            .build()?;

        let simln_conf = config.get_table("simln.conf")?;

        let config_data_dir =
            simln_conf
                .get("data_dir")
                .map_or(default_cli.data_dir.clone(), |val| {
                    let data_dir_res = val.clone().try_deserialize::<String>();
                    match data_dir_res {
                        Ok(data_dir) => {
                            log::info!("Configuration file arg: data_dir={:?}.", data_dir);
                            PathBuf::from(data_dir)
                        },
                        Err(e) => {
                            log::error!(
                                "Failed to parse data_dir. Default value used. Error: {e:?}."
                            );
                            default_cli.data_dir.clone()
                        },
                    }
                });

        let config_sim_file =
            simln_conf
                .get("sim_file")
                .map_or(default_cli.sim_file.clone(), |val| {
                    let sim_file_res = val.clone().try_deserialize::<String>();
                    match sim_file_res {
                        Ok(sim_file) => {
                            log::info!("Configuration file arg: sim_file={:?}.", sim_file);
                            PathBuf::from(sim_file)
                        },
                        Err(e) => {
                            log::error!(
                                "Failed to parse sim_file. Default value used. Error: {e:?}.",
                            );
                            default_cli.sim_file
                        },
                    }
                });

        let config_total_time =
            simln_conf
                .get("total_time")
                .map_or(default_cli.total_time, |val| {
                    let total_time_res = val.clone().try_deserialize::<Option<u32>>();
                    match total_time_res {
                        Ok(total_time) => {
                            log::info!("Configuration file arg: total_time={:?}.", total_time);
                            total_time
                        },
                        Err(e) => {
                            log::error!(
                                "Failed to parse total_time. Default value used. Error: {e:?}."
                            );
                            default_cli.total_time
                        },
                    }
                });

        let config_print_batch_size =
            simln_conf
                .get("print_batch_size")
                .map_or(default_cli.print_batch_size, |val| {
                    let print_batch_size_res = val.clone().try_deserialize::<u32>();
                    match print_batch_size_res {
                        Ok(print_batch_size) => {
                            log::info!(
                                "Configuration file arg: print_batch_size={:?}.",
                                print_batch_size
                            );
                            print_batch_size
                        },
                        Err(e) => {
                            log::error!(
                            "Failed to parse print_batch_size. Default value used. Error: {e:?}."
                        );
                            default_cli.print_batch_size
                        },
                    }
                });

        let config_log_level =
            simln_conf
                .get("log_level")
                .map_or(DEFAULT_LOG_LEVEL.to_string(), |val| {
                    let log_level_res = val.clone().try_deserialize::<String>();
                    match log_level_res {
                        Ok(log_level) => {
                            log::info!("Configuration file arg: log_level={:?}.", log_level);
                            log_level
                        },
                        Err(e) => {
                            log::error!(
                                "Failed to parse log_level. Default value used. Error: {e:?}."
                            );
                            DEFAULT_LOG_LEVEL.to_string()
                        },
                    }
                });

        let config_expected_pmt_amt =
            simln_conf
                .get("expected_pmt_amt")
                .map_or(default_cli.expected_pmt_amt, |val| {
                    let pmt_amt_res = val.clone().try_deserialize::<u64>();
                    match pmt_amt_res {
                        Ok(pmt_amt) => {
                            log::info!("Configuration file arg: expected_pmt_amt={:?}.", pmt_amt);
                            pmt_amt
                        },
                        Err(e) => {
                            log::error!(
                            "Failed to parse expected_pmt_amt. Default value used. Error: {e:?}."
                        );
                            default_cli.expected_pmt_amt
                        },
                    }
                });

        let config_capacity_multiplier =
            simln_conf
                .get("capacity_multiplier")
                .map_or(default_cli.capacity_multiplier, |val| {
                    let capacity_multiplier_res = val.clone().try_deserialize::<f64>();
                    match capacity_multiplier_res {
                        Ok(capacity_multiplier) => {
                            log::info!(
                                "Configuration file arg: capacity_multiplier={:?}.",
                                capacity_multiplier
                            );
                            capacity_multiplier
                        },
                        Err(e) => {
                            log::error!(
                            "Failed to parse capacity_multiplier. Default value used. Error: {e:?}."
                        );
                            default_cli.capacity_multiplier
                        },
                    }
                });

        let config_no_results =
            simln_conf
                .get("no_results")
                .map_or(default_cli.no_results, |val| {
                    let no_results_res = val.clone().try_deserialize::<bool>();
                    match no_results_res {
                        Ok(no_results) => {
                            log::info!("Configuration file arg: no_results={:?}.", no_results);
                            no_results
                        },
                        Err(e) => {
                            log::error!(
                                "Failed to parse no_results. Default value used. Error: {e:?}."
                            );
                            default_cli.no_results
                        },
                    }
                });

        let config_log_interval =
            simln_conf
                .get("log_interval")
                .map_or(default_cli.log_interval, |val| {
                    let log_interval_res = val.clone().try_deserialize::<u32>();
                    match log_interval_res {
                        Ok(log_interval) => {
                            log::info!("Configuration file arg: log_interval={:?}.", log_interval);
                            log_interval
                        },
                        Err(e) => {
                            log::error!(
                                "Failed to parse log_interval. Default value used. Error: {e:?}."
                            );
                            default_cli.log_interval
                        },
                    }
                });

        let config_cli = Cli {
            data_dir: config_data_dir,
            sim_file: config_sim_file,
            total_time: config_total_time,
            print_batch_size: config_print_batch_size,
            log_level: LevelFilter::from_str(&config_log_level)?,
            expected_pmt_amt: config_expected_pmt_amt,
            capacity_multiplier: config_capacity_multiplier,
            no_results: config_no_results,
            config: default_cli.config,
            log_interval: config_log_interval,
        };

        Ok(config_cli)
    }

    /// Converts into simulation config
    fn to_simulation_config(&self) -> SimulationConfig {
        SimulationConfig {
            log_level: self.log_level,
            total_time: self.total_time,
            expected_pmt_amt: self.expected_pmt_amt,
            capacity_multiplier: self.capacity_multiplier,
            no_results: self.no_results,
            print_batch_size: self.print_batch_size,
            data_dir: self.data_dir.to_path_buf(),
            sim_file: self.sim_file.to_path_buf(),
            log_interval: self.log_interval,
        }
    }
}

/// Merge command line and configuration value `Cli`s
fn merge_cli() -> anyhow::Result<Cli> {
    let cli = Cli::parse();
    log::info!(
        "Configuration file: {:?}.",
        cli.config.canonicalize()?.display()
    );

    let mut cli_from_config = Cli::from_config_file(cli.config.to_path_buf())?;

    if cli.data_dir != PathBuf::from(DEFAULT_DATA_DIR) {
        log::info!("Command line arg: data_dir={:?}.", cli.data_dir);
        cli_from_config.data_dir = cli.data_dir
    }

    if cli.sim_file != PathBuf::from(DEFAULT_SIM_FILE) {
        log::info!("Command line arg: sim_file={:?}.", cli.sim_file);
        cli_from_config.sim_file = cli.sim_file
    }

    if cli.total_time != DEFAULT_TOTAL_TIME {
        log::info!("Command line arg: total_time={:?}.", cli.total_time);
        cli_from_config.total_time = cli.total_time
    }

    if cli.print_batch_size != DEFAULT_PRINT_BATCH_SIZE {
        log::info!(
            "Command line arg: print_batch_size={:?}.",
            cli.print_batch_size
        );
        cli_from_config.print_batch_size = cli.print_batch_size
    }

    if cli.log_level.as_str() != DEFAULT_LOG_LEVEL {
        log::info!("Command line arg: log_level={:?}.", cli.log_level);
        cli_from_config.log_level = cli.log_level
    }

    if cli.expected_pmt_amt != EXPECTED_PAYMENT_AMOUNT {
        log::info!(
            "Command line arg: expected_pmt_amt={:?}.",
            cli.expected_pmt_amt
        );
        cli_from_config.expected_pmt_amt = cli.expected_pmt_amt
    }

    if cli.capacity_multiplier != ACTIVITY_MULTIPLIER {
        log::info!(
            "Command line arg: capacity_multiplier={:?}.",
            cli.capacity_multiplier
        );
        cli_from_config.capacity_multiplier = cli.capacity_multiplier
    }

    if cli.no_results != DEFAULT_NO_RESULTS {
        log::info!("Command line arg: no_results={:?}.", cli.no_results);
        cli_from_config.no_results = cli.no_results
    }

    if cli.log_interval != DEFAULT_LOG_INTERVAL {
        log::info!("Command line arg: log_interval={:?}.", cli.log_interval);
        cli_from_config.log_interval = cli.log_interval;
    }

    if cli.config != PathBuf::from(DEFAULT_CONFIGURATION_FILE) {
        log::info!("Command line arg: config={:?}.", cli.config);
    }

    anyhow::Ok(cli_from_config)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let logger_handle = Logger::try_with_str("info")?
        .set_palette("b1;3;2;4;6".to_string())
        .start()?;

    let cli = merge_cli()?;
    let opts = cli.to_simulation_config();

    logger_handle.set_new_spec(
        LogSpecBuilder::new()
            .default(LevelFilter::Warn)
            .module("sim_lib", opts.log_level)
            .module("sim_cli", opts.log_level)
            .build(),
    );

    let sim_path = read_sim_path(opts.data_dir.clone(), opts.sim_file).await?;
    let SimParams { nodes, activity } =
        serde_json::from_str(&std::fs::read_to_string(sim_path)?)
            .map_err(|e| anyhow!("Could not deserialize node connection data or activity description from simulation file (line {}, col {}).", e.line(), e.column()))?;

    let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> = HashMap::new();
    let mut pk_node_map = HashMap::new();
    let mut alias_node_map = HashMap::new();

    for connection in nodes {
        // TODO: Feels like there should be a better way of doing this without having to Arc<Mutex<T>>> it at this time.
        // Box sort of works, but we won't know the size of the dyn LightningNode at compile time so the compiler will
        // scream at us when trying to create the Arc<Mutex>> later on while adding the node to the clients map
        let node: Arc<Mutex<dyn LightningNode>> = match connection {
            NodeConnection::LND(c) => Arc::new(Mutex::new(LndNode::new(c).await?)),
            NodeConnection::CLN(c) => Arc::new(Mutex::new(ClnNode::new(c).await?)),
        };

        let node_info = node.lock().await.get_info().clone();

        log::info!(
            "Connected to {} - Node ID: {}.",
            node_info.alias,
            node_info.pubkey
        );

        if clients.contains_key(&node_info.pubkey) {
            anyhow::bail!(LightningError::ValidationError(format!(
                "duplicated node: {}.",
                node_info.pubkey
            )));
        }

        if alias_node_map.contains_key(&node_info.alias) {
            anyhow::bail!(LightningError::ValidationError(format!(
                "duplicated node: {}.",
                node_info.alias
            )));
        }

        clients.insert(node_info.pubkey, node);
        pk_node_map.insert(node_info.pubkey, node_info.clone());
        alias_node_map.insert(node_info.alias.clone(), node_info);
    }

    let mut validated_activities = vec![];
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
            anyhow::bail!(LightningError::ValidationError(format!(
                "activity source {} not found in nodes.",
                act.source
            )));
        };

        let destination = match &act.destination {
            NodeId::Alias(a) => {
                if let Some(info) = alias_node_map.get(a) {
                    info.clone()
                } else {
                    anyhow::bail!(LightningError::ValidationError(format!(
                        "unknown activity destination: {}.",
                        act.destination
                    )));
                }
            },
            NodeId::PublicKey(pk) => {
                if let Some(info) = pk_node_map.get(pk) {
                    info.clone()
                } else {
                    clients
                        .get(&source.pubkey)
                        .unwrap()
                        .lock()
                        .await
                        .get_node_info(pk)
                        .await
                        .map_err(|e| {
                            log::debug!("{}", e);
                            LightningError::ValidationError(format!(
                                "Destination node unknown or invalid: {}.",
                                pk,
                            ))
                        })?
                }
            },
        };

        validated_activities.push(ActivityDefinition {
            source,
            destination,
            interval_secs: act.interval_secs,
            amount_msat: act.amount_msat,
        });
    }

    let write_results = if !opts.no_results {
        Some(WriteResults {
            results_dir: mkdir(opts.data_dir.join("results")).await?,
            batch_size: opts.print_batch_size,
        })
    } else {
        None
    };

    let sim = Simulation::new(
        clients,
        validated_activities,
        opts.total_time,
        opts.expected_pmt_amt,
        opts.capacity_multiplier,
        write_results,
    );
    let sim2 = sim.clone();

    ctrlc::set_handler(move || {
        log::info!("Shutting down simulation.");
        sim2.shutdown();
    })?;

    sim.run().await?;

    Ok(())
}

async fn read_sim_path(data_dir: PathBuf, sim_file: PathBuf) -> anyhow::Result<PathBuf> {
    let sim_path = if sim_file.is_relative() {
        data_dir.join(sim_file)
    } else {
        sim_file
    };

    if sim_path.exists() {
        Ok(sim_path)
    } else {
        log::info!("Simulation file '{}' does not exist.", sim_path.display());
        select_sim_file(data_dir).await
    }
}

async fn select_sim_file(data_dir: PathBuf) -> anyhow::Result<PathBuf> {
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

async fn mkdir(dir: PathBuf) -> anyhow::Result<PathBuf> {
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}
