//! ldk-server built from the upstream repository at the rev this workspace's client dependency
//! pins, and driven over its authenticated gRPC API from the host.
//!
//! There is no published ldk-server image: the harness builds one with `docker build` against the
//! upstream git URL (using upstream's own Dockerfile) unless `SIMLN_LDK_SERVER_IMAGE` names a
//! prebuilt image — CI sets it to reuse a cached build.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use bitcoin::secp256k1::PublicKey;
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::ldk_server_grpc::api::{
    GetNodeInfoRequest, GraphListChannelsRequest, GraphListNodesRequest, ListChannelsRequest,
    ListPaymentsRequest, OnchainReceiveRequest, OpenChannelRequest,
};
use ldk_server_client::ldk_server_grpc::types::{payment_kind, PaymentDirection, PaymentStatus};
use serde_json::{json, Value};
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use super::{
    dump_logs, exec_stdout, extract_file, BTC_RPC_PASS, BTC_RPC_USER, CHANNEL_SIZE_SAT, P2P_PORT,
};
use crate::env::{NodeHandle, NodeImpl};
use crate::retry::{with_backoff, Backoff};

/// The rev the workspace's ldk-server-client dependency pins; the server is built from the same
/// rev so client and server always match.
const LDK_SERVER_REV: &str = "8163f4fe139368613959bf4f10b19ee6a5b9b4ab";
const LDK_SERVER_REPO: &str = "https://github.com/lightningdevkit/ldk-server.git";

/// Env var naming a prebuilt ldk-server image to use instead of building one.
const IMAGE_ENV: &str = "SIMLN_LDK_SERVER_IMAGE";

const GRPC_PORT: u16 = 3536;
const ALIAS: &str = "ldk";
const STORAGE_DIR: &str = "/data/ldk-server";
const CONFIG_PATH: &str = "/config/ldk-server.toml";

pub struct LdkHarness {
    container: ContainerAsync<GenericImage>,
    name: String,
    pub pubkey: PublicKey,
    client: LdkServerClient,
    grpc_host_port: u16,
    api_key_hex: String,
    cert_path: PathBuf,
}

impl LdkHarness {
    pub async fn start(
        docker_network: &str,
        bitcoind_name: &str,
        creds_dir: &Path,
    ) -> anyhow::Result<Self> {
        let (image, tag) = ensure_image().await?;

        let config = format!(
            r#"[node]
network = "regtest"
listening_addresses = ["0.0.0.0:{P2P_PORT}"]
alias = "{ALIAS}"

[storage.disk]
dir_path = "{STORAGE_DIR}"

[log]
level = "Info"
log_to_file = false

[bitcoind]
rpc_address = "{bitcoind_name}:18443"
rpc_user = "{BTC_RPC_USER}"
rpc_password = "{BTC_RPC_PASS}"
"#
        );

        let name = format!("simln-ldk-{}", std::process::id());
        let container = GenericImage::new(image, tag)
            .with_exposed_port(GRPC_PORT.tcp())
            .with_network(docker_network)
            .with_container_name(&name)
            .with_startup_timeout(Duration::from_secs(600))
            .with_copy_to(CONFIG_PATH, config.into_bytes())
            .with_cmd([CONFIG_PATH])
            .start()
            .await
            .context("starting ldk-server container")?;

        let init = async {
            // The api key and TLS cert are generated on first startup and are exactly what the
            // client needs, so waiting for them doubles as a readiness check.
            let cert_path = creds_dir.join("ldk/tls.crt");
            let cert_pem = extract_file(
                &container,
                &format!("{STORAGE_DIR}/tls.crt"),
                &cert_path,
                Backoff::slow(),
            )
            .await?;
            // The api key is 32 raw bytes on disk; clients use its lowercase hex encoding.
            let api_key_hex = hex::encode(
                with_backoff(
                    "waiting for ldk-server api key",
                    Backoff::slow(),
                    || async {
                        let bytes = exec_stdout(
                            &container,
                            &["cat", &format!("{STORAGE_DIR}/regtest/api_key")],
                        )
                        .await?;
                        if bytes.is_empty() {
                            Err(anyhow!("api key file empty"))
                        } else {
                            Ok(bytes)
                        }
                    },
                )
                .await?,
            );

            let grpc_host_port = container.get_host_port_ipv4(GRPC_PORT).await?;
            let client = LdkServerClient::new(
                format!("127.0.0.1:{grpc_host_port}"),
                api_key_hex.clone(),
                &cert_pem,
            )
            .map_err(|e| anyhow!("creating ldk-server client: {e}"))?;

            let info = with_backoff("ldk-server ready", Backoff::slow(), || async {
                client.get_node_info(GetNodeInfoRequest {}).await
            })
            .await?;

            let pubkey = info.node_id.parse().context("parsing ldk-server pubkey")?;
            anyhow::Ok((client, pubkey, grpc_host_port, api_key_hex, cert_path))
        };

        match init.await {
            Ok((client, pubkey, grpc_host_port, api_key_hex, cert_path)) => Ok(LdkHarness {
                container,
                name,
                pubkey,
                client,
                grpc_host_port,
                api_key_hex,
                cert_path,
            }),
            Err(e) => {
                dump_logs("ldk-server", &container).await;
                Err(e.context("ldk-server startup"))
            },
        }
    }

    pub fn container(&self) -> &ContainerAsync<GenericImage> {
        &self.container
    }

    pub fn p2p_address(&self) -> String {
        format!("{}:{P2P_PORT}", self.name)
    }

    pub fn node_handle(&self) -> NodeHandle {
        NodeHandle {
            pubkey: self.pubkey,
            alias: ALIAS.to_string(),
            implementation: NodeImpl::LdkServer,
        }
    }

    pub fn sim_config(&self) -> Value {
        json!({
            "address": format!("https://127.0.0.1:{}", self.grpc_host_port),
            "api_key": self.api_key_hex,
            "cert": self.cert_path.to_string_lossy(),
        })
    }

    pub async fn new_address(&self) -> anyhow::Result<String> {
        let response = self
            .client
            .onchain_receive(OnchainReceiveRequest {})
            .await
            .map_err(|e| anyhow!("ldk-server onchain_receive: {e}"))?;
        Ok(response.address)
    }

    pub async fn open_channel(&self, peer: &PublicKey, peer_addr: &str) -> anyhow::Result<()> {
        with_backoff("ldk-server open channel", Backoff::slow(), || async {
            self.client
                .open_channel(OpenChannelRequest {
                    node_pubkey: peer.to_string(),
                    address: peer_addr.to_string(),
                    channel_amount_sats: CHANNEL_SIZE_SAT,
                    announce_channel: true,
                    ..Default::default()
                })
                .await
        })
        .await?;
        Ok(())
    }

    pub async fn channel_count(&self) -> anyhow::Result<usize> {
        let response = self
            .client
            .list_channels(ListChannelsRequest {})
            .await
            .map_err(|e| anyhow!("ldk-server list_channels: {e}"))?;
        Ok(response.channels.len())
    }

    pub async fn graph_synced(&self, nodes: usize, channels: usize) -> anyhow::Result<()> {
        let node_count = self
            .client
            .graph_list_nodes(GraphListNodesRequest {})
            .await
            .map_err(|e| anyhow!("ldk-server graph_list_nodes: {e}"))?
            .node_ids
            .len();
        let channel_count = self
            .client
            .graph_list_channels(GraphListChannelsRequest {})
            .await
            .map_err(|e| anyhow!("ldk-server graph_list_channels: {e}"))?
            .short_channel_ids
            .len();

        if node_count >= nodes && channel_count >= channels {
            Ok(())
        } else {
            Err(anyhow!(
                "graph has {node_count}/{nodes} nodes, {channel_count}/{channels} channels"
            ))
        }
    }

    pub async fn settled_keysend_count(&self) -> anyhow::Result<u64> {
        let mut count = 0u64;
        let mut page_token = None;
        loop {
            let response = self
                .client
                .list_payments(ListPaymentsRequest { page_token })
                .await
                .map_err(|e| anyhow!("ldk-server list_payments: {e}"))?;

            count += response
                .payments
                .iter()
                .filter(|p| {
                    p.direction == PaymentDirection::Inbound as i32
                        && p.status == PaymentStatus::Succeeded as i32
                        && matches!(
                            p.kind.as_ref().and_then(|k| k.kind.as_ref()),
                            Some(payment_kind::Kind::Spontaneous(_))
                        )
                })
                .count() as u64;

            match response.next_page_token {
                Some(token) => page_token = Some(token),
                None => return Ok(count),
            }
        }
    }
}

/// Returns the (image, tag) to run, building the image from the pinned upstream rev if neither a
/// `SIMLN_LDK_SERVER_IMAGE` override nor a previously built image is available.
async fn ensure_image() -> anyhow::Result<(String, String)> {
    if let Ok(image) = std::env::var(IMAGE_ENV) {
        return split_image(&image);
    }

    let tag = format!("simln-ldk-server:{}", &LDK_SERVER_REV[..12]);
    let exists = tokio::process::Command::new("docker")
        .args(["image", "inspect", &tag])
        .output()
        .await
        .context("checking for ldk-server image")?
        .status
        .success();

    if !exists {
        eprintln!("building ldk-server image from {LDK_SERVER_REPO}#{LDK_SERVER_REV} (one-time, takes a few minutes)...");
        let output = tokio::process::Command::new("docker")
            .args([
                "build",
                "-t",
                &tag,
                &format!("{LDK_SERVER_REPO}#{LDK_SERVER_REV}"),
            ])
            .output()
            .await
            .context("running docker build for ldk-server")?;
        if !output.status.success() {
            return Err(anyhow!(
                "docker build of ldk-server failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }

    split_image(&tag)
}

fn split_image(image: &str) -> anyhow::Result<(String, String)> {
    match image.rsplit_once(':') {
        Some((name, tag)) => Ok((name.to_string(), tag.to_string())),
        None => Ok((image.to_string(), "latest".to_string())),
    }
}
