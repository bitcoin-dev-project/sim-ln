use std::collections::HashMap;

use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use cln_grpc::pb::{
    listpays_pays::ListpaysPaysStatus, node_client::NodeClient, Amount, GetinfoRequest,
    KeysendRequest, KeysendResponse, ListchannelsRequest, ListnodesRequest, ListpaysRequest,
    ListpaysResponse,
};
use lightning::ln::features::NodeFeatures;
use lightning::ln::PaymentHash;
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, Error};
use tokio::sync::Mutex;
use tokio::time::{self, Duration};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use triggered::Listener;

use crate::{
    serializers, Graph, LightningError, LightningNode, NodeId, NodeInfo, PaymentOutcome,
    PaymentResult,
};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClnConnection {
    #[serde(with = "serializers::serde_node_id")]
    pub id: NodeId,
    #[serde(with = "serializers::serde_address")]
    pub address: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub ca_cert: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub client_cert: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub client_key: String,
}

pub struct ClnNode {
    pub client: Mutex<NodeClient<Channel>>,
    info: NodeInfo,
    network: Network,
}

impl ClnNode {
    pub async fn new(connection: ClnConnection) -> Result<Self, LightningError> {
        let tls = ClientTlsConfig::new()
            .domain_name("cln")
            .identity(Identity::from_pem(
                reader(&connection.client_cert).await.map_err(|err| {
                    LightningError::ConnectionError(format!(
                        "Cannot load client certificate: {}",
                        err
                    ))
                })?,
                reader(&connection.client_key).await.map_err(|err| {
                    LightningError::ConnectionError(format!("Cannot load client key: {}", err))
                })?,
            ))
            .ca_certificate(Certificate::from_pem(
                reader(&connection.ca_cert).await.map_err(|err| {
                    LightningError::ConnectionError(format!("Cannot load CA certificate: {}", err))
                })?,
            ));

        let client = Mutex::new(NodeClient::new(
            Channel::from_shared(connection.address)
                .map_err(|err| LightningError::ConnectionError(err.to_string()))?
                .tls_config(tls)
                .map_err(|err| {
                    LightningError::ConnectionError(format!(
                        "Cannot establish tls connection: {}",
                        err
                    ))
                })?
                .connect()
                .await
                .map_err(|err| {
                    LightningError::ConnectionError(format!(
                        "Cannot connect to gRPC server: {}",
                        err
                    ))
                })?,
        ));

        let info = client
            .lock()
            .await
            .getinfo(GetinfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

        let pubkey = PublicKey::from_slice(&info.id)
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;
        let mut alias = info.alias.unwrap_or_default();
        connection.id.validate(&pubkey, &mut alias)?;

        let features = match info.our_features {
            Some(features) => NodeFeatures::from_be_bytes(features.node),
            None => NodeFeatures::empty(),
        };

        let network = Network::from_core_arg(&info.network)
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;

        Ok(Self {
            client,
            info: NodeInfo {
                pubkey,
                features,
                alias,
            },
            network,
        })
    }

    /// Fetch channels belonging to the local node, initiated locally if is_source is true, and initiated remotely if
    /// is_source is false. Introduced as a helper function because CLN doesn't have a single API to list all of our
    /// node's channels.
    async fn node_channels(&self, is_source: bool) -> Result<Vec<u64>, LightningError> {
        let req = if is_source {
            ListchannelsRequest {
                source: Some(self.info.pubkey.serialize().to_vec()),
                ..Default::default()
            }
        } else {
            ListchannelsRequest {
                destination: Some(self.info.pubkey.serialize().to_vec()),
                ..Default::default()
            }
        };

        let resp = self
            .client
            .lock()
            .await
            .list_channels(req)
            .await
            .map_err(|err| LightningError::ListChannelsError(err.to_string()))?
            .into_inner();

        Ok(resp
            .channels
            .into_iter()
            .map(|channel| channel.amount_msat.unwrap_or_default().msat)
            .collect())
    }
}

#[async_trait]
impl LightningNode for ClnNode {
    fn get_info(&self) -> &NodeInfo {
        &self.info
    }

    fn get_network(&self) -> Network {
        self.network
    }

    async fn send_payment(
        &self,
        dest: PublicKey,
        amount_msat: u64,
    ) -> Result<PaymentHash, LightningError> {
        let mut client = self.client.lock().await;
        let KeysendResponse { payment_hash, .. } = client
            .key_send(KeysendRequest {
                destination: dest.serialize().to_vec(),
                amount_msat: Some(Amount { msat: amount_msat }),
                ..Default::default()
            })
            .await
            .map_err(|s| {
                let message = s.message();
                // REF: https://docs.corelightning.org/reference/lightning-keysend#return-value

                if message.contains("Some(-1") | message.contains("Some(203") {
                    // Error codes -1 and 203 indicate permanent errors
                    LightningError::PermanentError(format!("{:?}", message))
                } else {
                    // Error codes, 205, 206 and 210 indicate temporary errors that can be retried
                    LightningError::SendPaymentError(format!("{:?}", message))
                }
            })?
            .into_inner();
        let slice: [u8; 32] = payment_hash
            .as_slice()
            .try_into()
            .map_err(|_| LightningError::InvalidPaymentHash)?;

        Ok(PaymentHash(slice))
    }

    async fn track_payment(
        &self,
        hash: &PaymentHash,
        shutdown: Listener,
    ) -> Result<PaymentResult, LightningError> {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.clone() => {
                    return Err(LightningError::TrackPaymentError("Shutdown before tracking results".to_string()));
                },
                _ = time::sleep(Duration::from_millis(500)) => {
                    let mut client = self.client.lock().await;
                    let ListpaysResponse { pays } = client
                        .list_pays(ListpaysRequest {
                            payment_hash: Some(hash.0.to_vec()),
                            ..Default::default()
                        })
                        .await
                        .map_err(|err| LightningError::TrackPaymentError(err.to_string()))?
                        .into_inner();

                    if let Some(pay) = pays.first() {
                        let payment_status = ListpaysPaysStatus::from_i32(pay.status)
                            .ok_or(LightningError::TrackPaymentError("Invalid payment status".to_string()))?;

                        let payment_outcome = match payment_status {
                            ListpaysPaysStatus::Pending => continue,
                            ListpaysPaysStatus::Complete => PaymentOutcome::Success,
                            // Task: https://github.com/bitcoin-dev-project/sim-ln/issues/26#issuecomment-1691780018
                            ListpaysPaysStatus::Failed => PaymentOutcome::UnexpectedError,
                        };
                        let htlc_count = pay.number_of_parts.unwrap_or(1).try_into().map_err(|_| LightningError::TrackPaymentError("Invalid number of parts".to_string()))?;
                        return Ok(PaymentResult {
                            htlc_count,
                            payment_outcome,
                        });
                    }
                },
            }
        }
    }

    async fn get_node_info(&self, node_id: &PublicKey) -> Result<NodeInfo, LightningError> {
        let mut client = self.client.lock().await;
        let mut nodes: Vec<cln_grpc::pb::ListnodesNodes> = client
            .list_nodes(ListnodesRequest {
                id: Some(node_id.serialize().to_vec()),
            })
            .await
            .map_err(|err| LightningError::GetNodeInfoError(err.to_string()))?
            .into_inner()
            .nodes;

        // We are filtering `list_nodes` to a single node, so we should get either an empty vector or one with a single element
        if let Some(node) = nodes.pop() {
            Ok(NodeInfo {
                pubkey: *node_id,
                alias: node.alias.unwrap_or(String::new()),
                features: node
                    .features
                    .clone()
                    .map_or(NodeFeatures::empty(), NodeFeatures::from_be_bytes),
            })
        } else {
            Err(LightningError::GetNodeInfoError(
                "Node not found".to_string(),
            ))
        }
    }

    async fn list_channels(&self) -> Result<Vec<u64>, LightningError> {
        let mut node_channels = self.node_channels(true).await?;
        node_channels.extend(self.node_channels(false).await?);
        Ok(node_channels)
    }

    async fn get_graph(&self) -> Result<Graph, LightningError> {
        let mut client = self.client.lock().await;
        let nodes: Vec<cln_grpc::pb::ListnodesNodes> = client
            .list_nodes(ListnodesRequest { id: None })
            .await
            .map_err(|err| LightningError::GetNodeInfoError(err.to_string()))?
            .into_inner()
            .nodes;

        let mut nodes_by_pk: HashMap<PublicKey, NodeInfo> = HashMap::new();

        for node in nodes {
            nodes_by_pk.insert(
                PublicKey::from_slice(&node.nodeid).expect("Public Key not valid"),
                NodeInfo {
                    pubkey: PublicKey::from_slice(&node.nodeid).expect("Public Key not valid"),
                    alias: node.clone().alias.unwrap_or(String::new()),
                    features: node
                        .features
                        .clone()
                        .map_or(NodeFeatures::empty(), NodeFeatures::from_be_bytes),
                },
            );
        }

        Ok(Graph { nodes_by_pk })
    }
}

async fn reader(filename: &str) -> Result<Vec<u8>, Error> {
    let mut file = File::open(filename).await?;
    let mut contents = vec![];
    file.read_to_end(&mut contents).await?;
    Ok(contents)
}
