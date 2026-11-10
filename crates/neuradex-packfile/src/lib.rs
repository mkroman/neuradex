//! Packfile format, reader, and writer for neuradex content-addressed storage.
//!
//! This crate owns the byte-level format only. It has no async runtime, object
//! storage, or service dependencies.
//!
//! # Layout
//!
//! A pack is a sequence of self-describing frames, followed by a footer and a
//! fixed-size trailer:
//!
//! ```text
//! +--------+--------+-----+-----------+-----------+
//! | frame  | frame  | ... |  footer   |  trailer  |
//! +--------+--------+-----+-----------+-----------+
//! ^ offset 0                            ^ pack_len - 128
//! ```
//!
//! Every field is little-endian and fixed-width. Types are encoded and decoded
//! field by field; no `repr` layout is exposed.
//!
//! - A **frame** stores the plaintext BLAKE3-256 and SHA-256 hashes, the
//!   plaintext and compressed lengths, an XXH3-64 checksum over the compressed
//!   bytes, and a zstd frame.
//! - The **footer** is a sorted index of frames keyed by BLAKE3-256, with an
//!   optional SHA-256 audit table in the same order. It also carries a checksum
//!   over its own bytes.
//! - The **trailer** stores the footer location, the pack-level BLAKE3/SHA-256
//!   hashes, and a copy of the footer checksum.
//!
//! # Footer rebuildability
//!
//! Frames are the source of truth: each frame header is self-describing, and
//! frames are laid out contiguously from offset zero. The footer can therefore
//! be rebuilt by scanning frame headers and recording their offsets. The
//! footer is only a lookup index; it is never required to understand a frame.
//!
//! # Integrity
//!
//! The BLAKE3 hash identifies the decoded content. The SHA-256 hash is an
//! independent audit hash, also stored over the decoded plaintext. The XXH3-64
//! checksum detects corruption in the compressed bytes before decompression.
//! The trailer's pack hashes cover every byte before the trailer (frames and
//! footer).
//!
//! There is no keyed MAC in this version. A keyed BLAKE3 MAC over the trailer
//! is a possible future extension; it is omitted because this crate has no key
//! management policy. The reserved trailer bytes leave room for such an
//! extension.

#![forbid(unsafe_code)]

mod error;
mod footer;
mod frame;
mod reader;
mod trailer;
mod util;
mod writer;

use std::fmt;
use std::str::FromStr;

pub use error::PackError;
pub use footer::{
    AUDIT_ENTRY_LEN, FOOTER_ENTRY_LEN, FOOTER_FLAG_AUDIT, FOOTER_HEADER_LEN, FOOTER_MAGIC, Footer,
    FooterEntry,
};
pub use frame::{FRAME_FLAG_ZSTD, FRAME_HEADER_LEN, FRAME_MAGIC, FrameHeader, decode_frame};
pub use reader::{FrameRef, PackReader};
pub use trailer::{TRAILER_LEN, TRAILER_MAGIC, Trailer};
pub use writer::{DEFAULT_COMPRESSION_LEVEL, PackSummary, PackWriter};

/// The packfile format version written and understood by this crate.
pub const FORMAT_VERSION: u16 = 1;

/// A content identifier: the BLAKE3-256 content hash plus a SHA-256 audit hash.
///
/// Both hashes are computed over the decoded plaintext. The BLAKE3 hash is the
/// primary identifier used for footer lookup; the SHA-256 hash is retained for
/// independent auditing and integrity checking.
///
/// [`Display`](std::fmt::Display) renders both hashes as
/// `blake3:<hex> sha256:<hex>`, and [`FromStr`] accepts those two tokens in
/// either order, separated by whitespace or commas. Hexadecimal digits are
/// case-insensitive; the prefixes are lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash {
    /// BLAKE3-256 hash of the decoded plaintext.
    pub blake3: [u8; 32],
    /// SHA-256 hash of the decoded plaintext.
    pub sha256: [u8; 32],
}

impl ContentHash {
    /// Creates a content hash from its two digests.
    #[must_use]
    pub const fn new(blake3: [u8; 32], sha256: [u8; 32]) -> Self {
        Self { blake3, sha256 }
    }

    /// Returns the BLAKE3 hash as lowercase hexadecimal.
    #[must_use]
    pub fn blake3_hex(&self) -> String {
        hex_encode(&self.blake3)
    }

    /// Returns the SHA-256 hash as lowercase hexadecimal.
    #[must_use]
    pub fn sha256_hex(&self) -> String {
        hex_encode(&self.sha256)
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("blake3:")?;
        write_hex(formatter, &self.blake3)?;
        formatter.write_str(" sha256:")?;
        write_hex(formatter, &self.sha256)
    }
}

impl FromStr for ContentHash {
    type Err = HashParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut blake3 = None;
        let mut sha256 = None;

        for token in
            value.split(|character: char| character.is_ascii_whitespace() || character == ',')
        {
            if token.is_empty() {
                continue;
            }
            if let Some(hex) = token.strip_prefix("blake3:") {
                if blake3.replace(parse_hex_32(hex, "blake3")?).is_some() {
                    return Err(HashParseError::Duplicate("blake3"));
                }
            } else if let Some(hex) = token.strip_prefix("sha256:") {
                if sha256.replace(parse_hex_32(hex, "sha256")?).is_some() {
                    return Err(HashParseError::Duplicate("sha256"));
                }
            } else {
                return Err(HashParseError::InvalidToken(token.to_owned()));
            }
        }

        let blake3 = blake3.ok_or(HashParseError::Missing("blake3"))?;
        let sha256 = sha256.ok_or(HashParseError::Missing("sha256"))?;
        Ok(Self { blake3, sha256 })
    }
}

/// An error returned while parsing a [`ContentHash`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HashParseError {
    /// A token was neither a `blake3:` nor a `sha256:` hash.
    #[error("invalid hash token {0:?}")]
    InvalidToken(String),
    /// One of the two required hashes was missing.
    #[error("missing {0} hash")]
    Missing(&'static str),
    /// A hash was supplied more than once.
    #[error("duplicate {0} hash")]
    Duplicate(&'static str),
    /// The hexadecimal string did not contain exactly 64 digits.
    #[error("{name} hash must contain 64 hex digits, found {found}")]
    InvalidLength {
        /// Hash name (`blake3` or `sha256`).
        name: &'static str,
        /// Number of hexadecimal digits found.
        found: usize,
    },
    /// The hexadecimal string contained a non-hexadecimal character.
    #[error("{name} hash contains a non-hexadecimal character")]
    InvalidHex {
        /// Hash name (`blake3` or `sha256`).
        name: &'static str,
    },
}

/// Computes both content hashes over a plaintext body.
#[must_use]
pub fn hash_body(body: &[u8]) -> ContentHash {
    use sha2::Digest as _;

    let blake3 = *blake3::hash(body).as_bytes();
    let sha256: [u8; 32] = sha2::Sha256::digest(body).into();
    ContentHash { blake3, sha256 }
}

/// Computes the XXH3-64 checksum stored for a compressed frame.
#[must_use]
pub fn frame_checksum(compressed: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(compressed)
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn write_hex(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for &byte in bytes {
        let high = char::from(HEX_DIGITS[usize::from(byte >> 4)]);
        let low = char::from(HEX_DIGITS[usize::from(byte & 0x0f)]);
        write!(formatter, "{high}{low}")?;
    }
    Ok(())
}

fn parse_hex_32(hex: &str, name: &'static str) -> Result<[u8; 32], HashParseError> {
    if hex.len() != 64 {
        return Err(HashParseError::InvalidLength {
            name,
            found: hex.len(),
        });
    }

    let mut output = [0_u8; 32];
    let (pairs, _) = hex.as_bytes().as_chunks::<2>();
    for (slot, pair) in output.iter_mut().zip(pairs) {
        let [high, low] = *pair;
        let high = hex_digit(high).ok_or(HashParseError::InvalidHex { name })?;
        let low = hex_digit(low).ok_or(HashParseError::InvalidHex { name })?;
        *slot = (high << 4) | low;
    }
    Ok(output)
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{ContentHash, HashParseError, frame_checksum, hash_body};

    const EMPTY_BLAKE3: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC_BLAKE3: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn hashes_match_known_vectors() {
        let empty = hash_body(b"");
        assert_eq!(empty.blake3_hex(), EMPTY_BLAKE3);
        assert_eq!(empty.sha256_hex(), EMPTY_SHA256);

        let abc = hash_body(b"abc");
        assert_eq!(abc.blake3_hex(), ABC_BLAKE3);
        assert_eq!(abc.sha256_hex(), ABC_SHA256);
    }

    #[test]
    fn display_and_parse_round_trip() {
        let hash = hash_body(b"round trip");
        let rendered = hash.to_string();
        assert_eq!(
            rendered,
            format!("blake3:{} sha256:{}", hash.blake3_hex(), hash.sha256_hex())
        );

        let parsed: ContentHash = rendered.parse().unwrap();
        assert_eq!(parsed, hash);

        let reversed = format!("sha256:{} blake3:{}", hash.sha256_hex(), hash.blake3_hex());
        assert_eq!(reversed.parse::<ContentHash>().unwrap(), hash);

        let comma = format!("sha256:{},blake3:{}", hash.sha256_hex(), hash.blake3_hex());
        assert_eq!(comma.parse::<ContentHash>().unwrap(), hash);
    }

    #[test]
    fn parse_accepts_uppercase_hex_digits() {
        let empty = hash_body(b"");
        let rendered = format!(
            "blake3:{} sha256:{}",
            empty.blake3_hex().to_uppercase(),
            empty.sha256_hex().to_uppercase()
        );
        assert_eq!(rendered.parse::<ContentHash>().unwrap(), empty);
    }

    #[test]
    fn parse_rejects_malformed_input() {
        let hash = hash_body(b"malformed");

        assert!(matches!(
            "".parse::<ContentHash>(),
            Err(HashParseError::Missing("blake3"))
        ));
        assert!(matches!(
            format!("blake3:{}", hash.blake3_hex()).parse::<ContentHash>(),
            Err(HashParseError::Missing("sha256"))
        ));
        assert!(matches!(
            format!(
                "blake3:{} sha256:{} sha256:{}",
                hash.blake3_hex(),
                hash.sha256_hex(),
                hash.sha256_hex()
            )
            .parse::<ContentHash>(),
            Err(HashParseError::Duplicate("sha256"))
        ));
        assert!(matches!(
            "md5:00".parse::<ContentHash>(),
            Err(HashParseError::InvalidToken(token)) if token == "md5:00"
        ));
        assert!(matches!(
            format!(
                "blake3:{} sha256:{}",
                &hash.blake3_hex()[..62],
                hash.sha256_hex()
            )
            .parse::<ContentHash>(),
            Err(HashParseError::InvalidLength {
                name: "blake3",
                found: 62
            })
        ));
        assert!(matches!(
            format!("blake3:{} sha256:{}", "z".repeat(64), hash.sha256_hex())
                .parse::<ContentHash>(),
            Err(HashParseError::InvalidHex { name: "blake3" })
        ));
    }

    #[test]
    fn frame_checksum_matches_known_vector() {
        assert_eq!(frame_checksum(b""), 0x2d06_8005_38d3_94c2);
    }
}
