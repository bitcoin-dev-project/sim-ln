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
