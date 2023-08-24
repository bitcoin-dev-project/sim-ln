use async_trait::async_trait;
use bitcoin::secp256k1::PublicKey;
use cln_grpc::pb::{
    node_client::NodeClient, Amount, GetinfoRequest, GetinfoResponse, KeysendRequest,
    KeysendResponse, ListnodesRequest,
};
use lightning::ln::features::NodeFeatures;
use lightning::ln::PaymentHash;

use tokio::fs::File;
use tokio::io::{AsyncReadExt, Error};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use triggered::Listener;

use crate::{ClnConnection, LightningError, LightningNode, NodeInfo, PaymentResult};

pub struct ClnNode {
    pub client: NodeClient<Channel>,
    info: NodeInfo,
}

impl ClnNode {
    pub async fn new(connection: ClnConnection) -> Result<Self, LightningError> {
        let tls = ClientTlsConfig::new()
            .domain_name("cln")
            .identity(Identity::from_pem(
                reader(&connection.client_cert).await.map_err(|_| {
                    LightningError::ConnectionError("Cannot loads client certificate".to_string())
                })?,
                reader(&connection.client_key).await.map_err(|_| {
                    LightningError::ConnectionError("Cannot loads client key".to_string())
                })?,
            ))
            .ca_certificate(Certificate::from_pem(
                reader(&connection.ca_cert).await.map_err(|_| {
                    LightningError::ConnectionError("Cannot loads CA certificate".to_string())
                })?,
            ));

        let mut client = NodeClient::new(
            Channel::from_shared(connection.address.to_string())
                .map_err(|err| LightningError::ConnectionError(err.to_string()))?
                .tls_config(tls)
                .map_err(|_| {
                    LightningError::ConnectionError("Cannot establish tls connection".to_string())
                })?
                .connect()
                .await
                .map_err(|_| {
                    LightningError::ConnectionError("Cannot connect to gRPC server".to_string())
                })?,
        );

        let GetinfoResponse {
            id,
            alias,
            our_features,
            ..
        } = client
            .getinfo(GetinfoRequest {})
            .await
            .map_err(|err| LightningError::GetInfoError(err.to_string()))?
            .into_inner();

        //FIXME: our_features is returning None, but it should not :S
        let features = if let Some(features) = our_features {
            NodeFeatures::from_le_bytes(features.node)
        } else {
            NodeFeatures::empty()
        };

        Ok(Self {
            client,
            info: NodeInfo {
                pubkey: PublicKey::from_slice(&id)
                    .map_err(|err| LightningError::GetInfoError(err.to_string()))?,
                features,
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
        unimplemented!()
    }

    async fn get_node_features(&mut self, node: PublicKey) -> Result<NodeFeatures, LightningError> {
        let node_id = node.serialize().to_vec();
        let nodes: Vec<cln_grpc::pb::ListnodesNodes> = self
            .client
            .list_nodes(ListnodesRequest {
                id: Some(node_id.clone()),
            })
            .await
            .map_err(|err| LightningError::GetNodeInfoError(err.to_string()))?
            .into_inner()
            .nodes;

        // We are filtering `list_nodes` to a single node, so we should get either an empty vector or one with a single element
        if let Some(node) = nodes.first() {
            Ok(node
                .features
                .clone()
                .map_or(NodeFeatures::empty(), |mut f| {
                    // We need to reverse this given it has the CLN wire encoding which is BE
                    f.reverse();
                    NodeFeatures::from_le_bytes(f)
                }))
        } else {
            Err(LightningError::GetNodeInfoError(
                "Node not found".to_string(),
            ))
        }
    }
}

async fn reader(filename: &str) -> Result<Vec<u8>, Error> {
    let mut file = File::open(filename).await?;
    let mut contents = vec![];
    file.read_to_end(&mut contents).await?;
    Ok(contents)
}
