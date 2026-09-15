//! LND running in the official lightninglabs image, driven over gRPC from the host.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use bitcoin::secp256k1::PublicKey;
use serde_json::{json, Value};
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tonic_lnd::lnrpc;

use super::bitcoind::{ZMQ_RAWBLOCK_PORT, ZMQ_RAWTX_PORT};
use super::{dump_logs, extract_file, BTC_RPC_PASS, BTC_RPC_USER, CHANNEL_SIZE_SAT, P2P_PORT};
use crate::env::{NodeHandle, NodeImpl};
use crate::retry::{with_backoff, Backoff};

const IMAGE: &str = "lightninglabs/lnd";
const TAG: &str = "v0.21.1-beta";

const GRPC_PORT: u16 = 10009;
const ALIAS: &str = "lnd";

pub struct LndHarness {
    container: ContainerAsync<GenericImage>,
    name: String,
    pub pubkey: PublicKey,
    client: tonic_lnd::Client,
    grpc_host_port: u16,
    cert_path: PathBuf,
    macaroon_path: PathBuf,
}

impl LndHarness {
    pub async fn start(
        docker_network: &str,
        bitcoind_name: &str,
        creds_dir: &Path,
    ) -> anyhow::Result<Self> {
        let name = format!("simln-lnd-{}", std::process::id());
        let container = GenericImage::new(IMAGE, TAG)
            .with_exposed_port(GRPC_PORT.tcp())
            .with_network(docker_network)
            .with_container_name(&name)
            .with_startup_timeout(Duration::from_secs(600))
            .with_cmd([
                "--noseedbackup",
                "--bitcoin.regtest",
                "--bitcoin.node=bitcoind",
                &format!("--bitcoind.rpchost={bitcoind_name}:18443"),
                &format!("--bitcoind.rpcuser={BTC_RPC_USER}"),
                &format!("--bitcoind.rpcpass={BTC_RPC_PASS}"),
                &format!("--bitcoind.zmqpubrawblock=tcp://{bitcoind_name}:{ZMQ_RAWBLOCK_PORT}"),
                &format!("--bitcoind.zmqpubrawtx=tcp://{bitcoind_name}:{ZMQ_RAWTX_PORT}"),
                &format!("--rpclisten=0.0.0.0:{GRPC_PORT}"),
                &format!("--listen=0.0.0.0:{P2P_PORT}"),
                "--accept-keysend",
                &format!("--alias={ALIAS}"),
                // Zero out routing fees so multi-hop payments through this node never hit
                // sender-side fee budgets.
                "--bitcoin.basefee=0",
                "--bitcoin.feerate=0",
                // The TLS cert is generated on first boot; make it validate both for other
                // containers (by name) and for the host dialing the mapped port.
                &format!("--tlsextradomain={name}"),
                "--tlsextraip=127.0.0.1",
            ])
            .start()
            .await
            .context("starting lnd container")?;

        let init = async {
            let cert_path = creds_dir.join("lnd/tls.cert");
            let macaroon_path = creds_dir.join("lnd/admin.macaroon");
            extract_file(
                &container,
                "/root/.lnd/tls.cert",
                &cert_path,
                Backoff::slow(),
            )
            .await?;
            extract_file(
                &container,
                "/root/.lnd/data/chain/bitcoin/regtest/admin.macaroon",
                &macaroon_path,
                Backoff::slow(),
            )
            .await?;

            let grpc_host_port = container.get_host_port_ipv4(GRPC_PORT).await?;
            let address = format!("https://127.0.0.1:{grpc_host_port}");

            let client = with_backoff("lnd grpc connect", Backoff::slow(), || async {
                tonic_lnd::connect(address.clone(), &cert_path, &macaroon_path).await
            })
            .await?;

            let info = with_backoff("lnd synced to chain", Backoff::slow(), || {
                let mut client = client.clone();
                async move {
                    let info = client
                        .lightning()
                        .get_info(lnrpc::GetInfoRequest {})
                        .await
                        .map_err(|e| anyhow!(e.to_string()))?
                        .into_inner();
                    if info.synced_to_chain {
                        Ok(info)
                    } else {
                        Err(anyhow!("not yet synced to chain"))
                    }
                }
            })
            .await?;

            let pubkey = info.identity_pubkey.parse().context("parsing lnd pubkey")?;
            anyhow::Ok((client, pubkey, grpc_host_port, cert_path, macaroon_path))
        };

        match init.await {
            Ok((client, pubkey, grpc_host_port, cert_path, macaroon_path)) => Ok(LndHarness {
                container,
                name,
                pubkey,
                client,
                grpc_host_port,
                cert_path,
                macaroon_path,
            }),
            Err(e) => {
                dump_logs("lnd", &container).await;
                Err(e.context("lnd startup"))
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
            implementation: NodeImpl::Lnd,
        }
    }

    /// The `nodes` entry for sim.json, connecting from the host through the mapped port.
    pub fn sim_config(&self) -> Value {
        json!({
            "id": self.pubkey.to_string(),
            "address": format!("https://127.0.0.1:{}", self.grpc_host_port),
            "macaroon": self.macaroon_path.to_string_lossy(),
            "cert": self.cert_path.to_string_lossy(),
        })
    }

    pub async fn new_address(&self) -> anyhow::Result<String> {
        let mut client = self.client.clone();
        let response = client
            .lightning()
            .new_address(lnrpc::NewAddressRequest {
                r#type: lnrpc::AddressType::TaprootPubkey as i32,
                ..Default::default()
            })
            .await?
            .into_inner();
        Ok(response.address)
    }

    /// Connects to the peer and opens an announced channel, retrying until the wallet has seen
    /// its funding confirm.
    pub async fn open_channel(&self, peer: &PublicKey, peer_addr: &str) -> anyhow::Result<()> {
        with_backoff("lnd open channel", Backoff::slow(), || async {
            let mut client = self.client.clone();
            // Connecting to an already-connected peer errors; the open below is the real check.
            let _ = client
                .lightning()
                .connect_peer(lnrpc::ConnectPeerRequest {
                    addr: Some(lnrpc::LightningAddress {
                        pubkey: peer.to_string(),
                        host: peer_addr.to_string(),
                    }),
                    ..Default::default()
                })
                .await;

            client
                .lightning()
                .open_channel_sync(lnrpc::OpenChannelRequest {
                    node_pubkey: peer.serialize().to_vec(),
                    local_funding_amount: CHANNEL_SIZE_SAT as i64,
                    ..Default::default()
                })
                .await
                .map_err(|e| anyhow!(e.to_string()))
        })
        .await?;
        Ok(())
    }

    pub async fn active_channel_count(&self) -> anyhow::Result<usize> {
        let mut client = self.client.clone();
        let response = client
            .lightning()
            .list_channels(lnrpc::ListChannelsRequest {
                active_only: true,
                ..Default::default()
            })
            .await?
            .into_inner();
        Ok(response.channels.len())
    }

    /// Checks that this node's graph view has at least the given node and channel counts.
    pub async fn graph_synced(&self, nodes: usize, channels: usize) -> anyhow::Result<()> {
        let mut client = self.client.clone();
        let graph = client
            .lightning()
            .describe_graph(lnrpc::ChannelGraphRequest {
                include_unannounced: false,
            })
            .await?
            .into_inner();
        if graph.nodes.len() >= nodes && graph.edges.len() >= channels {
            Ok(())
        } else {
            Err(anyhow!(
                "graph has {}/{nodes} nodes, {}/{channels} channels",
                graph.nodes.len(),
                graph.edges.len()
            ))
        }
    }

    pub async fn settled_keysend_count(&self) -> anyhow::Result<u64> {
        let mut client = self.client.clone();
        let response = client
            .lightning()
            .list_invoices(lnrpc::ListInvoiceRequest {
                num_max_invoices: 10_000,
                ..Default::default()
            })
            .await?
            .into_inner();
        Ok(response
            .invoices
            .iter()
            .filter(|i| i.is_keysend && i.state == lnrpc::invoice::InvoiceState::Settled as i32)
            .count() as u64)
    }
}
