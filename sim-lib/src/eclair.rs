use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;

use lightning::ln::PaymentHash;

use serde::{Deserialize, Serialize};

use triggered::Listener;

use crate::{
	serializers, LightningError, LightningNode, PaymentResult, NodeId, NodeInfo,
};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EclairConnection {
	#[serde(with = "serializers::serde_node_id")]
	pub id: NodeId,
	pub address: String,
	#[serde(deserialize_with = "serializers::deserialize_path")]
	pub ca_cert: String,
	#[serde(deserialize_with = "serializers::deserialize_path")]
	pub client_cert: String,
	#[serde(deserialize_with = "serializers::deserialize_path")]
	pub client_key: String,
}

//TODO: Feels tmp to code real client library to connect with eclair.
struct Client {}

pub struct EclairNode {
    	client: Client,
    	info: NodeInfo,
}

impl EclairNode {
    	pub async fn new(connection: EclairConnection) -> Result<Self, LightningError> {
		Err(LightningError::ConnectionError("tmp err".to_string()))
	}

	async fn node_channels(&mut self, is_source: bool) -> Result<Vec<u64>, LightningError> {
		Err(LightningError::ConnectionError("tmp err".to_string()))
	}
}

#[async_trait]
impl LightningNode for EclairNode {
	fn get_info(&self) -> &NodeInfo {
		&self.info
	}

	async fn get_network(&mut self) -> Result<Network, LightningError> {
		Err(LightningError::ConnectionError("tmp err".to_string()))
	}

	async fn send_payment(
		&mut self,
		dest: PublicKey,
		amount_msat: u64,
	) -> Result<PaymentHash, LightningError> {
		Err(LightningError::ConnectionError("tmp err".to_string()))
	}

	async fn track_payment(
		&mut self,
		hash: &PaymentHash,
		shutdown: Listener,
	) -> Result<PaymentResult, LightningError> {
		Err(LightningError::ConnectionError("tmp err".to_string()))
	}

	async fn get_node_info(&mut self, node_id: &PublicKey) -> Result<NodeInfo, LightningError> {
		Err(LightningError::ConnectionError("tmp err".to_string()))
	}

	async fn list_channels(&mut self) -> Result<Vec<u64>, LightningError> {
		Err(LightningError::ConnectionError("tmp err".to_string()))
	}
}
