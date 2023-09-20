use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use clap::Parser;
use log::LevelFilter;
use sim_lib::{
    cln::ClnNode, lnd::LndNode, ActivityDefinition, Config, LightningError, LightningNode,
    NodeConnection, NodeId, Simulation,
};
use simple_logger::SimpleLogger;

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[clap(long, short)]
    config: PathBuf,
    #[clap(long, short)]
    total_time: Option<u32>,
    /// Number of activity results to batch together before printing to csv file
    #[clap(long, short)]
    print_batch_size: Option<u32>,
    #[clap(long, short, default_value = "info")]
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
    let Config {
        nodes,
        mut activity,
    } = serde_json::from_str(&config_str)?;

    let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode + Send>>> = HashMap::new();
    let mut alias_node_map = HashMap::new();

    for connection in nodes {
        // TODO: Feels like there should be a better way of doing this without having to Arc<Mutex<T>>> it at this time.
        // Box sort of works, but we won't know the size of the dyn LightningNode at compile time so the compiler will
        // scream at us when trying to create the Arc<Mutex>> later on while adding the node to the clients map
        let node: Arc<Mutex<dyn LightningNode + Send>> = match connection {
            NodeConnection::LND(c) => Arc::new(Mutex::new(LndNode::new(c).await?)),
            NodeConnection::CLN(c) => Arc::new(Mutex::new(ClnNode::new(c).await?)),
        };

        let node_info = node.lock().await.get_info().clone();

        log::info!(
            "Connected to {} - Node ID: {}.",
            node_info.alias,
            node_info.pubkey
        );

        if clients.contains_key(&node_info.pubkey) {
            anyhow::bail!(LightningError::ValidationError(format!(
                "duplicated node: {}.",
                node_info.pubkey
            )));
        }

        if alias_node_map.contains_key(&node_info.alias) {
            anyhow::bail!(LightningError::ValidationError(format!(
                "duplicated node: {}.",
                node_info.alias
            )));
        }

        alias_node_map.insert(node_info.alias.clone(), node_info.pubkey);
        clients.insert(node_info.pubkey, node);
    }

    let mut validated_activities = vec![];
    // Make all the activities identifiable by PK internally
    for act in activity.iter_mut() {
        // We can only map aliases to nodes we control, so if either the source or destination alias
        // is not in alias_node_map, we fail
        if let NodeId::Alias(a) = &act.source {
            if let Some(pk) = alias_node_map.get(a) {
                act.source = NodeId::PublicKey(*pk);
            } else {
                anyhow::bail!(LightningError::ValidationError(format!(
                    "activity source alias {} not found in nodes.",
                    act.source
                )));
            }
        }
        if let NodeId::Alias(a) = &act.destination {
            if let Some(pk) = alias_node_map.get(a) {
                act.destination = NodeId::PublicKey(*pk);
            } else {
                anyhow::bail!(LightningError::ValidationError(format!(
                    "unknown activity destination: {}.",
                    act.destination
                )));
            }
        }
        validated_activities.push(ActivityDefinition::try_from(act)?);
    }

    let sim = Simulation::new(
        clients,
        validated_activities,
        cli.total_time,
        cli.print_batch_size,
    );
    let sim2 = sim.clone();

    ctrlc::set_handler(move || {
        log::info!("Shutting down simulation.");
        sim2.shutdown();
    })?;

    sim.run().await?;

    Ok(())
}
