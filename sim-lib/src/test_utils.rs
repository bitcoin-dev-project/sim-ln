use rand::distributions::Uniform;
use rand::Rng;

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};

/// Utility function to create a vector of pseudo random bytes.
///
/// Mainly used for testing purposes.
pub fn get_random_bytes(size: usize) -> Vec<u8> {
    rand::thread_rng()
        .sample_iter(Uniform::new(u8::MIN, u8::MAX))
        .take(size)
        .collect()
}

/// Utility function to create a random integer in a given range
pub fn get_random_int(s: u64, e: u64) -> u64 {
    rand::thread_rng().gen_range(s..e)
}

/// Gets a key pair generated in a pseudorandom way.
pub fn get_random_keypair() -> (SecretKey, PublicKey) {
    loop {
        if let Ok(sk) = SecretKey::from_slice(&get_random_bytes(32)) {
            return (sk, PublicKey::from_secret_key(&Secp256k1::new(), &sk));
        }
    }
}
