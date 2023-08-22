use std::collections::HashSet;

use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use cln_grpc::pb::{
    node_client::NodeClient, Amount, GetinfoRequest, GetinfoResponse, KeysendRequest,
    KeysendResponse,
};
use lightning::ln::PaymentHash;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, Error};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use triggered::Listener;

use crate::{LightningError, LightningNode, NodeInfo, PaymentResult};

pub struct ClnNode {
    pub client: NodeClient<Channel>,
    info: NodeInfo,
}

impl ClnNode {
    pub async fn new(
        address: &str,
        ca_pem_path: &str,
        client_pem_path: &str,
        client_key_path: &str,
    ) -> Result<Self, LightningError> {
        let ca_pem = reader(ca_pem_path).await?;
        let client_pem = reader(client_pem_path).await?;
        let client_key = reader(client_key_path).await?;

        let ca = Certificate::from_pem(ca_pem);
        let ident = Identity::from_pem(client_pem, client_key);

        let tls = ClientTlsConfig::new()
            .domain_name("cln")
            .identity(ident)
            .ca_certificate(ca);

        let channel = Channel::from_shared(address.to_string())
            .map_err(|err| LightningError::ConnectionError(err.to_string()))?
            .tls_config(tls)?
            .connect()
            .await?;
        let mut client = NodeClient::new(channel);

        let GetinfoResponse { id, alias, .. } = client
            .getinfo(GetinfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

        Ok(Self {
            client,
            info: NodeInfo {
                pubkey: PublicKey::from_slice(&id)
                    .map_err(|err| LightningError::GetInfoError(err.to_string()))?,
                features: vec![],
                alias,
            },
        })
    }
}

#[async_trait]
impl LightningNode for ClnNode {
    fn get_info(&self) -> &NodeInfo {
        &self.info
    }

    async fn send_payment(
        &mut self,
        dest: PublicKey,
        amount_msat: u64,
    ) -> Result<PaymentHash, LightningError> {
        let KeysendResponse { payment_hash, .. } = self
            .client
            .key_send(KeysendRequest {
                destination: dest.serialize().to_vec(),
                amount_msat: Some(Amount { msat: amount_msat }),
                ..Default::default()
            })
            .await
            .map_err(|err| LightningError::SendPaymentError(err.to_string()))?
            .into_inner();
        let slice: [u8; 32] = payment_hash
            .as_slice()
            .try_into()
            .map_err(|_| LightningError::InvalidPaymentHash)?;

        Ok(PaymentHash(slice))
    }

    async fn track_payment(
        &mut self,
        _hash: PaymentHash,
        _shutdown: Listener,
    ) -> Result<PaymentResult, LightningError> {
        // equivalent rpc for tracking payment?
        Err(LightningError::TrackPaymentError(
            "Not implemented".to_string(),
        ))
    }

    async fn get_node_announcement(
        &mut self,
        _node: PublicKey,
    ) -> Result<HashSet<u32>, LightningError> {
        // equivalent rpc for getting node announcement?
        Ok(HashSet::new())
    }
}

async fn reader(filename: &str) -> Result<Vec<u8>, Error> {
    let mut file = File::open(filename).await?;
    let mut contents = vec![];
    file.read_to_end(&mut contents).await?;
    Ok(contents)
}
