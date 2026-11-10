//! Fixed-size pack trailer.

use crate::util::{read_array, read_u16, read_u32, read_u64};
use crate::{FORMAT_VERSION, PackError};

/// Magic at the start of the trailer.
pub const TRAILER_MAGIC: [u8; 8] = *b"NDPKTRLR";

/// Size of the trailer in bytes.
pub const TRAILER_LEN: usize = 128;

const TRAILER_FLAGS: u16 = 0;

/// The last 128 bytes of a pack.
///
/// | Offset | Size | Field |
/// | --- | --- | --- |
/// | 0 | 8 | magic (`NDPKTRLR`) |
/// | 8 | 2 | format version |
/// | 10 | 2 | flags (zero) |
/// | 12 | 4 | reserved (zero) |
/// | 16 | 8 | footer offset |
/// | 24 | 8 | footer length |
/// | 32 | 32 | pack BLAKE3-256 hash |
/// | 64 | 32 | pack SHA-256 hash |
/// | 96 | 8 | footer checksum copy |
/// | 104 | 24 | reserved (zero) |
///
/// The pack hashes cover every byte before the trailer: all frames and the
/// footer. The trailer itself is not hashed. There is no keyed MAC in this
/// version; the reserved bytes are intended for a future keyed-BLAKE3
/// extension. All integers are little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trailer {
    /// Offset of the footer from the start of the pack.
    pub footer_offset: u64,
    /// Length of the encoded footer.
    pub footer_len: u64,
    /// BLAKE3-256 hash of all bytes before the trailer.
    pub pack_blake3: [u8; 32],
    /// SHA-256 hash of all bytes before the trailer.
    pub pack_sha256: [u8; 32],
    /// Copy of the footer's own XXH3-64 checksum.
    pub footer_checksum: u64,
}

impl Trailer {
    /// Creates a trailer.
    #[must_use]
    pub const fn new(
        footer_offset: u64,
        footer_len: u64,
        pack_blake3: [u8; 32],
        pack_sha256: [u8; 32],
        footer_checksum: u64,
    ) -> Self {
        Self {
            footer_offset,
            footer_len,
            pack_blake3,
            pack_sha256,
            footer_checksum,
        }
    }

    /// Encodes the trailer into its 128-byte wire representation.
    #[must_use]
    pub fn encode(&self) -> [u8; TRAILER_LEN] {
        let mut output = [0_u8; TRAILER_LEN];
        output[0..8].copy_from_slice(&TRAILER_MAGIC);
        output[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        output[10..12].copy_from_slice(&TRAILER_FLAGS.to_le_bytes());
        output[16..24].copy_from_slice(&self.footer_offset.to_le_bytes());
        output[24..32].copy_from_slice(&self.footer_len.to_le_bytes());
        output[32..64].copy_from_slice(&self.pack_blake3);
        output[64..96].copy_from_slice(&self.pack_sha256);
        output[96..104].copy_from_slice(&self.footer_checksum.to_le_bytes());
        output
    }

    /// Decodes a trailer from exactly 128 bytes.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::LengthMismatch`] when the input is not exactly
    /// [`TRAILER_LEN`] bytes, and format errors for bad magic, version, flags,
    /// or reserved bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, PackError> {
        if bytes.len() != TRAILER_LEN {
            return Err(PackError::LengthMismatch {
                expected: TRAILER_LEN,
                found: bytes.len(),
            });
        }

        if read_array::<8>(bytes, 0)? != TRAILER_MAGIC {
            return Err(PackError::InvalidTrailerMagic);
        }
        let version = read_u16(bytes, 8)?;
        if version != FORMAT_VERSION {
            return Err(PackError::UnsupportedVersion {
                found: version,
                expected: FORMAT_VERSION,
            });
        }
        let flags = read_u16(bytes, 10)?;
        if flags != TRAILER_FLAGS {
            return Err(PackError::UnsupportedFlags(flags));
        }
        if read_u32(bytes, 12)? != 0
            || read_u64(bytes, 104)? != 0
            || read_u64(bytes, 112)? != 0
            || read_u64(bytes, 120)? != 0
        {
            return Err(PackError::ReservedNotZero);
        }

        Ok(Self {
            footer_offset: read_u64(bytes, 16)?,
            footer_len: read_u64(bytes, 24)?,
            pack_blake3: read_array::<32>(bytes, 32)?,
            pack_sha256: read_array::<32>(bytes, 64)?,
            footer_checksum: read_u64(bytes, 96)?,
        })
    }

    /// Decodes the trailer at the end of a complete pack.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::Truncated`] when the input is shorter than
    /// [`TRAILER_LEN`], or any error from [`Trailer::decode`].
    pub fn from_pack(bytes: &[u8]) -> Result<Self, PackError> {
        let start = bytes
            .len()
            .checked_sub(TRAILER_LEN)
            .ok_or(PackError::Truncated {
                needed: TRAILER_LEN,
                available: bytes.len(),
            })?;
        let trailer = bytes.get(start..).ok_or(PackError::Truncated {
            needed: TRAILER_LEN,
            available: bytes.len(),
        })?;
        Self::decode(trailer)
    }
}

#[cfg(test)]
mod tests {
    use super::{TRAILER_LEN, TRAILER_MAGIC, Trailer};
    use crate::PackError;

    fn sample() -> Trailer {
        Trailer::new(10, 20, [1; 32], [2; 32], 0x1122_3344_5566_7788)
    }

    #[test]
    fn trailer_round_trip() {
        let trailer = sample();
        let encoded = trailer.encode();
        assert_eq!(encoded.len(), TRAILER_LEN);
        assert_eq!(&encoded[..8], &TRAILER_MAGIC);
        assert_eq!(Trailer::decode(&encoded).unwrap(), trailer);
        assert_eq!(Trailer::from_pack(&encoded).unwrap(), trailer);
    }

    #[test]
    fn trailer_rejects_bad_input() {
        let encoded = sample().encode();
        assert!(matches!(
            Trailer::decode(&encoded[..TRAILER_LEN - 1]),
            Err(PackError::LengthMismatch { .. })
        ));
        assert!(matches!(
            Trailer::from_pack(&encoded[..TRAILER_LEN - 1]),
            Err(PackError::Truncated { .. })
        ));

        let mut bad_magic = encoded;
        bad_magic[0] ^= 0xff;
        assert!(matches!(
            Trailer::decode(&bad_magic),
            Err(PackError::InvalidTrailerMagic)
        ));

        let mut bad_version = encoded;
        bad_version[8] = 2;
        assert!(matches!(
            Trailer::decode(&bad_version),
            Err(PackError::UnsupportedVersion {
                found: 2,
                expected: 1
            })
        ));

        let mut bad_flags = encoded;
        bad_flags[10] = 1;
        assert!(matches!(
            Trailer::decode(&bad_flags),
            Err(PackError::UnsupportedFlags(1))
        ));

        let mut bad_reserved = encoded;
        bad_reserved[12] = 1;
        assert!(matches!(
            Trailer::decode(&bad_reserved),
            Err(PackError::ReservedNotZero)
        ));
    }

    #[test]
    fn from_pack_uses_last_128_bytes() {
        let trailer = sample();
        let mut bytes = vec![0xaa; 17];
        bytes.extend_from_slice(&trailer.encode());
        assert_eq!(Trailer::from_pack(&bytes).unwrap(), trailer);
    }
}
