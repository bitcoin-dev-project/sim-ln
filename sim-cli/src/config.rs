use std::path::PathBuf;

use config::{Config, File};
use log::LevelFilter;
use serde::Deserialize;

use crate::Cli;

/// Overwrites the fields on a SimulationConfig if corresponding arguments are supplied via the command line
macro_rules! cli_overwrite {
    ($config:expr, $cli:expr, $($field:ident),*) => {
        match (&mut $config, $cli) {
            (config_struct, cli_struct) => {
                $(
                    if let Some(value) = cli_struct.$field {
                        log::info!(
                            "SimulatinConfig::{} {:?} overwritten by CLI argument {:?}",
                            stringify!($field),
                            config_struct.$field,
                            value
                        );
                        config_struct.$field = value;
                    }
                )*
            }
        }

    };
}

#[derive(Debug, Deserialize)]
pub struct SimulationConfig {
    /// Path to a directory where simulation results are save
    pub data_dir: PathBuf,
    /// Path to simulation file
    pub sim_file: PathBuf,
    /// Duration for which the simulation should run
    #[serde(deserialize_with = "serde_config::deserialize_total_time")]
    pub total_time: Option<u32>,
    /// Number of activity results to batch together before printing to csv file [min: 1]
    pub print_batch_size: u32,
    /// Level of verbosity of the messages displayed by the simulator.
    /// Possible values: [off, error, warn, info, debug, trace]
    #[serde(deserialize_with = "serde_config::deserialize_log_level")]
    pub log_level: LevelFilter,
    /// Expected payment amount for the random activity generator
    pub expected_pmt_amt: u64,
    /// Multiplier of the overall network capacity used by the random activity generator
    pub capacity_multiplier: f64,
    /// Do not create an output file containing the simulations results
    pub no_results: bool,
    /// Duration after which results are logged
    pub log_interval: u64,
}

/// Custom deserializers for SimulationConfig fields
mod serde_config {
    use std::str::FromStr;

    use log::LevelFilter;
    use serde::Deserialize;

    /// Custom deserialization function for `LevelFilter`. Required because `LevelFilter` does
    /// not implement deserialize
    pub fn deserialize_log_level<'de, D>(deserializer: D) -> Result<log::LevelFilter, D::Error>
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
}

impl SimulationConfig {
    /// The default simulation configuration file with default simulation values
    pub const DEFAULT_SIM_CONFIG_FILE: &'static str = "conf.ini";

    /// Loads the simulation configuration from a path and overwrites loaded values with
    /// CLI arguments
    pub fn load(path: &PathBuf, cli: Cli) -> anyhow::Result<Self> {
        // 1. Load simulation config from specified path
        let path_str = if let Some(path_str) = path.to_str() {
            path_str
        } else {
            anyhow::bail!("Failed to convert path {path:?} to &str.")
        };

        let config = Config::builder()
            .add_source(File::with_name(path_str))
            .build()?;
        let mut sim_conf: SimulationConfig = config.try_deserialize()?;

        // 2. Overwrite config values with CLI arguments (if passed)
        if let Some(total_time) = cli.total_time {
            log::info!(
                "SimulatinConfig::total_time {:?} overwritten by CLI argument {}",
                sim_conf.total_time,
                total_time
            );
            sim_conf.total_time = Some(total_time)
        };

        cli_overwrite!(
            sim_conf,
            cli,
            data_dir,
            sim_file,
            print_batch_size,
            log_level,
            expected_pmt_amt,
            capacity_multiplier,
            no_results
        );

        Ok(sim_conf)
    }
}

#[cfg(test)]
mod test {
    use std::{path::PathBuf, str::FromStr};

    use crate::Cli;

    use super::SimulationConfig;

    #[test]
    fn overwrite_config_with_cli() {
        let test_cli = Cli {
            data_dir: Some(
                PathBuf::from_str("data").expect("Failed to create test data directory"),
            ),
            sim_file: Some(
                PathBuf::from_str("sim.json").expect("Failed to create test simulation file"),
            ),
            total_time: Some(60),
            print_batch_size: Some(10),
            log_level: Some(log::LevelFilter::Debug),
            expected_pmt_amt: Some(500),
            capacity_multiplier: Some(3.142),
            no_results: Some(true),
        };

        let mut test_config = SimulationConfig {
            data_dir: PathBuf::from_str("simulation.data")
                .expect("Failed to create test config data directory"),
            sim_file: PathBuf::from_str("simulation.sim.json")
                .expect("Failed to create test config simulation file"),
            total_time: None,
            print_batch_size: 200,
            log_level: log::LevelFilter::Trace,
            expected_pmt_amt: 500,
            capacity_multiplier: 5.5,
            no_results: true,
            log_interval: 10,
        };

        cli_overwrite!(
            test_config,
            test_cli.clone(),
            data_dir,
            sim_file,
            print_batch_size,
            log_level,
            expected_pmt_amt,
            capacity_multiplier,
            no_results
        );

        assert_eq!(Some(test_config.data_dir), test_cli.data_dir);
        assert_eq!(Some(test_config.sim_file), test_cli.sim_file);
        assert_eq!(
            Some(test_config.print_batch_size),
            test_cli.print_batch_size
        );
        assert_eq!(Some(test_config.log_level), test_cli.log_level);
        assert_eq!(
            Some(test_config.expected_pmt_amt),
            test_cli.expected_pmt_amt
        );
        assert_eq!(
            Some(test_config.capacity_multiplier),
            test_cli.capacity_multiplier
        );
        assert_eq!(Some(test_config.no_results), test_cli.no_results);
    }

    #[test]
    fn replace_config_with_cli_args() {
        let cli = Cli {
            data_dir: Some(
                PathBuf::from_str("data").expect("Failed to create test data directory"),
            ),
            sim_file: Some(
                PathBuf::from_str("sim.json").expect("Failed to create test simulation file"),
            ),
            total_time: Some(60),
            print_batch_size: Some(10),
            log_level: Some(log::LevelFilter::Debug),
            expected_pmt_amt: Some(500),
            capacity_multiplier: Some(3.142),
            no_results: Some(true),
        };

        let conf = SimulationConfig::load(&PathBuf::from("../conf.ini"), cli.clone())
            .expect("Failed to create simulation config");

        assert_eq!(Some(conf.data_dir), cli.data_dir);
        assert_eq!(Some(conf.sim_file), cli.sim_file);
        assert_eq!(conf.total_time, cli.total_time);
        assert_eq!(Some(conf.print_batch_size), cli.print_batch_size);
        assert_eq!(Some(conf.log_level), cli.log_level);
        assert_eq!(Some(conf.expected_pmt_amt), cli.expected_pmt_amt);
        assert_eq!(Some(conf.capacity_multiplier), cli.capacity_multiplier);
        assert_eq!(Some(conf.no_results), cli.no_results);
    }
}
