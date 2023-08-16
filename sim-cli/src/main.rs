use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use clap::Parser;
use sim_lib::{lnd::LndNode, Config, LightningNode, Simulation};

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[clap(long, short)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let config_str = std::fs::read_to_string(cli.config)?;
    let Config { nodes, activity } = serde_json::from_str(&config_str)?;

    let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>> = HashMap::new();

    for node in nodes {
        let lnd = LndNode::new(node.address, node.macaroon, node.cert).await?;

        let node_info = lnd.get_info().await?;
        println!("Node info {:?}", node_info);

        clients.insert(node.id, Arc::new(Mutex::new(lnd)));
    }

    let sim = Simulation::new(clients, activity);
    sim.run().await
}
