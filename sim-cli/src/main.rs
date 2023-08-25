use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use clap::Parser;
use log::LevelFilter;
use sim_lib::{cln::ClnNode, lnd::LndNode, Config, LightningNode, NodeConnection, Simulation};
use simple_logger::SimpleLogger;

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[clap(long, short)]
    config: PathBuf,
    #[clap(long, short)]
    total_time: Option<u32>,
    #[clap(long, short)]
    log_level: LevelFilter,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    SimpleLogger::new()
        .with_level(LevelFilter::Warn)
        .with_module_level("sim_lib", cli.log_level)
        .with_module_level("sim_cli", cli.log_level)
        .init()
        .unwrap();

    let config_str = std::fs::read_to_string(cli.config)?;
    let Config { nodes, activity } = serde_json::from_str(&config_str)?;

    let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>> = HashMap::new();

    for connection in nodes {
        // TODO: We should simplify this into two minimal branches plus shared logging and inserting into the list
        match connection {
            NodeConnection::LND(c) => {
                let node_id = c.id;
                let lnd = LndNode::new(c).await?;

                log::info!(
                    "Connected to {} - Node ID: {}",
                    lnd.get_info().alias,
                    lnd.get_info().pubkey
                );

                clients.insert(node_id, Arc::new(Mutex::new(lnd)));
            }
            NodeConnection::CLN(c) => {
                let node_id = c.id;
                let cln = ClnNode::new(c).await?;

                log::info!(
                    "Connected to {} - Node ID: {}",
                    cln.get_info().alias,
                    cln.get_info().pubkey
                );

                clients.insert(node_id, Arc::new(Mutex::new(cln)));
            }
        }
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
