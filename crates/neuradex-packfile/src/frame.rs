//! Frame header and frame decoding.

use std::io::Read as _;

use crate::util::{read_array, read_u16, read_u32, read_u64};
use crate::{ContentHash, FORMAT_VERSION, PackError, frame_checksum, hash_body};

/// Magic at the start of every frame header.
pub const FRAME_MAGIC: [u8; 4] = *b"NDPF";

/// Size of a frame header in bytes.
pub const FRAME_HEADER_LEN: usize = 96;

/// Frame flag: the payload is a zstd frame.
pub const FRAME_FLAG_ZSTD: u16 = 0x0001;

const FRAME_FLAGS_SUPPORTED: u16 = FRAME_FLAG_ZSTD;
const RESERVED_OFFSET: usize = 20;

/// The fixed-size header preceding each zstd frame.
///
/// The encoded layout is:
///
/// | Offset | Size | Field |
/// | --- | --- | --- |
/// | 0 | 4 | magic (`NDPF`) |
/// | 4 | 2 | format version |
/// | 6 | 2 | flags |
/// | 8 | 8 | plaintext length |
/// | 16 | 4 | compressed length |
/// | 20 | 4 | reserved (zero) |
/// | 24 | 32 | BLAKE3-256 plaintext hash |
/// | 56 | 32 | SHA-256 plaintext hash |
/// | 88 | 8 | XXH3-64 compressed checksum |
///
/// All integers are little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Frame flags. Currently only [`FRAME_FLAG_ZSTD`] is defined.
    pub flags: u16,
    /// Length of the decoded plaintext in bytes.
    pub plaintext_len: u64,
    /// Length of the zstd frame that follows the header.
    pub compressed_len: u32,
    /// BLAKE3-256 hash of the decoded plaintext.
    pub blake3: [u8; 32],
    /// SHA-256 hash of the decoded plaintext.
    pub sha256: [u8; 32],
    /// XXH3-64 checksum over the compressed frame bytes.
    pub checksum: u64,
}

impl FrameHeader {
    /// Creates a zstd frame header.
    #[must_use]
    pub const fn new(
        plaintext_len: u64,
        compressed_len: u32,
        blake3: [u8; 32],
        sha256: [u8; 32],
        checksum: u64,
    ) -> Self {
        Self {
            flags: FRAME_FLAG_ZSTD,
            plaintext_len,
            compressed_len,
            blake3,
            sha256,
            checksum,
        }
    }

    /// Returns the content hash recorded in this header.
    #[must_use]
    pub const fn content_hash(&self) -> ContentHash {
        ContentHash::new(self.blake3, self.sha256)
    }

    /// Encodes the header into its 96-byte wire representation.
    #[must_use]
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut output = [0_u8; FRAME_HEADER_LEN];
        output[0..4].copy_from_slice(&FRAME_MAGIC);
        output[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        output[6..8].copy_from_slice(&self.flags.to_le_bytes());
        output[8..16].copy_from_slice(&self.plaintext_len.to_le_bytes());
        output[16..20].copy_from_slice(&self.compressed_len.to_le_bytes());
        output[24..56].copy_from_slice(&self.blake3);
        output[56..88].copy_from_slice(&self.sha256);
        output[88..96].copy_from_slice(&self.checksum.to_le_bytes());
        output
    }

    /// Decodes a frame header from the start of `bytes`.
    ///
    /// Additional bytes after the 96-byte header are ignored, which lets
    /// callers pass a header and payload slice directly.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::Truncated`] when fewer than [`FRAME_HEADER_LEN`]
    /// bytes are available, and a format error when the magic, version, flags,
    /// or reserved bytes are invalid.
    pub fn decode(bytes: &[u8]) -> Result<Self, PackError> {
        if bytes.len() < FRAME_HEADER_LEN {
            return Err(PackError::Truncated {
                needed: FRAME_HEADER_LEN,
                available: bytes.len(),
            });
        }

        if read_array::<4>(bytes, 0)? != FRAME_MAGIC {
            return Err(PackError::InvalidFrameMagic);
        }
        let version = read_u16(bytes, 4)?;
        if version != FORMAT_VERSION {
            return Err(PackError::UnsupportedVersion {
                found: version,
                expected: FORMAT_VERSION,
            });
        }

        let flags = read_u16(bytes, 6)?;
        if flags & !FRAME_FLAGS_SUPPORTED != 0 {
            return Err(PackError::UnsupportedFlags(flags));
        }
        if flags & FRAME_FLAG_ZSTD == 0 {
            return Err(PackError::FrameNotCompressed);
        }
        if read_u32(bytes, RESERVED_OFFSET)? != 0 {
            return Err(PackError::ReservedNotZero);
        }

        Ok(Self {
            flags,
            plaintext_len: read_u64(bytes, 8)?,
            compressed_len: read_u32(bytes, 16)?,
            blake3: read_array::<32>(bytes, 24)?,
            sha256: read_array::<32>(bytes, 56)?,
            checksum: read_u64(bytes, 88)?,
        })
    }
}

/// Decodes and verifies one zstd frame.
///
/// This checks the compressed length, the XXH3-64 checksum, the decoded
/// plaintext length, and both content hashes before returning the plaintext.
/// Decompression is capped at the length recorded in the header, so a
/// compression bomb cannot expand without bound.
///
/// # Errors
///
/// Returns an error when the header flags do not describe a zstd frame, when
/// lengths or the checksum do not match, when the payload is not valid zstd,
/// or when either content hash does not match the decoded plaintext.
pub fn decode_frame(header: &FrameHeader, compressed: &[u8]) -> Result<Vec<u8>, PackError> {
    if header.flags & !FRAME_FLAGS_SUPPORTED != 0 {
        return Err(PackError::UnsupportedFlags(header.flags));
    }
    if header.flags & FRAME_FLAG_ZSTD == 0 {
        return Err(PackError::FrameNotCompressed);
    }

    let compressed_len =
        usize::try_from(header.compressed_len).map_err(|_| PackError::ContentTooLarge)?;
    if compressed.len() != compressed_len {
        return Err(PackError::LengthMismatch {
            expected: compressed_len,
            found: compressed.len(),
        });
    }
    if frame_checksum(compressed) != header.checksum {
        return Err(PackError::FrameChecksumMismatch);
    }

    let expected_len =
        usize::try_from(header.plaintext_len).map_err(|_| PackError::ContentTooLarge)?;
    let limit = expected_len.checked_add(1).ok_or(PackError::Overflow)?;
    let decoder = zstd::stream::Decoder::new(compressed).map_err(PackError::InvalidZstd)?;
    let mut plaintext = Vec::with_capacity(expected_len.min(1024 * 1024));
    decoder
        .take(u64::try_from(limit).map_err(|_| PackError::Overflow)?)
        .read_to_end(&mut plaintext)
        .map_err(PackError::InvalidZstd)?;

    if plaintext.len() != expected_len {
        return Err(PackError::LengthMismatch {
            expected: expected_len,
            found: plaintext.len(),
        });
    }

    let actual = hash_body(&plaintext);
    if actual.blake3 != header.blake3 {
        return Err(PackError::Blake3Mismatch);
    }
    if actual.sha256 != header.sha256 {
        return Err(PackError::Sha256Mismatch);
    }

    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::{FRAME_HEADER_LEN, FRAME_MAGIC, FrameHeader, decode_frame};
    use crate::{PackError, frame_checksum, hash_body};

    fn sample() -> FrameHeader {
        let content = hash_body(b"frame payload");
        FrameHeader::new(
            u64::try_from(b"frame payload".len()).unwrap(),
            1234,
            content.blake3,
            content.sha256,
            0x1234_5678_9abc_def0,
        )
    }

    #[test]
    fn header_round_trip() {
        let header = sample();
        let bytes = header.encode();
        assert_eq!(bytes.len(), FRAME_HEADER_LEN);
        assert_eq!(&bytes[..4], &FRAME_MAGIC);

        let decoded = FrameHeader::decode(&bytes).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decoded.content_hash().blake3, header.blake3);
        assert_eq!(decoded.content_hash().sha256, header.sha256);
    }

    #[test]
    fn header_decode_ignores_trailing_payload() {
        let header = sample();
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(b"payload");
        assert_eq!(FrameHeader::decode(&bytes).unwrap(), header);
    }

    #[test]
    fn header_rejects_truncation_and_corruption() {
        let header = sample();
        let encoded = header.encode();

        assert!(matches!(
            FrameHeader::decode(&encoded[..FRAME_HEADER_LEN - 1]),
            Err(PackError::Truncated {
                needed: FRAME_HEADER_LEN,
                ..
            })
        ));

        let mut bad_magic = encoded;
        bad_magic[0] ^= 0xff;
        assert!(matches!(
            FrameHeader::decode(&bad_magic),
            Err(PackError::InvalidFrameMagic)
        ));

        let mut bad_version = encoded;
        bad_version[4] = 2;
        assert!(matches!(
            FrameHeader::decode(&bad_version),
            Err(PackError::UnsupportedVersion {
                found: 2,
                expected: 1
            })
        ));

        let mut bad_flags = encoded;
        bad_flags[6] = 0x80;
        assert!(matches!(
            FrameHeader::decode(&bad_flags),
            Err(PackError::UnsupportedFlags(_))
        ));

        let mut missing_zstd = encoded;
        missing_zstd[6] = 0;
        missing_zstd[7] = 0;
        assert!(matches!(
            FrameHeader::decode(&missing_zstd),
            Err(PackError::FrameNotCompressed)
        ));

        let mut bad_reserved = encoded;
        bad_reserved[20] = 1;
        assert!(matches!(
            FrameHeader::decode(&bad_reserved),
            Err(PackError::ReservedNotZero)
        ));
    }

    #[test]
    fn frame_decode_round_trip() {
        let plaintext = b"hello frame".to_vec();
        let compressed = zstd::stream::encode_all(plaintext.as_slice(), 3).unwrap();
        let content = hash_body(&plaintext);
        let header = FrameHeader::new(
            u64::try_from(plaintext.len()).unwrap(),
            u32::try_from(compressed.len()).unwrap(),
            content.blake3,
            content.sha256,
            frame_checksum(&compressed),
        );

        assert_eq!(decode_frame(&header, &compressed).unwrap(), plaintext);
    }

    #[test]
    fn frame_decode_rejects_bad_compressed_length() {
        let plaintext = b"hello frame";
        let compressed = zstd::stream::encode_all(plaintext.as_slice(), 3).unwrap();
        let content = hash_body(plaintext);
        let mut header = FrameHeader::new(
            u64::try_from(plaintext.len()).unwrap(),
            u32::try_from(compressed.len()).unwrap(),
            content.blake3,
            content.sha256,
            frame_checksum(&compressed),
        );
        header.compressed_len += 1;

        assert!(matches!(
            decode_frame(&header, &compressed),
            Err(PackError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn frame_decode_rejects_bad_checksum() {
        let plaintext = b"hello frame";
        let compressed = zstd::stream::encode_all(plaintext.as_slice(), 3).unwrap();
        let content = hash_body(plaintext);
        let header = FrameHeader::new(
            u64::try_from(plaintext.len()).unwrap(),
            u32::try_from(compressed.len()).unwrap(),
            content.blake3,
            content.sha256,
            frame_checksum(&compressed) ^ 1,
        );

        assert!(matches!(
            decode_frame(&header, &compressed),
            Err(PackError::FrameChecksumMismatch)
        ));
    }

    #[test]
    fn frame_decode_rejects_invalid_zstd() {
        let compressed = b"this is not a zstd frame";
        let content = hash_body(b"expected");
        let header = FrameHeader::new(
            u64::try_from(b"expected".len()).unwrap(),
            u32::try_from(compressed.len()).unwrap(),
            content.blake3,
            content.sha256,
            frame_checksum(compressed),
        );

        assert!(matches!(
            decode_frame(&header, compressed),
            Err(PackError::InvalidZstd(_))
        ));
    }

    #[test]
    fn frame_decode_rejects_hash_mismatch() {
        let plaintext = b"hello frame";
        let compressed = zstd::stream::encode_all(plaintext.as_slice(), 3).unwrap();
        let content = hash_body(b"different");
        let header = FrameHeader::new(
            u64::try_from(plaintext.len()).unwrap(),
            u32::try_from(compressed.len()).unwrap(),
            content.blake3,
            content.sha256,
            frame_checksum(&compressed),
        );

        assert!(matches!(
            decode_frame(&header, &compressed),
            Err(PackError::Blake3Mismatch)
        ));
    }

    #[test]
    fn frame_decode_rejects_oversized_plaintext() {
        let plaintext = b"hello frame";
        let compressed = zstd::stream::encode_all(plaintext.as_slice(), 3).unwrap();
        let content = hash_body(plaintext);
        let header = FrameHeader::new(
            u64::try_from(plaintext.len()).unwrap() + 1,
            u32::try_from(compressed.len()).unwrap(),
            content.blake3,
            content.sha256,
            frame_checksum(&compressed),
        );

        assert!(matches!(
            decode_frame(&header, &compressed),
            Err(PackError::LengthMismatch { .. })
        ));
    }
}
