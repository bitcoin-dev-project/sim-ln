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

pub mod serde_address {
    use super::*;

    pub fn serialize<S>(address: &str, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(address)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s.starts_with("https://") || s.starts_with("http://") {
            Ok(s)
        } else {
            Ok(format!("https://{}", s))
        }
    }
}

pub mod serde_value_or_range {
    use super::*;
    use serde::{de::Error, ser::SerializeTuple};

    use crate::ValueOrRange;

    pub fn serialize<S, T>(x: &ValueOrRange<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
        T: std::fmt::Display,
        T: serde::Serialize,
    {
        match x {
            ValueOrRange::Value(p) => serializer.serialize_some(p),
            ValueOrRange::Range(x, y) => {
                let mut tup = serializer.serialize_tuple(2)?;
                tup.serialize_element(x)?;
                tup.serialize_element(y)?;
                tup.end()
            },
        }
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<ValueOrRange<T>, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: serde::Deserialize<'de> + std::cmp::PartialOrd + std::fmt::Display + Copy,
    {
        let a = ValueOrRange::deserialize(deserializer)?;
        if let ValueOrRange::Range(x, y) = a {
            if x >= y {
                return Err(D::Error::custom(format!(
                    "Cannot parse range. Ranges must be strictly increasing (i.e. [x, y] with x > y). Received [{}, {}]",
                    x, y
                )));
            }
        }

        Ok(a)
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
