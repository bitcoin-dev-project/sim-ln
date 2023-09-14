use expanduser::expanduser;
use serde::Deserialize;

pub mod serde_payment_hash {

    use lightning::ln::PaymentHash;
    use serde::Deserialize;

    pub fn serialize<S>(hash: &PaymentHash, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&hex::encode(hash.0))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PaymentHash, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        let slice: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(serde::de::Error::custom)?;

        Ok(PaymentHash(slice))
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
