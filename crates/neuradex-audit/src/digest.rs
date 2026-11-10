//! Small digest helpers shared by the event schema and the header policy.
//!
//! `hmac` is not used here: the workspace pins `hmac` 0.12, which is built on digest 0.10,
//! while `sha2` 0.11 is built on digest 0.11. The two cannot be combined in one HMAC
//! implementation. HMAC-SHA256 is small enough to implement directly on top of `sha2`; the
//! construction follows RFC 2104.

use sha2::{Digest, Sha256};

/// Returns the lowercase hex SHA-256 digest of `input`.
pub fn sha256_hex(input: &[u8]) -> String {
    to_hex(&Sha256::digest(input))
}

/// Renders bytes as lowercase hex.
pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

/// Returns the lowercase hex HMAC-SHA256 of `message` under `key`.
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;

    let mut key_block = [0_u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36_u8; BLOCK_SIZE];
    let mut outer_pad = [0x5c_u8; BLOCK_SIZE];
    for (index, byte) in key_block.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);

    to_hex(&outer.finalize())
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
