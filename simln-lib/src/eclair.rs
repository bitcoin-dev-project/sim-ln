use crate::{
    serializers, Graph, LightningError, LightningNode, NodeId, NodeInfo, PaymentOutcome,
    PaymentResult,
};
use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use lightning::ln::features::NodeFeatures;
use lightning::ln::{PaymentHash, PaymentPreimage};
use reqwest::multipart::Form;
use reqwest::{Client, Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::error::Error;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time;
use triggered::Listener;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EclairConnection {
    #[serde(with = "serializers::serde_node_id")]
    pub id: NodeId,
    #[serde(with = "serializers::serde_address")]
    pub base_url: String,
    pub api_username: String,
    pub api_password: String,
}

struct EclairClient {
    base_url: Url,
    api_username: String,
    api_password: String,
    http_client: Client,
}

impl EclairClient {
    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        params: Option<HashMap<String, String>>,
    ) -> Result<T, Box<dyn Error>> {
        let url = self.base_url.join(endpoint)?;
        let mut request = self
            .http_client
            .request(Method::POST, url)
            .basic_auth(self.api_username.clone(), Some(self.api_password.clone()));

        if let Some(params) = params {
            let mut form = Form::new();
            for (key, value) in params {
                form = form.text(key, value);
            }
            request = request.multipart(form);
        }

        let response = request
            .send()
            .await?
            .error_for_status()?
            .json::<T>()
            .await?;

        Ok(response)
    }
}

impl TryInto<EclairClient> for EclairConnection {
    type Error = LightningError;

    fn try_into(self) -> Result<EclairClient, LightningError> {
        let base_url = Url::parse(&self.base_url)
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;
        let http_client = Client::new();
        Ok(EclairClient {
            base_url,
            api_username: self.api_username,
            api_password: self.api_password,
            http_client,
        })
    }
}

pub struct EclairNode {
    client: Mutex<EclairClient>,
    info: NodeInfo,
    network: Network,
}

impl EclairNode {
    pub async fn new(connection: EclairConnection) -> Result<Self, LightningError> {
        let client: EclairClient = connection.try_into()?;
        let info: GetInfoResponse = client
            .request("getinfo", None)
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;
        let pubkey = PublicKey::from_str(info.node_id.as_str())
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;
        let network = Network::from_str(match info.network.as_str() {
            "mainnet" => "bitcoin",
            "simnet" => {
                return Err(LightningError::ValidationError(
                    "simnet is not supported".to_string(),
                ))
            },
            x => x,
        })
        .map_err(|err| LightningError::ValidationError(err.to_string()))?;
        let features = parse_json_to_node_features(&info.features);

        Ok(Self {
            client: Mutex::new(client),
            info: NodeInfo {
                pubkey,
                alias: info.alias,
                features,
            },
            network,
        })
    }
}

#[async_trait]
impl LightningNode for EclairNode {
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
        let client = self.client.lock().await;
        let preimage = PaymentPreimage(rand::random()).0;
        let mut params = HashMap::new();
        params.insert("nodeId".to_string(), hex::encode(dest.serialize()));
        params.insert("amountMsat".to_string(), amount_msat.to_string());
        params.insert("paymentHash".to_string(), hex::encode(preimage));
        let uuid: String = client
            .request("sendtonode", Some(params))
            .await
            .map_err(|err| LightningError::SendPaymentError(err.to_string()))?;

        let mut params = HashMap::new();
        params.insert("paymentHash".to_string(), hex::encode(preimage));
        params.insert("id".to_string(), uuid);
        let payment_parts: PaymentInfoResponse = client
            .request("getsentinfo", Some(params))
            .await
            .map_err(|_| LightningError::InvalidPaymentHash)?;
        let payment_hash: [u8; 32] = hex::decode(&payment_parts[0].payment_hash)
            .map_err(|_| LightningError::InvalidPaymentHash)?
            .try_into()
            .map_err(|_| LightningError::InvalidPaymentHash)?;

        Ok(PaymentHash(payment_hash))
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
                    let client = self.client.lock().await;
                    let mut params = HashMap::new();
                    params.insert("paymentHash".to_string(), hex::encode(hash.0));

                    let payment_parts: PaymentInfoResponse = client
                    .request("getsentinfo", Some(params))
                    .await
                    .map_err(|err| LightningError::TrackPaymentError(err.to_string()))?;

                    if let Some(payment) = payment_parts.first() {
                        let htlc_count = payment_parts.len();
                        let payment_outcome = match payment.status.r#type {
                            PaymentStatus::Pending => continue,
                            PaymentStatus::Sent => PaymentOutcome::Success,
                            // PaymentStatus::Failed means API call failed (can happen for various reasons).
                            // Task to improve it: https://github.com/bitcoin-dev-project/sim-ln/issues/26#issuecomment-1691780018
                            PaymentStatus::Failed => PaymentOutcome::UnexpectedError,
                        };

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
        let mut params = HashMap::new();
        params.insert("nodeId".to_string(), hex::encode(node_id.serialize()));

        let client = self.client.lock().await;
        let node_info: NodeResponse = client
            .request("node", Some(params))
            .await
            .map_err(|err| LightningError::GetNodeInfoError(err.to_string()))?;
        let features = parse_json_to_node_features(&node_info.announcement.features);

        Ok(NodeInfo {
            pubkey: *node_id,
            alias: node_info.announcement.alias,
            features,
        })
    }

    async fn list_channels(&self) -> Result<Vec<u64>, LightningError> {
        let client = self.client.lock().await;
        let channels: ChannelsResponse = client
            .request("channels", None)
            .await
            .map_err(|err| LightningError::ListChannelsError(err.to_string()))?;

        let capacities_msat: Vec<u64> = channels
            .iter()
            .map(|channel| {
                channel
                    .data
                    .commitments
                    .active_channels
                    .iter()
                    .map(|ac| ac.local_commit.spec.to_local * 1_000)
                    .sum()
            })
            .collect();

        Ok(capacities_msat)
    }

    async fn get_graph(&self) -> Result<Graph, LightningError> {
        let client = self.client.lock().await;
        let nodes: NodesResponse = client
            .request("nodes", None)
            .await
            .map_err(|err| LightningError::GetNodeInfoError(err.to_string()))?;

        let mut nodes_by_pk: HashMap<PublicKey, NodeInfo> = HashMap::new();

        for node in nodes {
            nodes_by_pk.insert(
                PublicKey::from_str(&node.node_id).expect("Public Key not valid"),
                NodeInfo {
                    pubkey: PublicKey::from_str(&node.node_id).expect("Public Key not valid"),
                    alias: node.alias.clone(),
                    features: parse_json_to_node_features(&node.features),
                },
            );
        }

        Ok(Graph { nodes_by_pk })
    }
}

#[derive(Debug, Deserialize)]
struct GetInfoResponse {
    #[serde(rename = "nodeId")]
    node_id: String,
    alias: String,
    network: String,
    features: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PaymentStatus {
    Pending,
    Failed,
    Sent,
}

#[derive(Debug, Deserialize)]
struct PaymentStatusDetails {
    #[serde(rename = "type")]
    r#type: PaymentStatus,
}

#[derive(Debug, Deserialize)]
struct PaymentPart {
    status: PaymentStatusDetails,
    #[serde(rename = "paymentHash")]
    payment_hash: String,
}

type PaymentInfoResponse = Vec<PaymentPart>;

#[derive(Debug, Deserialize)]
struct Announcement {
    alias: String,
    features: Value,
}

#[derive(Debug, Deserialize)]
struct NodeResponse {
    announcement: Announcement,
}

#[derive(Debug, Deserialize)]
struct NodeInGraph {
    #[serde(rename = "nodeId")]
    node_id: String,
    alias: String,
    features: Value,
}

type ChannelsResponse = Vec<Channel>;
type NodesResponse = Vec<NodeInGraph>;

#[derive(Debug, Deserialize)]
struct Channel {
    #[serde(rename = "data")]
    data: ChannelData,
}

#[derive(Debug, Deserialize)]
struct ChannelData {
    #[serde(rename = "commitments")]
    commitments: Commitments,
}

#[derive(Debug, Deserialize)]
struct Commitments {
    #[serde(rename = "active")]
    active_channels: Vec<ActiveChannel>,
}

#[derive(Debug, Deserialize)]
struct ActiveChannel {
    #[serde(rename = "localCommit")]
    local_commit: LocalCommit,
}

#[derive(Debug, Deserialize)]
struct LocalCommit {
    #[serde(rename = "spec")]
    spec: LocalCommitSpec,
}

#[derive(Debug, Deserialize)]
struct LocalCommitSpec {
    #[serde(rename = "toLocal")]
    to_local: u64,
}

fn parse_json_to_node_features(json: &Value) -> NodeFeatures {
    let mut flags = vec![0u8; 8];

    let feature_mapping: HashMap<&str, usize> = [
        ("option_data_loss_protect", 0),
        ("option_upfront_shutdown_script", 4),
        ("gossip_queries", 6),
        ("var_onion_optin", 8),
        ("gossip_queries_ex", 10),
        ("option_static_remotekey", 12),
        ("payment_secret", 14),
        ("basic_mpp", 16),
        ("option_support_large_channel", 18),
        ("option_anchors_zero_fee_htlc_tx", 22),
        ("option_route_blinding", 24),
        ("option_shutdown_anysegwit", 26),
        ("option_dual_fund", 28),
        ("option_quiesce", 34),
        ("option_onion_messages", 38),
        ("option_provide_storage", 42),
        ("option_channel_type", 44),
        ("option_scid_alias", 46),
        ("option_payment_metadata", 48),
        ("option_zeroconf", 50),
        ("keysend", 54),
        ("option_simple_close", 60),
    ]
    .iter()
    .cloned()
    .collect();

    if let Some(activated) = json["activated"].as_object() {
        for (feature, level) in activated.iter() {
            if let Some(&bit_position) = feature_mapping.get(feature.as_str()) {
                // Determine if the feature is mandatory or optional.
                let is_mandatory = level == "mandatory";
                // Calculate the exact bit position.
                let bit = if is_mandatory {
                    bit_position // Even bit for mandatory
                } else {
                    bit_position + 1 // Odd bit for optional
                };
                // Calculate the byte index and bit offset.
                let byte_index = bit / 8;
                let bit_offset = bit % 8;

                // Set the corresponding bit.
                flags[byte_index] |= 1 << bit_offset;
            }
        }
    }

    NodeFeatures::from_le_bytes(flags)
}
