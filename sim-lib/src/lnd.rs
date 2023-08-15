use crate::{LightningNode, PaymentError};
use bitcoin::secp256k1::PublicKey;
use lightning::ln::PaymentHash;
use tonic_lnd::Client;

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

impl LightningNode for LndNode {
    fn send_payment(&self, _dest: PublicKey, _amt_msat: u64) -> anyhow::Result<PaymentHash> {
        unimplemented!()
    }

    fn track_payment(&self, _hash: PaymentHash) -> Result<(), PaymentError> {
        unimplemented!()
    }
}
