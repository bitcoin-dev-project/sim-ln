use clap::Parser;
use log::LevelFilter;
use sim_cli::parsing::{create_simulation, get_validated_activities, parse_sim_params, Cli};
use simple_logger::SimpleLogger;

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

    let (sim, nodes_info) = create_simulation(&cli, &sim_params).await?;
    let sim2 = sim.clone();

    ctrlc::set_handler(move || {
        log::info!("Shutting down simulation.");
        sim2.shutdown();
    })?;

    let validated_activities =
        get_validated_activities(&sim.nodes, nodes_info, sim_params.activity).await?;

    sim.run(&validated_activities).await?;

    Ok(())
}
