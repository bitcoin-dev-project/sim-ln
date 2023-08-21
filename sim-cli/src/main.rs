use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use clap::Parser;
use log::LevelFilter;
use sim_lib::{lnd::LndNode, Config, LightningNode, LndNodeConnection, NodeConnection, Simulation};
use simple_logger::SimpleLogger;

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[clap(long, short)]
    config: PathBuf,
    #[clap(long, short)]
    total_time: Option<u32>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    SimpleLogger::new()
        .with_level(LevelFilter::Warn)
        .with_module_level("sim_lib", LevelFilter::Info)
        .with_module_level("sim_cli", LevelFilter::Debug)
        .init()
        .unwrap();

    let cli = Cli::parse();

    let config_str = std::fs::read_to_string(cli.config)?;
    let Config { nodes, activity } = serde_json::from_str(&config_str)?;

    let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>> = HashMap::new();

    for node in nodes {
        let (ln, id) = match node {
            NodeConnection::LND(LndNodeConnection {
                address,
                macaroon,
                cert,
                id,
            }) => (LndNode::new(address, macaroon, cert).await?, id),
        };

        log::info!(
            "Connected to {} - Node ID: {}",
            ln.get_info().alias,
            ln.get_info().pubkey
        );

        clients.insert(id, Arc::new(Mutex::new(ln)));
    }

    let sim = Simulation::new(clients, activity, cli.total_time);
    let sim2 = sim.clone();

    ctrlc::set_handler(move || {
        log::info!("Shutting down simulation...");
        sim2.shutdown();
    })?;

    sim.run().await?;

    Ok(())
}
