use std::str::FromStr;

use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::error::{LdkServerError, LdkServerErrorCode};
use ldk_server_client::ldk_server_grpc::api::{
    GetNodeInfoRequest, GetPaymentDetailsRequest, GraphGetNodeRequest, GraphListNodesRequest,
    ListChannelsRequest, SpontaneousSendRequest,
};
use ldk_server_client::ldk_server_grpc::types::PaymentStatus;
use lightning::ln::features::NodeFeatures;
use lightning::ln::PaymentHash;
use serde::{Deserialize, Serialize};
use tokio::time::{self, Duration};
use triggered::Listener;

use std::collections::HashMap;

use crate::{
    serializers, Graph, LightningError, LightningNode, NodeInfo, PaymentOutcome, PaymentResult,
};

pub struct LdkServerNode {
    client: LdkServerClient,
    info: NodeInfo,
    network: Network,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LdkServerConnection {
    pub address: String,
    pub api_key: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub cert: String,
}

impl LdkServerNode {
    pub async fn new(connection: LdkServerConnection) -> Result<Self, LightningError> {
        let cert_pem = std::fs::read(&connection.cert).map_err(|err| {
            LightningError::ConnectionError(format!("Cannot load TLS cert: {err}"))
        })?;

        let base_url = connection
            .address
            .strip_prefix("https://")
            .or_else(|| connection.address.strip_prefix("http://"))
            .unwrap_or(&connection.address)
            .trim_end_matches('/')
            .to_string();

        let client = LdkServerClient::new(base_url, connection.api_key.clone(), &cert_pem)
            .map_err(LightningError::ConnectionError)?;

        let info = client
            .get_node_info(GetNodeInfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;

        let pubkey = PublicKey::from_str(&info.node_id)
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;
        let alias = info.node_alias.unwrap_or_default();
        let network = network_from_proto(info.network)?;

        let features = parse_node_features(info.features.keys().copied());

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
}

#[async_trait]
impl LightningNode for LdkServerNode {
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
        let response = self
            .client
            .spontaneous_send(SpontaneousSendRequest {
                amount_msat,
                node_id: dest.to_string(),
                route_parameters: None,
                custom_tlvs: Vec::new(),
            })
            .await
            .map_err(ldk_server_error_to_send_error)?;

        string_to_payment_hash(&response.payment_id)
    }

    async fn track_payment(
        &self,
        hash: &PaymentHash,
        shutdown: Listener,
    ) -> Result<PaymentResult, LightningError> {
        // ldk-node uses the payment hash as the payment_id for spontaneous
        // payments, which is what `send_payment` dispatches.
        let payment_id = hex::encode(hash.0);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.clone() => {
                    return Err(LightningError::TrackPaymentError(
                        "Shutdown before tracking results".to_string(),
                    ));
                },
                _ = time::sleep(Duration::from_millis(500)) => {
                    let payment = self
                        .client
                        .get_payment_details(GetPaymentDetailsRequest {
                            payment_id: payment_id.clone(),
                        })
                        .await
                        .map_err(|err| LightningError::TrackPaymentError(err.to_string()))?
                        .payment;

                    let Some(payment) = payment else { continue };

                    let status = PaymentStatus::from_i32(payment.status).ok_or_else(|| {
                        LightningError::TrackPaymentError(
                            "Invalid payment status".to_string(),
                        )
                    })?;

                    let payment_outcome = match status {
                        PaymentStatus::Pending => continue,
                        PaymentStatus::Succeeded => PaymentOutcome::Success,
                        // ldk-server's Payment doesn't expose a failure reason,
                        // so we can't distinguish RouteNotFound/Expired/etc.
                        PaymentStatus::Failed => PaymentOutcome::UnexpectedError,
                    };

                    // ldk-server doesn't expose per-HTLC details; report as 1.
                    return Ok(PaymentResult {
                        htlc_count: 1,
                        payment_outcome,
                    });
                },
            }
        }
    }

    async fn get_node_info(&self, node_id: &PublicKey) -> Result<NodeInfo, LightningError> {
        if node_id == &self.info.pubkey {
            return Ok(self.info.clone());
        }

        let response = self
            .client
            .graph_get_node(GraphGetNodeRequest {
                node_id: node_id.to_string(),
            })
            .await
            .map_err(|err| LightningError::GetNodeInfoError(err.to_string()))?;

        let node = response
            .node
            .ok_or_else(|| LightningError::GetNodeInfoError("Node not found".to_string()))?;

        let alias = node.announcement_info.map(|a| a.alias).unwrap_or_default();

        let mut features = NodeFeatures::empty();
        features.set_keysend_optional();

        Ok(NodeInfo {
            pubkey: *node_id,
            alias,
            features,
        })
    }

    async fn channel_capacities(&self) -> Result<u64, LightningError> {
        let response = self
            .client
            .list_channels(ListChannelsRequest {})
            .await
            .map_err(|err| LightningError::ListChannelsError(err.to_string()))?;

        // ldk-server reports capacities in msat already; sum both sides to get
        // total channel capacity (matching lnd/cln which report total capacity).
        Ok(response
            .channels
            .iter()
            .map(|channel| channel.outbound_capacity_msat + channel.inbound_capacity_msat)
            .sum())
    }

    async fn get_graph(&self) -> Result<Graph, LightningError> {
        let node_ids = self
            .client
            .graph_list_nodes(GraphListNodesRequest {})
            .await
            .map_err(|err| LightningError::GetGraphError(err.to_string()))?
            .node_ids;

        // ldk-server has no bulk "describe graph" RPC, so we pull each node's
        // announcement (for its alias) one by one. This is sequential and will
        // be slow on large public graphs — fine for regtest/signet simulations.
        let mut nodes_by_pk: HashMap<PublicKey, NodeInfo> = HashMap::new();
        for node_id in node_ids {
            let pubkey = PublicKey::from_str(&node_id)
                .map_err(|err| LightningError::GetGraphError(err.to_string()))?;

            let alias = self
                .client
                .graph_get_node(GraphGetNodeRequest {
                    node_id: node_id.clone(),
                })
                .await
                .map_err(|err| LightningError::GetGraphError(err.to_string()))?
                .node
                .and_then(|n| n.announcement_info)
                .map(|a| a.alias)
                .unwrap_or_default();

            let mut features = NodeFeatures::empty();
            features.set_keysend_optional();

            nodes_by_pk.insert(
                pubkey,
                NodeInfo {
                    pubkey,
                    alias,
                    features,
                },
            );
        }

        Ok(Graph { nodes_by_pk })
    }
}

/// Convert the `types.Network` proto enum (encoded as i32) returned by
/// ldk-server's GetNodeInfo into a `bitcoin::Network`.  
fn network_from_proto(value: i32) -> Result<Network, LightningError> {
    match value {
        0 => Ok(Network::Bitcoin),
        1 => Ok(Network::Testnet),
        2 => Err(LightningError::GetInfoError(
            "testnet4 network is not supported".to_string(),
        )),
        3 => Ok(Network::Signet),
        4 => Ok(Network::Regtest),
        other => Err(LightningError::GetInfoError(format!(
            "ldk-server returned unsupported network value: {other}"
        ))),
    }
}

/// Convert the BOLT feature bits advertised by ldk-server (keyed by feature bit number) into LDK's
/// NodeFeatures by setting the corresponding bit for each advertised feature.
fn parse_node_features(bits: impl IntoIterator<Item = u32>) -> NodeFeatures {
    let mut flags = Vec::new();

    for bit in bits {
        let byte_offset = (bit / 8) as usize;
        if flags.len() <= byte_offset {
            flags.resize(byte_offset + 1, 0u8);
        }
        flags[byte_offset] |= 1 << (bit % 8);
    }

    NodeFeatures::from_le_bytes(flags)
}

fn string_to_payment_hash(hash: &str) -> Result<PaymentHash, LightningError> {
    let bytes = hex::decode(hash).map_err(|_| LightningError::InvalidPaymentHash)?;
    let slice: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| LightningError::InvalidPaymentHash)?;
    Ok(PaymentHash(slice))
}

/// Map an ldk-server error returned from a send RPC to a sim-ln error. Errors
/// that are specific to the payment attempt (routing, balance, upstream
/// lightning failures) are returned as `SendPaymentError` so the simulation
/// keeps running; misconfiguration-style errors are surfaced as permanent.
fn ldk_server_error_to_send_error(err: LdkServerError) -> LightningError {
    match err.error_code {
        LdkServerErrorCode::LightningError
        | LdkServerErrorCode::InternalError
        | LdkServerErrorCode::InternalServerError => LightningError::SendPaymentError(err.message),
        LdkServerErrorCode::AuthError | LdkServerErrorCode::InvalidRequestError => {
            LightningError::PermanentError(err.message)
        },
    }
}
