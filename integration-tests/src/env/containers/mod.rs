//! A heterogeneous real-node network run in docker containers: bitcoind plus one node each of
//! LND, CLN, Eclair and ldk-server, connected in a ring of announced channels.
//!
//! Startup is error resistant by construction: every step that depends on a node becoming ready
//! polls with backoff (see [`crate::retry`]) rather than assuming readiness, and each harness
//! dumps its container's logs when its own startup fails so errors point at the node that never
//! came up.

pub mod bitcoind;
pub mod cln;
pub mod eclair;
pub mod ldk_server;
pub mod lnd;

use std::path::Path;

use anyhow::{anyhow, Context};
use serde_json::Value;
use testcontainers::core::{CmdWaitFor, ExecCommand};
use testcontainers::{ContainerAsync, GenericImage};

use super::{ConfigFragment, NodeHandle, TestNetwork};
use crate::retry::{with_backoff, Backoff};

/// The p2p listening port used by every lightning node container.
pub const P2P_PORT: u16 = 9735;

/// Size of every channel in the ring.
pub const CHANNEL_SIZE_SAT: u64 = 1_000_000;

/// On-chain amount each channel-opening node is funded with.
const ONCHAIN_FUND_SAT: u64 = 10_000_000;

/// Shared credentials for bitcoind RPC, used by every node's chain backend connection.
pub const BTC_RPC_USER: &str = "user";
pub const BTC_RPC_PASS: &str = "pass";

/// Runs a command in a container and returns its raw stdout, failing on non-zero exit.
pub(crate) async fn exec_stdout(
    container: &ContainerAsync<GenericImage>,
    cmd: &[&str],
) -> anyhow::Result<Vec<u8>> {
    let mut result = container
        .exec(
            ExecCommand::new(cmd.iter().copied())
                .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )
        .await
        .with_context(|| format!("exec {cmd:?}"))?;
    result
        .stdout_to_vec()
        .await
        .with_context(|| format!("reading stdout of {cmd:?}"))
}

/// Copies a file out of a container into `dest` on the host, polling until it exists and is
/// non-empty (files like macaroons and certs are created asynchronously on node startup).
pub(crate) async fn extract_file(
    container: &ContainerAsync<GenericImage>,
    container_path: &str,
    dest: &Path,
    backoff: Backoff,
) -> anyhow::Result<Vec<u8>> {
    let contents = with_backoff(
        &format!("waiting for {container_path} in container"),
        backoff,
        || async {
            match exec_stdout(container, &["cat", container_path]).await {
                Ok(bytes) if !bytes.is_empty() => Ok(bytes),
                Ok(_) => Err(anyhow!("{container_path} exists but is empty")),
                Err(e) => Err(e),
            }
        },
    )
    .await?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, &contents)
        .with_context(|| format!("writing {} to host", dest.display()))?;
    Ok(contents)
}

/// Prints the tail of a container's logs, used when a node fails to become ready or a scenario
/// fails, so CI output includes the node-side view of the failure.
pub(crate) async fn dump_logs(name: &str, container: &ContainerAsync<GenericImage>) {
    for (stream, bytes) in [
        ("stdout", container.stdout_to_vec().await),
        ("stderr", container.stderr_to_vec().await),
    ] {
        let text = match bytes {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => format!("<failed to read logs: {e}>"),
        };
        let tail: Vec<&str> = text.lines().rev().take(100).collect();
        eprintln!("===== {name} {stream} (last {} lines) =====", tail.len());
        for line in tail.iter().rev() {
            eprintln!("{line}");
        }
    }
}

/// The full heterogeneous network. Nodes are indexed in ring order: 0 = LND, 1 = CLN,
/// 2 = Eclair, 3 = ldk-server, with channels 0->1->2->3->0.
pub struct RealNetwork {
    handles: Vec<NodeHandle>,
    node_configs: Vec<Value>,
    pub bitcoind: bitcoind::Bitcoind,
    pub lnd: lnd::LndHarness,
    pub cln: cln::ClnHarness,
    pub eclair: eclair::EclairHarness,
    pub ldk: ldk_server::LdkHarness,
}

impl RealNetwork {
    /// Brings up the whole network: bitcoind, all four nodes in parallel, on-chain funding, the
    /// ring of channels, and finally waits for every node to see the full graph. Extracted
    /// credentials (certs, macaroons) are written under `creds_dir`, which must outlive the
    /// simulations run against this network.
    pub async fn start(creds_dir: &Path) -> anyhow::Result<Self> {
        let docker_network = format!("simln-itest-{}", std::process::id());

        let bitcoind = bitcoind::Bitcoind::start(&docker_network).await?;

        // The four nodes are independent of each other until channels open: start them in
        // parallel since (particularly with cold image pulls) startup dominates runtime.
        let (lnd, cln, eclair, ldk) = tokio::try_join!(
            lnd::LndHarness::start(&docker_network, bitcoind.container_name(), creds_dir),
            cln::ClnHarness::start(&docker_network, bitcoind.container_name(), creds_dir),
            eclair::EclairHarness::start(&docker_network, bitcoind.container_name()),
            ldk_server::LdkHarness::start(&docker_network, bitcoind.container_name(), creds_dir),
        )?;

        // Fund every node that opens a channel from its own wallet. Eclair spends directly from
        // bitcoind's wallet, which is already funded from mining.
        let addresses = [
            lnd.new_address().await?,
            cln.new_address().await?,
            ldk.new_address().await?,
        ];
        bitcoind.fund(&addresses, ONCHAIN_FUND_SAT)?;

        // Open the ring. Each open polls with backoff because it can only succeed once the
        // opener's wallet has seen the funding confirmation.
        lnd.open_channel(&cln.pubkey, &cln.p2p_address()).await?;
        cln.open_channel(&eclair.pubkey, eclair.container_name(), P2P_PORT)
            .await?;
        eclair.open_channel(&ldk.pubkey, &ldk.p2p_address()).await?;
        ldk.open_channel(&lnd.pubkey, &lnd.p2p_address()).await?;

        // Confirm the funding transactions deeply enough for the channels to be announced. Some
        // implementations broadcast their funding transaction asynchronously after the open call
        // returns (ldk-node batches broadcasts), so await_ready keeps mining while it waits
        // rather than relying on this one round of confirmations.
        bitcoind.mine(6)?;

        let result = Self::await_ready(&bitcoind, &lnd, &cln, &eclair, &ldk).await;
        let network = RealNetwork {
            handles: vec![
                lnd.node_handle(),
                cln.node_handle(),
                eclair.node_handle(),
                ldk.node_handle(),
            ],
            node_configs: vec![
                lnd.sim_config(),
                cln.sim_config(),
                eclair.sim_config(),
                ldk.sim_config(),
            ],
            bitcoind,
            lnd,
            cln,
            eclair,
            ldk,
        };

        if let Err(e) = result {
            network.dump_all_logs().await;
            return Err(e);
        }
        Ok(network)
    }

    /// Waits until every node reports both of its ring channels active, and until every node's
    /// own gossip view contains the full network (4 channels, 4 node announcements). Gossip
    /// propagation is the slowest and least predictable step, so it gets the long schedule.
    async fn await_ready(
        bitcoind: &bitcoind::Bitcoind,
        lnd: &lnd::LndHarness,
        cln: &cln::ClnHarness,
        eclair: &eclair::EclairHarness,
        ldk: &ldk_server::LdkHarness,
    ) -> anyhow::Result<()> {
        with_backoff("ring channels active", Backoff::slow(), || async {
            // Nudge any funding transaction that was broadcast after the initial confirmation
            // round towards confirmation; extra regtest blocks are harmless.
            bitcoind.mine(1)?;

            let (lnd_active, cln_active, eclair_active, ldk_active) = tokio::try_join!(
                lnd.active_channel_count(),
                cln.active_channel_count(),
                eclair.active_channel_count(),
                ldk.channel_count(),
            )?;
            if [lnd_active, cln_active, eclair_active, ldk_active] == [2, 2, 2, 2] {
                Ok(())
            } else {
                Err(anyhow!(
                    "channels active: lnd {lnd_active}/2, cln {cln_active}/2, \
                     eclair {eclair_active}/2, ldk {ldk_active}/2"
                ))
            }
        })
        .await?;

        with_backoff("lnd graph sync", Backoff::slow(), || async {
            lnd.graph_synced(4, 4).await
        })
        .await?;
        with_backoff("cln graph sync", Backoff::slow(), || async {
            cln.graph_synced(4, 4).await
        })
        .await?;
        with_backoff("eclair graph sync", Backoff::slow(), || async {
            eclair.graph_synced(4, 4).await
        })
        .await?;
        with_backoff("ldk-server graph sync", Backoff::slow(), || async {
            ldk.graph_synced(4, 4).await
        })
        .await?;

        Ok(())
    }

    /// The number of settled inbound keysend payments the node at ring index `idx` reports, from
    /// the node's own books — used to verify receipt independently of sim-ln's records.
    pub async fn settled_keysend_count(&self, idx: usize) -> anyhow::Result<u64> {
        match idx {
            0 => self.lnd.settled_keysend_count().await,
            1 => self.cln.settled_keysend_count().await,
            2 => self.eclair.settled_keysend_count().await,
            3 => self.ldk.settled_keysend_count().await,
            n => Err(anyhow!("no node at ring index {n}")),
        }
    }

    /// Dumps the tail of every container's logs to test output.
    pub async fn dump_all_logs(&self) {
        dump_logs("bitcoind", self.bitcoind.container()).await;
        dump_logs("lnd", self.lnd.container()).await;
        dump_logs("cln", self.cln.container()).await;
        dump_logs("eclair", self.eclair.container()).await;
        dump_logs("ldk-server", self.ldk.container()).await;
    }
}

impl TestNetwork for RealNetwork {
    fn nodes(&self) -> &[NodeHandle] {
        &self.handles
    }

    fn config_fragment(&self) -> ConfigFragment {
        ConfigFragment::RealNodes(self.node_configs.clone())
    }
}
