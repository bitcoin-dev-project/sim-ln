use std::str::FromStr;

use crate::{LightningNode, NodeInfo, PaymentError};
use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use lightning::ln::PaymentHash;
use tonic_lnd::{
    lnrpc::{GetInfoRequest, GetInfoResponse},
    Client,
};

#[allow(dead_code)]
pub struct LndNode {
    client: Client,
}

impl LndNode {
    pub async fn new(address: String, macaroon: String, cert: String) -> anyhow::Result<Self> {
        let client = tonic_lnd::connect(address, cert, macaroon).await?;
        Ok(Self { client })
    }
}

#[async_trait]
impl LightningNode for LndNode {
    async fn get_info(&self) -> Result<NodeInfo, PaymentError> {
        let mut client = self.client.clone();
        let ln_client = client.lightning();

        let GetInfoResponse {
            identity_pubkey,
            features,
            alias,
            ..
        } = ln_client
            .get_info(GetInfoRequest {})
            .await
            .map_err(|err| PaymentError::GetInfoFailed(err.to_string()))?
            .into_inner();

        Ok(NodeInfo {
            pubkey: PublicKey::from_str(&identity_pubkey)
                .map_err(|err| PaymentError::GetInfoFailed(err.to_string()))?,
            features: features.keys().into_iter().copied().collect(),
            alias,
        })
    }

    async fn send_payment(
        &self,
        _dest: PublicKey,
        _amount_msat: u64,
    ) -> Result<PaymentHash, PaymentError> {
        unimplemented!()
    }

    async fn track_payment(&self, _hash: PaymentHash) -> Result<(), PaymentError> {
        unimplemented!()
    }
}
