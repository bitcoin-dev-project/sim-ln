use anyhow::anyhow;
use clap::Parser;
use log::LevelFilter;
use sim_cli::parsing::{create_simulation, create_simulation_with_network, read_sim_path, Cli};
use simple_logger::SimpleLogger;
use tokio_util::task::TaskTracker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Enable tracing if building in developer mode.
    #[cfg(feature = "dev")]
    {
        console_subscriber::init();
    }

    let cli = Cli::parse();

    SimpleLogger::new()
        .with_level(LevelFilter::Warn)
        .with_module_level("simln_lib", cli.log_level)
        .with_module_level("sim_cli", cli.log_level)
        .init()
        .unwrap();

    let sim_path = read_sim_path(cli.data_dir.clone(), cli.sim_file.clone()).await?;
    let sim_params = serde_json::from_str(&std::fs::read_to_string(sim_path)?).map_err(|e| {
        anyhow!(
            "Could not deserialize node connection data or activity description from simulation file (line {}, col {}, err: {}).",
            e.line(),
            e.column(),
            e.to_string()
        )
        })?;

    cli.validate(&sim_params)?;

    let tasks = TaskTracker::new();

    let sim = if sim_params.sim_network.is_empty() {
        create_simulation(&cli, &sim_params, tasks.clone()).await?
    } else {
        create_simulation_with_network(&cli, &sim_params, tasks.clone()).await?
    };
    let sim2 = sim.clone();

    ctrlc::set_handler(move || {
        log::info!("Shutting down simulation.");
        sim2.shutdown();
    })?;

    sim.run().await?;
    tasks.wait().await;

    Ok(())
}
