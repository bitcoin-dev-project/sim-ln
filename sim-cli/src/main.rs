use bitcoin::secp256k1::PublicKey;
use flexi_logger::{LogSpecBuilder, Logger};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use anyhow::anyhow;
use log::LevelFilter;
use sim_lib::{
    cln::ClnNode, lnd::LndNode, ActivityDefinition, LightningError, LightningNode, NodeConnection, NodeId, SimParams,
    Simulation,
};

mod cli;
use cli::{merge_cli, DEFAULT_LOG_LEVEL};

/// Colour pallette of the logger
const COLOUR_PALETTE: &str = "b1;3;2;4;6";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let logger_handle = Logger::try_with_str(DEFAULT_LOG_LEVEL)?
        .set_palette(COLOUR_PALETTE.to_string())
        .start()?;

    let cli = merge_cli()?;
    let opts = cli.to_simulation_config();

    logger_handle.set_new_spec(
        LogSpecBuilder::new()
            .default(LevelFilter::Warn)
            .module("sim_lib", opts.log_level)
            .module("sim_cli", opts.log_level)
            .build(),
    );

    let sim_path = read_sim_path(opts.data_dir.clone(), opts.sim_file.clone()).await?;
    let SimParams { nodes, activity } = serde_json::from_str(&std::fs::read_to_string(sim_path)?).map_err(|e| {
        anyhow!(
            "Nodes or activities not parsed from simulation file (line {}, col {}).",
            e.line(),
            e.column()
        )
    })?;

    let mut clients: HashMap<PublicKey, Arc<Mutex<dyn LightningNode>>> = HashMap::new();
    let mut pk_node_map = HashMap::new();
    let mut alias_node_map = HashMap::new();

    for connection in nodes {
        // TODO: Feels like there should be a better way of doing this without having to Arc<Mutex<T>>> it at this time.
        // Box sort of works, but we won't know the size of the dyn LightningNode at compile time so the compiler will
        // scream at us when trying to create the Arc<Mutex>> later on while adding the node to the clients map
        let node: Arc<Mutex<dyn LightningNode>> = match connection {
            NodeConnection::LND(c) => Arc::new(Mutex::new(LndNode::new(c).await?)),
            NodeConnection::CLN(c) => Arc::new(Mutex::new(ClnNode::new(c).await?)),
        };

        let node_info = node.lock().await.get_info().clone();

        log::info!("Connected to {} - Node ID: {}.", node_info.alias, node_info.pubkey);

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

        clients.insert(node_info.pubkey, node);
        pk_node_map.insert(node_info.pubkey, node_info.clone());
        alias_node_map.insert(node_info.alias.clone(), node_info);
    }

    let mut validated_activities = vec![];
    // Make all the activities identifiable by PK internally
    for act in activity.into_iter() {
        // We can only map aliases to nodes we control, so if either the source or destination alias
        // is not in alias_node_map, we fail
        let source = if let Some(source) = match &act.source {
            NodeId::PublicKey(pk) => pk_node_map.get(pk),
            NodeId::Alias(a) => alias_node_map.get(a),
        } {
            source.clone()
        } else {
            anyhow::bail!(LightningError::ValidationError(format!(
                "activity source {} not found in nodes.",
                act.source
            )));
        };

        let destination = match &act.destination {
            NodeId::Alias(a) => {
                if let Some(info) = alias_node_map.get(a) {
                    info.clone()
                } else {
                    anyhow::bail!(LightningError::ValidationError(format!(
                        "unknown activity destination: {}.",
                        act.destination
                    )));
                }
            },
            NodeId::PublicKey(pk) => {
                if let Some(info) = pk_node_map.get(pk) {
                    info.clone()
                } else {
                    clients
                        .get(&source.pubkey)
                        .unwrap()
                        .lock()
                        .await
                        .get_node_info(pk)
                        .await
                        .map_err(|e| {
                            log::debug!("{}", e);
                            LightningError::ValidationError(format!("Destination node unknown or invalid: {}.", pk,))
                        })?
                }
            },
        };

        validated_activities.push(ActivityDefinition {
            source,
            destination,
            interval_secs: act.interval_secs,
            amount_msat: act.amount_msat,
        });
    }

    let sim = Simulation::new(clients, validated_activities, opts).await?;
    let sim2 = sim.clone();

    ctrlc::set_handler(move || {
        log::info!("Shutting down simulation.");
        sim2.shutdown();
    })?;

    sim.run().await?;

    Ok(())
}

async fn read_sim_path(data_dir: PathBuf, sim_file: PathBuf) -> anyhow::Result<PathBuf> {
    let sim_path = if sim_file.is_relative() {
        data_dir.join(sim_file)
    } else {
        sim_file
    };

    if sim_path.exists() {
        Ok(sim_path)
    } else {
        log::info!("Simulation file '{}' does not exist.", sim_path.display());
        select_sim_file(data_dir).await
    }
}

async fn select_sim_file(data_dir: PathBuf) -> anyhow::Result<PathBuf> {
    let sim_files = std::fs::read_dir(data_dir.clone())?
        .filter_map(|f| {
            f.ok().and_then(|f| {
                if f.path().extension()?.to_str()? == "json" {
                    f.file_name().into_string().ok()
                } else {
                    None
                }
            })
        })
        .collect::<Vec<_>>();

    if sim_files.is_empty() {
        anyhow::bail!("no simulation files found in {}.", data_dir.canonicalize()?.display());
    }

    let selection = dialoguer::Select::new()
        .with_prompt(format!(
            "Select a simulation file. Found these in {}",
            data_dir.canonicalize()?.display()
        ))
        .items(&sim_files)
        .default(0)
        .interact()?;

    Ok(data_dir.join(sim_files[selection].clone()))
}
