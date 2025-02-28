use std::str::FromStr;

use bitcoin::{secp256k1::PublicKey, Network};
use lightning::ln::features::NodeFeatures;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tonic_lnd::Client;

use crate::{
    serializers, LightningError, LightningNode, NodeId, NodeInfo, PaymentOutcome, PaymentResult,
};

#[derive(Debug, Deserialize)]
pub struct GetNodeInfoRequest {}

#[derive(Debug, Deserialize)]
pub struct GetNodeInfoResponse {
    /// The hex-encoded `node-id` or public key for our own lightning node.
    pub node_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LdkConnection {
    #[serde(with = "serializers::serde_node_id")]
    pub id: NodeId,
    #[serde(with = "serializers::serde_address")]
    pub address: String,
}

pub struct LdkNode {
    pub client: Client,
    base_url: String,
    info: Option<NodeInfo>,
}

impl LdkNode {
    pub async fn new(&self, connection: LdkConnection) -> Result<Self, LightningError> {
        let client = Client::new();
        let ldk_node = Self {
            base_url: connection.address,
            client,
            info: None,
        };

        let GetNodeInfoResponse { node_id } = self.get_node_info(GetNodeInfoRequest {}).await?;
        let node_info = NodeInfo {
            pubkey: PublicKey::from_str(&node_id),
            alias: String::from(""),
            features: NodeFeatures::empty(),
        };

        Ok(ldk_node)
    }

    async fn post_request<Rq: Message, Rs: Message + Default>(
        &self,
        request: &Rq,
        url: &str,
    ) -> Result<Rs, LightningError> {
        let request_body = request.encode_to_vec();
        let response_raw = match self
            .client
            .post(url)
            .header(CONTENT_TYPE, APPLICATION_OCTET_STREAM)
            .body(request_body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                return Err(LightningError::GetInfoError(e.to_string()));
            },
        };
        let status = response_raw.status();
        let payload = response_raw.bytes().await?;

        if status.is_success() {
            Ok(Rs::decode(&payload[..])?)
        } else {
            Err(LightningError::GetInfoError("Unknown Error".to_string()))
        }
    }

    pub async fn get_node_info(
        &self,
        request: GetNodeInfoRequest,
    ) -> Result<GetNodeInfoResponse, LightningError> {
        let url = format!("http://{}/{GET_NODE_INFO_PATH}", self.base_url);
        self.post_request(&request, &url).await
    }
}

impl LightningNode for LdkNode {
    fn get_info(&self) -> &NodeInfo {
        &self.info
    }

    async fn get_network(&mut self) -> Result<Network, LightningError> {
        Ok(Network::from("regtest"))
    }
}
