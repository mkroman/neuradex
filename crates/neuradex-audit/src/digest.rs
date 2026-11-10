//! Small digest helpers shared by the event schema and the header policy.

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Returns the lowercase hex SHA-256 digest of `input`.
pub fn sha256_hex(input: &[u8]) -> String {
    const_hex::encode(Sha256::digest(input))
}

/// Returns the lowercase hex HMAC-SHA256 of `message` under `key`.
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(message);

    const_hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_a_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_matches_rfc_4231_vectors() {
        assert_eq!(
            hmac_sha256_hex(b"key", b"The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn long_keys_are_hashed_to_the_block_size() {
        let key = [b'a'; 100];
        let digest = hmac_sha256_hex(&key, b"message");

        assert_eq!(digest.len(), 64);
        assert_ne!(digest, hmac_sha256_hex(&key[..64], b"message"));
    }
}
