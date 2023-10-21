use std::collections::HashSet;
use std::{collections::HashMap, str::FromStr};

use crate::{
    serializers, LightningError, LightningNode, NodeId, NodeInfo, PaymentOutcome, PaymentResult,
};
use async_trait::async_trait;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use fedimint_tonic_lnd::lnrpc::{payment::PaymentStatus, GetInfoRequest, GetInfoResponse};
use fedimint_tonic_lnd::lnrpc::{ListChannelsRequest, NodeInfoRequest, PaymentFailureReason};
use fedimint_tonic_lnd::routerrpc::TrackPaymentRequest;
use fedimint_tonic_lnd::tonic::Code::Unavailable;
use fedimint_tonic_lnd::tonic::Status;
use fedimint_tonic_lnd::{routerrpc::SendPaymentRequest, Client};
use lightning::ln::features::NodeFeatures;
use lightning::ln::{PaymentHash, PaymentPreimage};
use serde::{Deserialize, Serialize};
use triggered::Listener;

const KEYSEND_KEY: u64 = 5482373484;
const SEND_PAYMENT_TIMEOUT_SECS: i32 = 300;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LndConnection {
    #[serde(with = "serializers::serde_node_id")]
    pub id: NodeId,
    pub address: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub macaroon: String,
    #[serde(deserialize_with = "serializers::deserialize_path")]
    pub cert: String,
}

pub struct LndNode {
    client: Client,
    info: NodeInfo,
}

// TODO: We could even generalize this to parse any type of Features
/// Parses the node features from the format returned by LND gRPC to LDK NodeFeatures
fn parse_node_features(features: HashSet<u32>) -> NodeFeatures {
    let mut flags = vec![0; 256];

    for f in features.into_iter() {
        let byte_offset = (f / 8) as usize;
        let mask = 1 << (f - 8 * byte_offset as u32);
        if flags.len() <= byte_offset {
            flags.resize(byte_offset + 1, 0u8);
        }

        flags[byte_offset] |= mask
    }

    NodeFeatures::from_le_bytes(flags)
}

impl LndNode {
    pub async fn new(connection: LndConnection) -> Result<Self, LightningError> {
        let mut client =
            fedimint_tonic_lnd::connect(connection.address, connection.cert, connection.macaroon)
                .await
                .map_err(|err| LightningError::ConnectionError(err.to_string()))?;

        let GetInfoResponse {
            identity_pubkey,
            features,
            mut alias,
            ..
        } = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

        let pubkey = PublicKey::from_str(&identity_pubkey)
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?;
        connection.id.validate(&pubkey, &mut alias)?;

        Ok(Self {
            client,
            info: NodeInfo {
                pubkey,
                features: parse_node_features(features.keys().cloned().collect()),
                alias,
            },
        })
    }
}

#[async_trait]
impl LightningNode for LndNode {
    /// NOTE: This is cached now. We do call the node's RPC get_info method in the constructor and save the info there.
    /// Currently, that info cannot be outdated, given we only store node_id, alias and features, but it may not be the case
    /// if we end up storing some other info returned by the RPC call, such as the block height
    fn get_info(&self) -> &NodeInfo {
        &self.info
    }

    async fn get_network(&mut self) -> Result<Network, LightningError> {
        let info = self
            .client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

        if info.chains.is_empty() {
            return Err(LightningError::ValidationError(format!(
                "{} is not connected any chain",
                self.get_info()
            )));
        } else if info.chains.len() > 1 {
            return Err(LightningError::ValidationError(format!(
                "{} is connected to more than one chain: {:?}",
                self.get_info(),
                info.chains.iter().map(|c| c.chain.to_string())
            )));
        }

        Ok(Network::from_str(match info.chains[0].network.as_str() {
            "mainnet" => "bitcoin",
            "simnet" => {
                return Err(LightningError::ValidationError(
                    "simnet is not supported".to_string(),
                ))
            }
            x => x,
        })
        .map_err(|err| LightningError::ValidationError(err.to_string()))?)
    }

    async fn send_payment(
        &mut self,
        dest: PublicKey,
        amount_msat: u64,
    ) -> Result<PaymentHash, LightningError> {
        let amt_msat: i64 = amount_msat
            .try_into()
            .map_err(|_| LightningError::SendPaymentError("Invalid send amount".to_string()))?;

        let preimage = PaymentPreimage(rand::random());

        let mut dest_custom_records = HashMap::new();
        let payment_hash = sha256::Hash::hash(&preimage.0).to_byte_array().to_vec();
        dest_custom_records.insert(KEYSEND_KEY, preimage.0.to_vec());

        let response = self
            .client
            .router()
            .send_payment_v2(SendPaymentRequest {
                amt_msat,
                dest: dest.serialize().to_vec(),
                dest_custom_records,
                payment_hash,
                timeout_seconds: SEND_PAYMENT_TIMEOUT_SECS,
                fee_limit_msat: i64::max_value(),
                ..Default::default()
            })
            .await
            .map_err(status_to_lightning_error)?;

        let mut stream = response.into_inner();

        let payment_hash = match stream.message().await.map_err(status_to_lightning_error)? {
            Some(payment) => string_to_payment_hash(&payment.payment_hash)?,
            None => return Err(LightningError::SendPaymentError("No payment".to_string())),
        };

        Ok(payment_hash)
    }

    async fn track_payment(
        &mut self,
        hash: PaymentHash,
        shutdown: Listener,
    ) -> Result<PaymentResult, LightningError> {
        let response = self
            .client
            .router()
            .track_payment_v2(TrackPaymentRequest {
                payment_hash: hash.0.to_vec(),
                no_inflight_updates: true,
            })
            .await
            .map_err(|err| LightningError::TrackPaymentError(err.to_string()))?;

        let mut stream = response.into_inner();

        tokio::select! {
            biased;
            _ = shutdown => { Err(LightningError::TrackPaymentError("Shutdown before tracking results".to_string())) },
            stream = stream.message() => {
                let payment = stream.map_err(|err| LightningError::TrackPaymentError(err.to_string()))?;
                match payment {
                    Some(payment) => {
                        let payment_status: PaymentStatus =payment.status.try_into()
                            .map_err(|_| LightningError::TrackPaymentError("Invalid payment status".to_string()))?;
                        let failure_reason: PaymentFailureReason = payment.failure_reason.try_into()
                            .map_err(|_| LightningError::TrackPaymentError("Invalid failure reason".to_string()))?;

                        let payment_outcome = match payment_status {
                            PaymentStatus::Succeeded => PaymentOutcome::Success,
                            PaymentStatus::Failed => match failure_reason {
                                PaymentFailureReason::FailureReasonTimeout => PaymentOutcome::PaymentExpired,
                                PaymentFailureReason::FailureReasonNoRoute => PaymentOutcome::RouteNotFound,
                                PaymentFailureReason::FailureReasonError => PaymentOutcome::UnexpectedError,
                                PaymentFailureReason::FailureReasonIncorrectPaymentDetails => PaymentOutcome::IncorrectPaymentDetails,
                                PaymentFailureReason::FailureReasonInsufficientBalance => PaymentOutcome::InsufficientBalance,
                                // Payment status is Failed, but failure reason is None or unknown
                                _ => return Err(LightningError::TrackPaymentError("Unexpected failure reason".to_string())),
                            },
                            // PaymentStatus::InFlight or PaymentStatus::Unknown
                            _ => PaymentOutcome::Unknown,
                        };
                        return Ok(PaymentResult {
                            htlc_count: payment.htlcs.len(),
                            payment_outcome
                        });
                    },
                    None => {
                        return Err(LightningError::TrackPaymentError(
                            "No payment".to_string(),
                        ));
                    },
                }
            },
        }
    }

    async fn get_node_info(&mut self, node_id: &PublicKey) -> Result<NodeInfo, LightningError> {
        let node_info = self
            .client
            .lightning()
            .get_node_info(NodeInfoRequest {
                pub_key: node_id.to_string(),
                include_channels: false,
            })
            .await
            .map_err(|err| LightningError::GetNodeInfoError(err.to_string()))?
            .into_inner();

        if let Some(node_info) = node_info.node {
            Ok(NodeInfo {
                pubkey: *node_id,
                alias: node_info.alias,
                features: parse_node_features(node_info.features.keys().cloned().collect()),
            })
        } else {
            Err(LightningError::GetNodeInfoError(
                "Node not found".to_string(),
            ))
        }
    }

    async fn list_channels(&mut self) -> Result<Vec<u64>, LightningError> {
        let channels = self
            .client
            .lightning()
            .list_channels(ListChannelsRequest {
                ..Default::default()
            })
            .await
            .map_err(|err| LightningError::ListChannelsError(err.to_string()))?
            .into_inner();

        // Capacity is returned in satoshis, so we convert to msat.
        Ok(channels
            .channels
            .iter()
            .map(|channel| 1000 * channel.capacity as u64)
            .collect())
    }
}

fn string_to_payment_hash(hash: &str) -> Result<PaymentHash, LightningError> {
    let bytes = hex::decode(hash).map_err(|_| LightningError::InvalidPaymentHash)?;
    let slice: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| LightningError::InvalidPaymentHash)?;
    Ok(PaymentHash(slice))
}

fn status_to_lightning_error(s: Status) -> LightningError {
    let code = s.code();
    let message = s.message();
    match code {
        Unavailable => LightningError::SendPaymentError(format!("Node unavailable: {message}")),
        _ => LightningError::PermanentError(message.to_string()),
    }
}
