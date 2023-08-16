use std::{collections::HashMap, str::FromStr};

use crate::{LightningError, LightningNode, NodeInfo, PaymentResult};
use async_trait::async_trait;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::PublicKey;
use lightning::events::PaymentFailureReason;
use lightning::ln::{PaymentHash, PaymentPreimage};
use tonic_lnd::routerrpc::TrackPaymentRequest;
use tonic_lnd::{
    lnrpc::{GetInfoRequest, GetInfoResponse},
    routerrpc::SendPaymentRequest,
    Client,
};
use triggered::Listener;

const KEYSEND_KEY: u64 = 5482373484;
const SEND_PAYMENT_TIMEOUT_SECS: i32 = 300;

#[allow(dead_code)]
pub struct LndNode {
    client: Client,
    info: NodeInfo,
}

impl LndNode {
    pub async fn new(
        address: String,
        macaroon: String,
        cert: String,
    ) -> Result<Self, LightningError> {
        let mut client = tonic_lnd::connect(address, cert, macaroon)
            .await
            .map_err(|err| LightningError::ConnectionError(err.to_string()))?;

        let GetInfoResponse {
            identity_pubkey,
            features,
            alias,
            ..
        } = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

        Ok(Self {
            client,
            info: NodeInfo {
                pubkey: PublicKey::from_str(&identity_pubkey)
                    .map_err(|err| LightningError::GetInfoError(err.to_string()))?,
                features: features.keys().copied().collect(),
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
                ..Default::default()
            })
            .await?;

        let mut stream = response.into_inner();

        let payment_hash = match stream.message().await? {
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
            stream = stream.message() => {
                let payment = stream.map_err(|err| LightningError::TrackPaymentError(err.to_string()))?;
                match payment {
                    Some(payment) => {
                        return Ok(PaymentResult {
                            settled: payment.status == 2 || payment.status == 3,
                            htlc_count: payment.htlcs.len(),
                            failure_reason: match payment.failure_reason {
                            1 => Some(PaymentFailureReason::PaymentExpired),
                            2 => Some(PaymentFailureReason::RouteNotFound),
                            3 => Some(PaymentFailureReason::UnexpectedError),
                            4 => Some(PaymentFailureReason::UnexpectedError), // TODO: Capture failure reasons for incorrect payment
                            5 => Some(PaymentFailureReason::UnexpectedError), // TODO: Capture failure reasons for insufficient balance
                            _ => None,
                        },
                        });
                    },
                    None => {
                        return Err(LightningError::TrackPaymentError(
                            "No payment".to_string(),
                        ));
                    },
                }
            },
            _ = shutdown => { Err(LightningError::TrackPaymentError("Shutdown before tracking results".to_string())) }
        }
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
