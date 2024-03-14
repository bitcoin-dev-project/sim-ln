use std::{collections::HashMap, fmt::Debug, path::PathBuf, str::FromStr};

use clap::{builder::TypedValueParser, Parser};
use config::{Config, File};
use log::LevelFilter;
use serde::Deserialize;
use sim_lib::SimulationConfig;

/// The default directory where the simulation files are stored and where the results will be written to.
pub const DEFAULT_DATA_DIR: &str = ".";

/// The default simulation file to be used by the simulator.
pub const DEFAULT_SIM_FILE: &str = "sim.json";

/// The default expected payment amount for the simulation, around ~$10 at the time of writing.
pub const EXPECTED_PAYMENT_AMOUNT: u64 = 3_800_000;

/// The number of times over each node in the network sends its total deployed capacity in a calendar month.
pub const ACTIVITY_MULTIPLIER: f64 = 2.0;

/// Default batch size to flush result data to disk
pub const DEFAULT_PRINT_BATCH_SIZE: u32 = 500;

/// Default configuration file
pub const DEFAULT_CONFIGURATION_FILE: &str = "conf.ini";

/// Default total time
pub const DEFAULT_TOTAL_TIME: Option<u32> = None;

/// Default log level
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// Default no results
pub const DEFAULT_NO_RESULTS: bool = false;

/// Default log interval
pub const DEFAULT_LOG_INTERVAL: u64 = 60;

/// Configuration header
pub const CONFIG_HEADER: &str = "simln.conf";

/// Workspace root directory
pub const WORKSPACE_ROOT_DIR: &str = ".";

/// Deserializes a f64 as long as it is positive and greater than 0.
fn deserialize_f64_greater_than_zero(x: String) -> Result<f64, String> {
    match x.parse::<f64>() {
        Ok(x) => {
            if x > 0.0 {
                Ok(x)
            } else {
                Err(format!("capacity_multiplier must be higher than 0. {x} received."))
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

    let level_filter = LevelFilter::from_str(&level_filter_str)
        .map_err(|e| serde::de::Error::custom(format!("Failed to deserialize LevelFilter from &str: {e}")))?;

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
pub struct Cli {
    /// Path to a directory containing simulation files, and where simulation results will be stored
    #[clap(long, short, default_value = DEFAULT_DATA_DIR)]
    data_dir: PathBuf,
    /// Path to the simulation file to be used by the simulator
    /// This can either be an absolute path, or relative path with respect to data_dir
    #[clap(long, short, default_value = DEFAULT_SIM_FILE)]
    sim_file: PathBuf,
    /// Total time the simulator will be running
    #[clap(long, short)]
    #[serde(deserialize_with = "deserialize_total_time")]
    total_time: Option<u32>,
    /// Number of activity results to batch together before printing to csv file [min: 1]
    #[clap(
        long,
        short,
        default_value_t = DEFAULT_PRINT_BATCH_SIZE,
        value_parser = clap::builder::RangedU64ValueParser::<u32>::new().range(1..u32::MAX as u64)
    )]
    print_batch_size: u32,
    /// Level of verbosity of the messages displayed by the simulator.
    /// Possible values: [off, error, warn, info, debug, trace]
    #[clap(long, short, verbatim_doc_comment, default_value = DEFAULT_LOG_LEVEL)]
    #[serde(deserialize_with = "deserialize_log_level")]
    log_level: LevelFilter,
    /// Expected payment amount for the random activity generator
    #[clap(
        long,
        short,
        default_value_t = EXPECTED_PAYMENT_AMOUNT,
        value_parser = clap::builder::RangedU64ValueParser::<u64>::new().range(1..u64::MAX)
    )]
    expected_pmt_amt: u64,
    /// Multiplier of the overall network capacity used by the random activity generator
    #[clap(
        long,
        short,
        default_value_t = ACTIVITY_MULTIPLIER,
        value_parser = clap::builder::StringValueParser::new().try_map(deserialize_f64_greater_than_zero)
    )]
    capacity_multiplier: f64,
    /// Do not create an output file containing the simulations results
    #[clap(long, default_value_t = DEFAULT_NO_RESULTS)]
    no_results: bool,
    /// Sets a custom configuration (INI) file
    #[clap(long, short = 'C', value_name = "CONFIG_FILE", default_value = DEFAULT_CONFIGURATION_FILE)]
    config: PathBuf,
    #[clap(
        long,
        short = 'L',
        default_value_t = DEFAULT_LOG_INTERVAL,
        value_parser = clap::builder::RangedU64ValueParser::<u32>::new().range(1..u32::MAX as u64)
    )]
    log_interval: u64,
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
            config: DEFAULT_CONFIGURATION_FILE.into(),
            log_interval: DEFAULT_LOG_INTERVAL,
        }
    }
}

impl Cli {
    /// Creates Cli from a configuration if provided, else it defaults to
    /// CLI default values
    fn from_config_file(file_path: PathBuf) -> anyhow::Result<Self> {
        let default_cli = Cli::default();

        if file_path == PathBuf::from(DEFAULT_CONFIGURATION_FILE) {
            let config_path_res = default_config_path(WORKSPACE_ROOT_DIR.into(), file_path.clone());
            if config_path_res.is_err() {
                log::info!("Default configuration file: {file_path:?} (not found, skipping).");
                return anyhow::Ok(default_cli);
            }
        }

        let simln_conf = Config::builder()
            .add_source(File::with_name(file_path.as_os_str().to_string_lossy().as_ref()))
            .build()?
            .get_table(CONFIG_HEADER)?;

        let config_cli = Cli {
            data_dir: from_config_field(&simln_conf, "data_dir", DEFAULT_DATA_DIR.into()),
            sim_file: from_config_field(&simln_conf, "sim_file", DEFAULT_SIM_FILE.into()),
            total_time: from_config_field(&simln_conf, "total_time", DEFAULT_TOTAL_TIME),
            print_batch_size: from_config_field(&simln_conf, "print_batch_size", DEFAULT_PRINT_BATCH_SIZE),
            log_level: LevelFilter::from_str(&from_config_field::<String>(
                &simln_conf,
                "log_level",
                DEFAULT_LOG_LEVEL.to_string(),
            ))?,
            expected_pmt_amt: from_config_field(&simln_conf, "expected_pmt_amt", EXPECTED_PAYMENT_AMOUNT),
            capacity_multiplier: from_config_field(&simln_conf, "capacity_multiplier", ACTIVITY_MULTIPLIER),
            no_results: from_config_field(&simln_conf, "no_results", DEFAULT_NO_RESULTS),
            log_interval: from_config_field(&simln_conf, "log_interval", DEFAULT_LOG_INTERVAL),
            config: default_cli.config,
        };

        anyhow::Ok(config_cli)
    }

    /// Converts into simulation config
    pub fn to_simulation_config(&self) -> SimulationConfig {
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

    /// Overwrites Cli with another
    fn overwrite_with(&mut self, cli: &Cli) {
        macro_rules! overwrite_field {
            ($cli:ident, $field:ident, $default:expr) => {
                if $cli.$field != $default {
                    log::info!("Command line arg: {}={:?}.", stringify!($field), $cli.$field);
                    self.$field = $cli.$field.clone();
                }
            };
        }

        overwrite_field!(cli, data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        overwrite_field!(cli, sim_file, PathBuf::from(DEFAULT_SIM_FILE));
        overwrite_field!(cli, total_time, DEFAULT_TOTAL_TIME);
        overwrite_field!(cli, print_batch_size, DEFAULT_PRINT_BATCH_SIZE);
        overwrite_field!(cli, log_level, LevelFilter::Info);
        overwrite_field!(cli, expected_pmt_amt, EXPECTED_PAYMENT_AMOUNT);
        overwrite_field!(cli, capacity_multiplier, ACTIVITY_MULTIPLIER);
        overwrite_field!(cli, no_results, DEFAULT_NO_RESULTS);
        overwrite_field!(cli, log_interval, DEFAULT_LOG_INTERVAL);
        overwrite_field!(cli, config, PathBuf::from(DEFAULT_CONFIGURATION_FILE));
    }
}

/// Helper function to parse field_name from the config map. Defaults to the
/// field_name's default if deserialization fails
fn from_config_field<'de, T>(config_map: &HashMap<String, config::Value>, field_name: &str, default_value: T) -> T
where
    T: Clone + Deserialize<'de> + Debug,
{
    config_map.get(field_name).map_or(default_value.clone(), |value| {
        let res = value.clone().try_deserialize();
        match res {
            Ok(result) => {
                log::info!("Configuration file arg: {field_name}={result:?}.");
                result
            },
            Err(e) => {
                log::error!("Failed to parse {field_name}. Error => {e:?}.");
                log::info!("Default value {:?} used.", default_value);
                default_value
            },
        }
    })
}

/// Merge command line and configuration value `Cli`s
pub fn merge_cli() -> anyhow::Result<Cli> {
    let cli = Cli::parse();
    log::info!("Configuration file: {:?}.", cli.config.display());
    let mut cli_from_config = Cli::from_config_file(cli.config.clone())?;
    cli_from_config.overwrite_with(&cli);

    anyhow::Ok(cli_from_config)
}

fn default_config_path(root_dir: PathBuf, config_file: PathBuf) -> anyhow::Result<PathBuf> {
    let conf_path = if config_file.is_relative() {
        root_dir.join(config_file)
    } else {
        config_file
    };

    if conf_path.exists() {
        Ok(conf_path)
    } else {
        anyhow::bail!("Default configuration file not found.")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::cli::from_config_field;

    #[test]
    fn test_from_config_field() {
        let mut config_map = HashMap::new();
        config_map.insert("key".to_string(), config::Value::new(None, "value"));

        let value = from_config_field(&config_map, "key", "default_value".to_string());
        let default_value = from_config_field(&config_map, "no_key", "default_value");

        assert_eq!(value, "value");
        assert_eq!(default_value, "default_value");
    }
}
