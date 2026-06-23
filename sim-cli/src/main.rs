use std::sync::Arc;
use std::time::SystemTime;

use clap::Parser;
use log::LevelFilter;
use sim_cli::parsing::{create_simulation, create_simulation_with_network, parse_sim_params, Cli};
use simln_lib::latency_interceptor::LatencyIntercepor;
use simln_lib::sim_node::Interceptor;
use simln_lib::{
    clock::SimulationClock, runtime::block_on_virtual_time, sim_node::CustomRecords,
    ActivityDefinition, Simulation, SimulationCfg,
};
use simple_logger::SimpleLogger;
use tokio_util::task::TaskTracker;

fn main() -> anyhow::Result<()> {
    // Enable tracing if building in developer mode.
    #[cfg(feature = "dev")]
    {
        console_subscriber::init();
    }

    let cli = Cli::parse();
    let sim_params = parse_sim_params(&cli)?;

    SimpleLogger::new()
        .with_level(LevelFilter::Warn)
        .with_module_level("simln_lib", cli.log_level)
        .with_module_level("sim_cli", cli.log_level)
        .init()
        .unwrap();

    cli.validate(&sim_params)?;

    let cfg = SimulationCfg::try_from(&cli)?;
    let latency = cli.latency_ms.unwrap_or(0);
    let build_and_run = move |clock: Arc<SimulationClock>| async move {
        let (sim, activities) = if sim_params.sim_network.is_empty() {
            create_simulation(cfg, &sim_params, clock, TaskTracker::new()).await?
        } else {
            let (sim, activities, _) = create_simulation_with_network(
                cfg,
                &sim_params,
                clock,
                TaskTracker::new(),
                // Create an interceptor to add latency to payments, otherwise none.
                if latency > 0 {
                    vec![Arc::new(LatencyIntercepor::new_poisson(
                        latency as f32,
                        cli.fix_seed,
                    )?) as Arc<dyn Interceptor>]
                } else {
                    vec![]
                },
                CustomRecords::default(),
            )
            .await?;
            (sim, activities)
        };

        run_simulation(sim, activities).await
    };

    // For virtual time, our helper will build the clock we need and pass it to build_and_run. If
    // running with regular time, we can just pass a clock right in.
    if cli.virtual_time {
        block_on_virtual_time(SystemTime::now(), build_and_run)?
    } else {
        tokio::runtime::Runtime::new()?.block_on(build_and_run(Arc::new(SimulationClock::new(
            SystemTime::now(),
        ))))
    }
}

/// Drives a fully-configured simulation to completion, wiring up a ctrl-c handler that triggers a clean shutdown.
async fn run_simulation(
    sim: Simulation<SimulationClock>,
    validated_activities: Vec<ActivityDefinition>,
) -> anyhow::Result<()> {
    let sim2 = sim.clone();
    ctrlc::set_handler(move || {
        log::info!("Shutting down simulation.");
        sim2.shutdown();
    })?;

    sim.run(&validated_activities).await?;

    Ok(())
}
