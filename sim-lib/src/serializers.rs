use expanduser::expanduser;
use serde::Deserialize;

pub mod serde_option_payment_hash {
    use lightning::ln::PaymentHash;

    pub fn serialize<S>(hash: &Option<PaymentHash>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match hash {
            Some(hash) => serializer.serialize_str(&hex::encode(hash.0)),
            None => serializer.serialize_str("Unknown"),
        }
    }
}

pub mod serde_node_id {
    use super::*;
    use std::str::FromStr;

    use crate::NodeId;
    use bitcoin::secp256k1::PublicKey;

    pub fn serialize<S>(id: &NodeId, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&match id {
            NodeId::PublicKey(p) => p.to_string(),
            NodeId::Alias(s) => s.to_string(),
        })
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<NodeId, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if let Ok(pk) = PublicKey::from_str(&s) {
            Ok(NodeId::PublicKey(pk))
        } else {
            Ok(NodeId::Alias(s))
        }
    }
}

pub fn deserialize_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(expanduser(s)
        .map_err(serde::de::Error::custom)?
        .display()
        .to_string())
}
