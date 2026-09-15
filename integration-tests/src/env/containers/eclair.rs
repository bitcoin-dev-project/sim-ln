//! Eclair running in ACINQ's image, driven over its REST API from the host.
//!
//! Two quirks relative to the other nodes: the image is amd64-only (it runs under emulation on
//! Apple Silicon, so it gets generous timeouts) and only ships a moving `latest` tag, so it is
//! pinned by digest. Configuration goes through `JAVA_OPTS` because the image's shell-form
//! entrypoint ignores container args. Eclair has no on-chain wallet of its own — it spends
//! directly from bitcoind's wallet, so it needs no separate funding.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context};
use bitcoin::secp256k1::PublicKey;
use serde_json::{json, Value};
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use super::bitcoind::{ZMQ_HASHBLOCK_PORT, ZMQ_RAWTX_PORT};
use super::{dump_logs, BTC_RPC_PASS, BTC_RPC_USER, CHANNEL_SIZE_SAT, P2P_PORT};
use crate::env::{NodeHandle, NodeImpl};
use crate::retry::{with_backoff, Backoff};

const IMAGE: &str = "acinq/eclair";
// Versioned tags stopped at 0.8.0; `latest` is the maintained tag, pinned here by digest
// (0.14.1 at the time of pinning).
const TAG: &str = "latest@sha256:6eb7d528bc150822231d7d73cc6f27942d02c1682aa0e22c7a2b0b2dc3249aa3";

const API_PORT: u16 = 8080;
const API_PASSWORD: &str = "simln-api";
const ALIAS: &str = "eclair";

pub struct EclairHarness {
    container: ContainerAsync<GenericImage>,
    name: String,
    pub pubkey: PublicKey,
    http: reqwest::Client,
    api_base: String,
    api_host_port: u16,
}

impl EclairHarness {
    pub async fn start(docker_network: &str, bitcoind_name: &str) -> anyhow::Result<Self> {
        let name = format!("simln-eclair-{}", std::process::id());
        let java_opts = [
            // ACINQ's image is built from the post-release "Back to dev" commit, which carries
            // an unconditional guard against running dev builds. The guard checks nothing
            // dynamic — this opt-out is required to start the image at all.
            "-Declair.allow-unsafe-startup=true".to_string(),
            "-Declair.chain=regtest".to_string(),
            format!("-Declair.node-alias={ALIAS}"),
            format!("-Declair.server.port={P2P_PORT}"),
            "-Declair.api.enabled=true".to_string(),
            "-Declair.api.binding-ip=0.0.0.0".to_string(),
            format!("-Declair.api.port={API_PORT}"),
            format!("-Declair.api.password={API_PASSWORD}"),
            format!("-Declair.bitcoind.host={bitcoind_name}"),
            "-Declair.bitcoind.rpcport=18443".to_string(),
            format!("-Declair.bitcoind.rpcuser={BTC_RPC_USER}"),
            format!("-Declair.bitcoind.rpcpassword={BTC_RPC_PASS}"),
            "-Declair.bitcoind.wallet=simln".to_string(),
            // Eclair's zmqblock endpoint expects hashblock, not rawblock.
            format!("-Declair.bitcoind.zmqblock=tcp://{bitcoind_name}:{ZMQ_HASHBLOCK_PORT}"),
            format!("-Declair.bitcoind.zmqtx=tcp://{bitcoind_name}:{ZMQ_RAWTX_PORT}"),
            "-Declair.features.keysend=optional".to_string(),
            // CLN -> Eclair keysend interop needs a min final expiry of at least CLN's 22-block
            // default, with the fulfill safety margin strictly below it.
            "-Declair.channel.min-final-expiry-delta-blocks=24".to_string(),
            "-Declair.channel.fulfill-safety-before-timeout-blocks=12".to_string(),
            // Zero fees so multi-hop payments through this node never hit fee budgets.
            "-Declair.relay.fees.public-channels.fee-base-msat=0".to_string(),
            "-Declair.relay.fees.public-channels.fee-proportional-millionths=0".to_string(),
        ]
        .join(" ");

        let container = GenericImage::new(IMAGE, TAG)
            .with_exposed_port(API_PORT.tcp())
            .with_network(docker_network)
            .with_container_name(&name)
            .with_startup_timeout(Duration::from_secs(600))
            .with_env_var("JAVA_OPTS", java_opts)
            .start()
            .await
            .context("starting eclair container")?;

        let init = async {
            let api_host_port = container.get_host_port_ipv4(API_PORT).await?;
            let api_base = format!("http://127.0.0.1:{api_host_port}");
            let http = reqwest::Client::new();

            // The JVM (under emulation on Apple Silicon) takes a while; poll until the API
            // answers getinfo.
            let info = with_backoff("eclair api ready", Backoff::slow(), || async {
                api_call(&http, &api_base, "getinfo", &[]).await
            })
            .await?;

            let pubkey = info
                .get("nodeId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("getinfo response missing nodeId: {info}"))?
                .parse()
                .context("parsing eclair pubkey")?;
            anyhow::Ok((http, api_base, api_host_port, pubkey))
        };

        match init.await {
            Ok((http, api_base, api_host_port, pubkey)) => Ok(EclairHarness {
                container,
                name,
                pubkey,
                http,
                api_base,
                api_host_port,
            }),
            Err(e) => {
                dump_logs("eclair", &container).await;
                Err(e.context("eclair startup"))
            },
        }
    }

    async fn api(&self, endpoint: &str, params: &[(&str, String)]) -> anyhow::Result<Value> {
        api_call(&self.http, &self.api_base, endpoint, params).await
    }

    pub fn container(&self) -> &ContainerAsync<GenericImage> {
        &self.container
    }

    pub fn container_name(&self) -> &str {
        &self.name
    }

    pub fn node_handle(&self) -> NodeHandle {
        NodeHandle {
            pubkey: self.pubkey,
            alias: ALIAS.to_string(),
            implementation: NodeImpl::Eclair,
        }
    }

    pub fn sim_config(&self) -> Value {
        json!({
            "id": self.pubkey.to_string(),
            "base_url": format!("http://127.0.0.1:{}", self.api_host_port),
            "api_username": "",
            "api_password": API_PASSWORD,
        })
    }

    /// Connects and opens an announced channel, retrying until bitcoind's wallet (which eclair
    /// spends from) can fund it.
    pub async fn open_channel(&self, peer: &PublicKey, peer_addr: &str) -> anyhow::Result<()> {
        with_backoff("eclair open channel", Backoff::slow(), || async {
            let _ = self
                .api("connect", &[("uri", format!("{peer}@{peer_addr}"))])
                .await;

            self.api(
                "open",
                &[
                    ("nodeId", peer.to_string()),
                    ("fundingSatoshis", CHANNEL_SIZE_SAT.to_string()),
                    ("announceChannel", "true".to_string()),
                ],
            )
            .await
        })
        .await?;
        Ok(())
    }

    pub async fn active_channel_count(&self) -> anyhow::Result<usize> {
        let channels = self.api("channels", &[]).await?;
        let channels = channels
            .as_array()
            .ok_or_else(|| anyhow!("channels response is not an array: {channels}"))?;
        Ok(channels
            .iter()
            .filter(|c| c.get("state").and_then(Value::as_str) == Some("NORMAL"))
            .count())
    }

    pub async fn graph_synced(&self, nodes: usize, channels: usize) -> anyhow::Result<()> {
        let node_count = self
            .api("nodes", &[])
            .await?
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);
        let channel_count = self
            .api("allchannels", &[])
            .await?
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);

        if node_count >= nodes && channel_count >= channels {
            Ok(())
        } else {
            Err(anyhow!(
                "graph has {node_count}/{nodes} nodes, {channel_count}/{channels} channels"
            ))
        }
    }

    pub async fn settled_keysend_count(&self) -> anyhow::Result<u64> {
        let payments = self.api("listreceivedpayments", &[]).await?;
        let payments = payments
            .as_array()
            .ok_or_else(|| anyhow!("listreceivedpayments is not an array: {payments}"))?;
        Ok(payments
            .iter()
            .filter(|p| {
                p.get("paymentType").and_then(Value::as_str) == Some("KeySend")
                    && p.pointer("/status/type").and_then(Value::as_str) == Some("received")
            })
            .count() as u64)
    }
}

async fn api_call(
    http: &reqwest::Client,
    base: &str,
    endpoint: &str,
    params: &[(&str, String)],
) -> anyhow::Result<Value> {
    let mut form = HashMap::new();
    for (key, value) in params {
        form.insert(*key, value.clone());
    }

    let response = http
        .post(format!("{base}/{endpoint}"))
        .basic_auth("", Some(API_PASSWORD))
        .form(&form)
        .send()
        .await
        .with_context(|| format!("eclair {endpoint} request"))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("eclair {endpoint} returned {status}: {body}"));
    }

    serde_json::from_str(&body).with_context(|| format!("eclair {endpoint} response: {body}"))
}
