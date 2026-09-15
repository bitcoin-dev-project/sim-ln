//! Core Lightning running in the official elementsproject image, driven over its mTLS gRPC
//! interface from the host.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use bitcoin::secp256k1::PublicKey;
use cln_grpc::pb;
use cln_grpc::pb::node_client::NodeClient;
use serde_json::{json, Value};
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

use super::{dump_logs, extract_file, BTC_RPC_PASS, BTC_RPC_USER, CHANNEL_SIZE_SAT, P2P_PORT};
use crate::env::{NodeHandle, NodeImpl};
use crate::retry::{with_backoff, Backoff};

const IMAGE: &str = "elementsproject/lightningd";
const TAG: &str = "v26.06.6";

const GRPC_PORT: u16 = 9736;
const ALIAS: &str = "cln";

pub struct ClnHarness {
    container: ContainerAsync<GenericImage>,
    name: String,
    pub pubkey: PublicKey,
    channel: Channel,
    grpc_host_port: u16,
    ca_path: PathBuf,
    client_cert_path: PathBuf,
    client_key_path: PathBuf,
}

impl ClnHarness {
    pub async fn start(
        docker_network: &str,
        bitcoind_name: &str,
        creds_dir: &Path,
    ) -> anyhow::Result<Self> {
        let name = format!("simln-cln-{}", std::process::id());
        let container = GenericImage::new(IMAGE, TAG)
            .with_exposed_port(GRPC_PORT.tcp())
            .with_network(docker_network)
            .with_container_name(&name)
            .with_startup_timeout(Duration::from_secs(600))
            .with_env_var("LIGHTNINGD_NETWORK", "regtest")
            .with_cmd([
                format!("--bitcoin-rpcconnect={bitcoind_name}"),
                "--bitcoin-rpcport=18443".to_string(),
                format!("--bitcoin-rpcuser={BTC_RPC_USER}"),
                format!("--bitcoin-rpcpassword={BTC_RPC_PASS}"),
                format!("--bind-addr=0.0.0.0:{P2P_PORT}"),
                "--grpc-host=0.0.0.0".to_string(),
                format!("--grpc-port={GRPC_PORT}"),
                format!("--alias={ALIAS}"),
                // Zero fees so multi-hop payments through this node never hit fee budgets.
                "--fee-base=0".to_string(),
                "--fee-per-satoshi=0".to_string(),
            ])
            .start()
            .await
            .context("starting cln container")?;

        let init = async {
            let ca_path = creds_dir.join("cln/ca.pem");
            let client_cert_path = creds_dir.join("cln/client.pem");
            let client_key_path = creds_dir.join("cln/client-key.pem");

            let ca = extract_file(
                &container,
                "/root/.lightning/regtest/ca.pem",
                &ca_path,
                Backoff::slow(),
            )
            .await?;
            let cert = extract_file(
                &container,
                "/root/.lightning/regtest/client.pem",
                &client_cert_path,
                Backoff::slow(),
            )
            .await?;
            let key = extract_file(
                &container,
                "/root/.lightning/regtest/client-key.pem",
                &client_key_path,
                Backoff::slow(),
            )
            .await?;

            let grpc_host_port = container.get_host_port_ipv4(GRPC_PORT).await?;
            // The server cert's SANs are "cln" and "localhost"; override the domain the same way
            // sim-ln's connector does since we dial by IP.
            let tls = ClientTlsConfig::new()
                .domain_name("cln")
                .ca_certificate(Certificate::from_pem(&ca))
                .identity(Identity::from_pem(&cert, &key));

            let channel = with_backoff("cln grpc connect", Backoff::slow(), || async {
                Channel::from_shared(format!("https://127.0.0.1:{grpc_host_port}"))?
                    .tls_config(tls.clone())?
                    .connect()
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;

            let info = with_backoff("cln synced", Backoff::slow(), || async {
                let info = NodeClient::new(channel.clone())
                    .getinfo(pb::GetinfoRequest {})
                    .await
                    .map_err(|e| anyhow!(e.to_string()))?
                    .into_inner();
                if info.warning_bitcoind_sync.is_none() && info.warning_lightningd_sync.is_none() {
                    Ok(info)
                } else {
                    Err(anyhow!("still syncing with bitcoind"))
                }
            })
            .await?;

            let pubkey = PublicKey::from_slice(&info.id).context("parsing cln pubkey")?;
            anyhow::Ok((
                channel,
                pubkey,
                grpc_host_port,
                ca_path,
                client_cert_path,
                client_key_path,
            ))
        };

        match init.await {
            Ok((channel, pubkey, grpc_host_port, ca_path, client_cert_path, client_key_path)) => {
                Ok(ClnHarness {
                    container,
                    name,
                    pubkey,
                    channel,
                    grpc_host_port,
                    ca_path,
                    client_cert_path,
                    client_key_path,
                })
            },
            Err(e) => {
                dump_logs("cln", &container).await;
                Err(e.context("cln startup"))
            },
        }
    }

    fn client(&self) -> NodeClient<Channel> {
        NodeClient::new(self.channel.clone())
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
            implementation: NodeImpl::Cln,
        }
    }

    pub fn sim_config(&self) -> Value {
        json!({
            "id": self.pubkey.to_string(),
            "address": format!("https://127.0.0.1:{}", self.grpc_host_port),
            "ca_cert": self.ca_path.to_string_lossy(),
            "client_cert": self.client_cert_path.to_string_lossy(),
            "client_key": self.client_key_path.to_string_lossy(),
        })
    }

    pub async fn new_address(&self) -> anyhow::Result<String> {
        let response = self
            .client()
            .new_addr(pb::NewaddrRequest::default())
            .await?
            .into_inner();
        response
            .bech32
            .ok_or_else(|| anyhow!("cln newaddr returned no bech32 address"))
    }

    pub async fn open_channel(
        &self,
        peer: &PublicKey,
        peer_host: &str,
        peer_port: u16,
    ) -> anyhow::Result<()> {
        with_backoff("cln open channel", Backoff::slow(), || async {
            let _ = self
                .client()
                .connect_peer(pb::ConnectRequest {
                    id: peer.to_string(),
                    host: Some(peer_host.to_string()),
                    port: Some(peer_port as u32),
                })
                .await;

            self.client()
                .fund_channel(pb::FundchannelRequest {
                    id: peer.serialize().to_vec(),
                    amount: Some(pb::AmountOrAll {
                        value: Some(pb::amount_or_all::Value::Amount(pb::Amount {
                            msat: CHANNEL_SIZE_SAT * 1000,
                        })),
                    }),
                    announce: Some(true),
                    ..Default::default()
                })
                .await
                .map_err(|e| anyhow!(e.to_string()))
        })
        .await?;
        Ok(())
    }

    pub async fn active_channel_count(&self) -> anyhow::Result<usize> {
        let response = self
            .client()
            .list_peer_channels(pb::ListpeerchannelsRequest::default())
            .await?
            .into_inner();
        let normal =
            pb::listpeerchannels_channels::ListpeerchannelsChannelsState::ChanneldNormal as i32;
        Ok(response
            .channels
            .iter()
            .filter(|c| c.state == normal)
            .count())
    }

    pub async fn graph_synced(&self, nodes: usize, channels: usize) -> anyhow::Result<()> {
        let node_count = self
            .client()
            .list_nodes(pb::ListnodesRequest::default())
            .await?
            .into_inner()
            .nodes
            .len();
        // CLN's gossip view lists one entry per direction.
        let direction_count = self
            .client()
            .list_channels(pb::ListchannelsRequest::default())
            .await?
            .into_inner()
            .channels
            .len();

        if node_count >= nodes && direction_count >= channels * 2 {
            Ok(())
        } else {
            Err(anyhow!(
                "graph has {node_count}/{nodes} nodes, {direction_count}/{} channel directions",
                channels * 2
            ))
        }
    }

    pub async fn settled_keysend_count(&self) -> anyhow::Result<u64> {
        let response = self
            .client()
            .list_invoices(pb::ListinvoicesRequest::default())
            .await?
            .into_inner();
        let paid = pb::listinvoices_invoices::ListinvoicesInvoicesStatus::Paid as i32;
        Ok(response
            .invoices
            .iter()
            .filter(|i| i.status == paid && i.label.starts_with("keysend-"))
            .count() as u64)
    }
}
