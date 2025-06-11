use std::sync::Arc;

use clap::Parser;
use log::LevelFilter;
use sim_cli::parsing::{create_simulation, create_simulation_with_network, parse_sim_params, Cli};
use simln_lib::{
    latency_interceptor::LatencyIntercepor,
    sim_node::{CustomRecords, Interceptor},
    SimulationCfg,
};
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
    let sim_params = parse_sim_params(&cli).await?;

    SimpleLogger::new()
        .with_level(LevelFilter::Warn)
        .with_module_level("simln_lib", cli.log_level)
        .with_module_level("sim_cli", cli.log_level)
        .init()
        .unwrap();

    cli.validate(&sim_params)?;

    let tasks = TaskTracker::new();

    let (sim, validated_activities) = if sim_params.sim_network.is_empty() {
        create_simulation(&cli, &sim_params, tasks.clone()).await?
    } else {
        let latency = cli.latency_ms.unwrap_or(0);
        let interceptors = if latency > 0 {
            vec![Arc::new(LatencyIntercepor::new_poisson(latency as f32)?) as Arc<dyn Interceptor>]
        } else {
            vec![]
        };
        let sim_cfg: SimulationCfg = SimulationCfg::try_from(&cli)?;
        create_simulation_with_network(
            sim_cfg,
            &sim_params,
            cli.speedup_clock.unwrap_or(1),
            tasks.clone(),
            interceptors,
            CustomRecords::default(),
        )
        .await?
    };
    let sim2 = sim.clone();

    ctrlc::set_handler(move || {
        log::info!("Shutting down simulation.");
        sim2.shutdown();
    })?;

    sim.run(&validated_activities).await?;

    Ok(())
}
