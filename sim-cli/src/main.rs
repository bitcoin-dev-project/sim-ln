use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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

    let mut clients: HashMap<PublicKey, Arc<dyn LightningNode>> = HashMap::new();

    for node in nodes {
        let lnd = LndNode::new(node.address, node.macaroon, node.cert).await?;
        clients.insert(node.id, Arc::new(lnd));
    }

    let sim = Simulation::new(clients, activity);
    sim.run().await
}
