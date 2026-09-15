//! The execution layer: assembles sim.json files from the environment and scenario layers, runs
//! them through the same public entry points the sim-cli binary uses, and collects the observable
//! output for assertions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, Context};
use bitcoin::secp256k1::PublicKey;
use serde_json::{json, Value};
use sim_cli::parsing::{create_simulation, create_simulation_with_network, SimParams};
use simln_lib::clock::SimulationClock;
use simln_lib::runtime::block_on_virtual_time;
use simln_lib::sim_node::CustomRecords;
use simln_lib::{ActivityDefinition, Simulation, SimulationCfg, WriteResults};
use tokio_util::task::TaskTracker;

use crate::env::{ConfigFragment, NodeHandle};
use crate::scenario::Scenario;

/// Assembles a sim.json file from the environment and scenario layers.
pub struct SimFile {
    fragment: ConfigFragment,
    activity: Vec<Value>,
    exclude: Vec<Value>,
}

impl SimFile {
    pub fn new(fragment: ConfigFragment) -> Self {
        SimFile {
            fragment,
            activity: vec![],
            exclude: vec![],
        }
    }

    /// Populates activity and exclusions from a scenario.
    pub fn scenario(mut self, scenario: &Scenario, nodes: &[NodeHandle]) -> Self {
        self.activity = scenario.activity_json(nodes);
        self.exclude = scenario.exclude_json(nodes);
        self
    }

    /// Sets a raw activity section, for tests that intentionally write invalid references.
    pub fn activity_raw(mut self, activity: Vec<Value>) -> Self {
        self.activity = activity;
        self
    }

    /// Writes the assembled sim.json into `dir` and returns its path.
    pub fn write(self, dir: &Path) -> anyhow::Result<PathBuf> {
        let mut config = json!({});
        match self.fragment {
            ConfigFragment::RealNodes(nodes) => config["nodes"] = json!(nodes),
            ConfigFragment::SimGraph(channels) => config["sim_network"] = json!(channels),
        }
        if !self.activity.is_empty() {
            config["activity"] = json!(self.activity);
        }
        if !self.exclude.is_empty() {
            config["exclude"] = json!(self.exclude);
        }

        let path = dir.join("sim.json");
        std::fs::write(&path, serde_json::to_string_pretty(&config)?)
            .with_context(|| format!("writing sim file to {}", path.display()))?;
        Ok(path)
    }
}

/// Options controlling a simulation run, mirroring the sim-cli flags relevant to tests.
#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub total_time: Option<u32>,
    pub expected_pmt_amt: u64,
    pub capacity_multiplier: f64,
    pub fix_seed: Option<u64>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            total_time: Some(3600),
            expected_pmt_amt: 3_800_000,
            capacity_multiplier: 2.0,
            fix_seed: Some(42),
        }
    }
}

/// A payment recorded in the simulation's results CSV.
#[derive(Debug, Clone)]
pub struct PaymentRecord {
    pub source: PublicKey,
    pub destination: PublicKey,
    pub amount_msat: u64,
    pub outcome: String,
}

impl PaymentRecord {
    pub fn is_success(&self) -> bool {
        self.outcome == "Success"
    }
}

/// The observable output of a simulation run.
#[derive(Debug)]
pub struct SimOutput {
    /// The parameters as deserialized from the sim file, for assertions on parsing itself.
    pub params: SimParams,
    /// The validated activities the simulation ran with, with node references fully resolved.
    pub activities: Vec<ActivityDefinition>,
    /// Total payments dispatched, as reported by the simulation.
    pub total_payments: u64,
    /// Success rate percentage, as reported by the simulation.
    pub success_rate: f64,
    /// Per-payment records read back from the results CSV.
    pub records: Vec<PaymentRecord>,
}

/// Deserializes a sim file exactly as the sim-cli binary would.
pub fn parse_params(sim_file: &Path) -> anyhow::Result<SimParams> {
    let contents = std::fs::read_to_string(sim_file)
        .with_context(|| format!("reading sim file {}", sim_file.display()))?;
    serde_json::from_str(&contents).context("deserializing sim file")
}

/// Runs a sim file describing a simulated network to completion on virtual time, so runs bounded
/// by `total_time` finish as fast as the CPU allows. Must be called from a synchronous context
/// (not inside a tokio runtime).
pub fn run_simulated(sim_file: &Path, opts: RunOptions) -> anyhow::Result<SimOutput> {
    let params = parse_params(sim_file)?;
    let (cfg, results_dir) = simulation_cfg(sim_file, opts)?;

    let run_params = params.clone();
    // Anchor virtual time at the wall clock: the simulated graph's channel updates are stamped
    // with clock time, and pathfinding rejects gossip older than two weeks. Reproducibility of
    // payment sequences comes from the seeded RNG, not the clock anchor.
    let (activities, total_payments, success_rate) =
        block_on_virtual_time(SystemTime::now(), |clock| async move {
            let (sim, activities, _nodes) = create_simulation_with_network(
                cfg,
                &run_params,
                clock,
                TaskTracker::new(),
                vec![],
                CustomRecords::default(),
            )
            .await?;

            finish(sim, &activities)
                .await
                .map(|(total, rate)| (activities, total, rate))
        })??;

    Ok(SimOutput {
        params,
        activities,
        total_payments,
        success_rate,
        records: read_records(&results_dir)?,
    })
}

/// Runs a sim file describing real nodes to completion on wall-clock time, from within a tokio
/// runtime.
pub async fn run_real(sim_file: &Path, opts: RunOptions) -> anyhow::Result<SimOutput> {
    let params = parse_params(sim_file)?;
    let (cfg, results_dir) = simulation_cfg(sim_file, opts)?;

    let clock = Arc::new(SimulationClock::new(SystemTime::now()));
    let (sim, activities) = create_simulation(cfg, &params, clock, TaskTracker::new()).await?;
    let (total_payments, success_rate) = finish(sim, &activities).await?;

    Ok(SimOutput {
        params,
        activities,
        total_payments,
        success_rate,
        records: read_records(&results_dir)?,
    })
}

/// Builds the simulation config, creating a results directory next to the sim file so each test
/// run's CSV output is isolated with the rest of its files.
fn simulation_cfg(sim_file: &Path, opts: RunOptions) -> anyhow::Result<(SimulationCfg, PathBuf)> {
    let results_dir = sim_file
        .parent()
        .ok_or_else(|| anyhow!("sim file {} has no parent directory", sim_file.display()))?
        .join("results");
    std::fs::create_dir_all(&results_dir)?;

    let cfg = SimulationCfg::new(
        opts.total_time,
        opts.expected_pmt_amt,
        opts.capacity_multiplier,
        Some(WriteResults {
            results_dir: results_dir.clone(),
            // Flush every record so results are complete even if shutdown races the writer.
            batch_size: 1,
        }),
        opts.fix_seed,
    );

    Ok((cfg, results_dir))
}

/// Drives a configured simulation to completion and reports its aggregates.
async fn finish(
    sim: Simulation<SimulationClock>,
    activities: &[ActivityDefinition],
) -> anyhow::Result<(u64, f64)> {
    sim.run(activities).await?;
    Ok((sim.get_total_payments().await, sim.get_success_rate().await))
}

/// Reads back the payment records written to the results directory. The CSV rows are flattened
/// `(Payment, PaymentResult)` tuples with a header row: source, destination, amount_msat, hash,
/// dispatch_time, htlc_count, payment_outcome.
fn read_records(results_dir: &Path) -> anyhow::Result<Vec<PaymentRecord>> {
    let mut csv_files: Vec<PathBuf> = std::fs::read_dir(results_dir)?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "csv").then_some(path)
        })
        .collect();

    let path = match csv_files.len() {
        0 => return Err(anyhow!("no results CSV found in {}", results_dir.display())),
        1 => csv_files.remove(0),
        n => {
            return Err(anyhow!(
                "expected a single results CSV in {}, found {n}",
                results_dir.display()
            ))
        },
    };

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_path(&path)?;

    let mut records = vec![];
    for row in reader.records() {
        let row = row?;
        let field = |i: usize| -> anyhow::Result<&str> {
            row.get(i)
                .ok_or_else(|| anyhow!("results row missing field {i}: {row:?}"))
        };

        records.push(PaymentRecord {
            source: PublicKey::from_str(field(0)?).context("results row source pubkey")?,
            destination: PublicKey::from_str(field(1)?)
                .context("results row destination pubkey")?,
            amount_msat: field(2)?.parse().context("results row amount")?,
            outcome: field(6)?.to_string(),
        });
    }

    Ok(records)
}

/// Convenience lookup from pubkey to handle for assertion messages.
pub fn handles_by_pubkey(nodes: &[NodeHandle]) -> HashMap<PublicKey, &NodeHandle> {
    nodes.iter().map(|n| (n.pubkey, n)).collect()
}
