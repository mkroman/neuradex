//! Sorted footer index.

use crate::util::{read_array, read_u16, read_u32, read_u64};
use crate::{ContentHash, FORMAT_VERSION, PackError};

/// Magic at the start of every footer.
pub const FOOTER_MAGIC: [u8; 4] = *b"NDPX";

/// Size of the footer header in bytes.
pub const FOOTER_HEADER_LEN: usize = 32;

/// Size of one footer entry in bytes.
pub const FOOTER_ENTRY_LEN: usize = 64;

/// Size of one SHA-256 audit hash in bytes.
pub const AUDIT_ENTRY_LEN: usize = 32;

/// Footer flag: the optional SHA-256 audit table is present.
pub const FOOTER_FLAG_AUDIT: u16 = 0x0001;

const FOOTER_FLAGS_SUPPORTED: u16 = FOOTER_FLAG_AUDIT;
const CHECKSUM_OFFSET: usize = 16;

/// One sorted footer entry describing a frame.
///
/// The 64-byte encoding is:
///
/// | Offset | Size | Field |
/// | --- | --- | --- |
/// | 0 | 32 | BLAKE3-256 content hash |
/// | 32 | 8 | frame offset |
/// | 40 | 4 | compressed length |
/// | 44 | 4 | plaintext length |
/// | 48 | 8 | XXH3-64 compressed checksum |
/// | 56 | 2 | frame flags |
/// | 58 | 2 | reserved (zero) |
/// | 60 | 4 | reserved (zero) |
///
/// All integers are little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FooterEntry {
    /// BLAKE3-256 hash of the decoded plaintext; also the sort key.
    pub blake3: [u8; 32],
    /// Offset of the frame header from the start of the pack.
    pub frame_offset: u64,
    /// Length of the compressed zstd frame after the header.
    pub compressed_len: u32,
    /// Length of the decoded plaintext.
    pub plaintext_len: u32,
    /// XXH3-64 checksum over the compressed frame bytes.
    pub xxh3_64: u64,
    /// Frame flags, as stored in the frame header.
    pub flags: u16,
    /// Reserved for future use; must be zero.
    pub reserved: u16,
    /// Reserved for future use; must be zero.
    pub reserved2: [u8; 4],
}

impl FooterEntry {
    /// Creates an entry with the reserved fields zeroed.
    #[must_use]
    pub const fn new(
        blake3: [u8; 32],
        frame_offset: u64,
        compressed_len: u32,
        plaintext_len: u32,
        xxh3_64: u64,
        flags: u16,
    ) -> Self {
        Self {
            blake3,
            frame_offset,
            compressed_len,
            plaintext_len,
            xxh3_64,
            flags,
            reserved: 0,
            reserved2: [0; 4],
        }
    }

    /// Encodes the entry into its 64-byte wire representation.
    #[must_use]
    pub fn encode(&self) -> [u8; FOOTER_ENTRY_LEN] {
        let mut output = [0_u8; FOOTER_ENTRY_LEN];
        output[0..32].copy_from_slice(&self.blake3);
        output[32..40].copy_from_slice(&self.frame_offset.to_le_bytes());
        output[40..44].copy_from_slice(&self.compressed_len.to_le_bytes());
        output[44..48].copy_from_slice(&self.plaintext_len.to_le_bytes());
        output[48..56].copy_from_slice(&self.xxh3_64.to_le_bytes());
        output[56..58].copy_from_slice(&self.flags.to_le_bytes());
        output[58..60].copy_from_slice(&self.reserved.to_le_bytes());
        output[60..64].copy_from_slice(&self.reserved2);
        output
    }

    /// Decodes exactly one 64-byte footer entry.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::LengthMismatch`] when the input is not exactly
    /// [`FOOTER_ENTRY_LEN`] bytes, or [`PackError::ReservedNotZero`] when a
    /// reserved field is non-zero.
    pub fn decode(bytes: &[u8]) -> Result<Self, PackError> {
        if bytes.len() != FOOTER_ENTRY_LEN {
            return Err(PackError::LengthMismatch {
                expected: FOOTER_ENTRY_LEN,
                found: bytes.len(),
            });
        }

        let entry = Self {
            blake3: read_array::<32>(bytes, 0)?,
            frame_offset: read_u64(bytes, 32)?,
            compressed_len: read_u32(bytes, 40)?,
            plaintext_len: read_u32(bytes, 44)?,
            xxh3_64: read_u64(bytes, 48)?,
            flags: read_u16(bytes, 56)?,
            reserved: read_u16(bytes, 58)?,
            reserved2: read_array::<4>(bytes, 60)?,
        };
        entry.validate()?;
        Ok(entry)
    }

    fn validate(&self) -> Result<(), PackError> {
        if self.reserved != 0 || self.reserved2 != [0; 4] {
            return Err(PackError::ReservedNotZero);
        }
        Ok(())
    }
}

/// A sorted footer index keyed by BLAKE3-256 content hash.
///
/// Frames are the source of truth; a footer is an index that can be rebuilt by
/// scanning frame headers. The optional audit table stores one SHA-256 hash per
/// entry in the same order as [`Footer::entries`].
///
/// The footer encoding is a 32-byte header, then the fixed-size entries, then
/// the optional audit table:
///
/// | Offset | Size | Field |
/// | --- | --- | --- |
/// | 0 | 4 | magic (`NDPX`) |
/// | 4 | 2 | format version |
/// | 6 | 2 | flags |
/// | 8 | 4 | entry count |
/// | 12 | 4 | reserved (zero) |
/// | 16 | 8 | XXH3-64 footer checksum |
/// | 24 | 8 | reserved (zero) |
/// | 32 | 64 each | entries, sorted by BLAKE3 |
/// | ... | 32 each | optional SHA-256 audit table |
///
/// The checksum covers the complete footer encoding with the checksum field
/// itself treated as zero. All integers are little-endian.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    entries: Vec<FooterEntry>,
    audit: Option<Vec<[u8; 32]>>,
    checksum: u64,
}

impl Footer {
    /// Builds a footer from entries and an optional same-length audit table.
    ///
    /// Entries must be strictly increasing by BLAKE3 hash.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::EntriesNotSorted`] when entries are unsorted or
    /// contain duplicate BLAKE3 hashes, [`PackError::AuditLengthMismatch`] when
    /// the audit table length differs from the entry count,
    /// [`PackError::ReservedNotZero`] when a reserved entry field is non-zero,
    /// and [`PackError::TooManyEntries`] when there are more than `u32::MAX`
    /// entries.
    pub fn from_entries(
        entries: Vec<FooterEntry>,
        audit: Option<Vec<[u8; 32]>>,
    ) -> Result<Self, PackError> {
        validate_entries(&entries)?;
        if let Some(audit) = &audit
            && audit.len() != entries.len()
        {
            return Err(PackError::AuditLengthMismatch {
                entries: entries.len(),
                audit: audit.len(),
            });
        }
        u32::try_from(entries.len()).map_err(|_| PackError::TooManyEntries {
            count: entries.len(),
        })?;

        let mut footer = Self {
            entries,
            audit,
            checksum: 0,
        };
        let encoded = footer.layout(0)?;
        footer.checksum = xxhash_rust::xxh3::xxh3_64(&encoded);
        Ok(footer)
    }

    /// Returns the sorted entries.
    #[must_use]
    pub fn entries(&self) -> &[FooterEntry] {
        &self.entries
    }

    /// Returns the optional SHA-256 audit table, in entry order.
    #[must_use]
    pub fn audit(&self) -> Option<&[[u8; 32]]> {
        self.audit.as_deref()
    }

    /// Returns whether the optional audit table is present.
    #[must_use]
    pub const fn has_audit(&self) -> bool {
        self.audit.is_some()
    }

    /// Returns the number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the footer has no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the stored XXH3-64 footer checksum.
    #[must_use]
    pub const fn checksum(&self) -> u64 {
        self.checksum
    }

    /// Looks up an entry by content hash.
    ///
    /// When the audit table is present, the supplied SHA-256 hash must match
    /// the audit hash as well.
    #[must_use]
    pub fn lookup(&self, hash: &ContentHash) -> Option<&FooterEntry> {
        let index = self.index_of_blake3(&hash.blake3)?;
        if self
            .audit
            .as_ref()
            .is_some_and(|audit| audit.get(index) != Some(&hash.sha256))
        {
            return None;
        }
        self.entries.get(index)
    }

    /// Looks up an entry by BLAKE3 hash alone.
    #[must_use]
    pub fn lookup_blake3(&self, blake3: &[u8; 32]) -> Option<&FooterEntry> {
        self.entries.get(self.index_of_blake3(blake3)?)
    }

    fn index_of_blake3(&self, blake3: &[u8; 32]) -> Option<usize> {
        self.entries
            .binary_search_by(|entry| entry.blake3.cmp(blake3))
            .ok()
    }

    /// Encodes the footer, including its checksum.
    ///
    /// # Errors
    ///
    /// Returns an error only if the internal length calculations overflow.
    pub fn encode(&self) -> Result<Vec<u8>, PackError> {
        self.layout(self.checksum)
    }

    /// Decodes a complete footer encoding.
    ///
    /// The input must contain exactly the header, entries, and optional audit
    /// table described by the header.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::Truncated`] when the input is shorter than the
    /// header or the table lengths require, [`PackError::LengthMismatch`] for
    /// trailing bytes, and format errors for bad magic, version, flags,
    /// reserved bytes, unsorted entries, or a checksum mismatch.
    pub fn decode(bytes: &[u8]) -> Result<Self, PackError> {
        if bytes.len() < FOOTER_HEADER_LEN {
            return Err(PackError::Truncated {
                needed: FOOTER_HEADER_LEN,
                available: bytes.len(),
            });
        }

        if read_array::<4>(bytes, 0)? != FOOTER_MAGIC {
            return Err(PackError::InvalidFooterMagic);
        }
        let version = read_u16(bytes, 4)?;
        if version != FORMAT_VERSION {
            return Err(PackError::UnsupportedVersion {
                found: version,
                expected: FORMAT_VERSION,
            });
        }

        let flags = read_u16(bytes, 6)?;
        if flags & !FOOTER_FLAGS_SUPPORTED != 0 {
            return Err(PackError::UnsupportedFlags(flags));
        }
        if read_u32(bytes, 12)? != 0 || read_u32(bytes, 24)? != 0 || read_u32(bytes, 28)? != 0 {
            return Err(PackError::ReservedNotZero);
        }

        let stored_checksum = read_u64(bytes, CHECKSUM_OFFSET)?;
        let count = usize::try_from(read_u32(bytes, 8)?).map_err(|_| PackError::Overflow)?;
        let has_audit = flags & FOOTER_FLAG_AUDIT != 0;

        let entries_len = count
            .checked_mul(FOOTER_ENTRY_LEN)
            .ok_or(PackError::Overflow)?;
        let audit_len = if has_audit {
            count
                .checked_mul(AUDIT_ENTRY_LEN)
                .ok_or(PackError::Overflow)?
        } else {
            0
        };
        let expected = FOOTER_HEADER_LEN
            .checked_add(entries_len)
            .and_then(|length| length.checked_add(audit_len))
            .ok_or(PackError::Overflow)?;

        if bytes.len() < expected {
            return Err(PackError::Truncated {
                needed: expected,
                available: bytes.len(),
            });
        }
        if bytes.len() != expected {
            return Err(PackError::LengthMismatch {
                expected,
                found: bytes.len(),
            });
        }

        let entries_end = FOOTER_HEADER_LEN
            .checked_add(entries_len)
            .ok_or(PackError::Overflow)?;
        let entries_bytes =
            bytes
                .get(FOOTER_HEADER_LEN..entries_end)
                .ok_or(PackError::Truncated {
                    needed: entries_end,
                    available: bytes.len(),
                })?;
        let entries = entries_bytes
            .as_chunks::<FOOTER_ENTRY_LEN>()
            .0
            .iter()
            .map(|chunk| FooterEntry::decode(chunk))
            .collect::<Result<Vec<_>, _>>()?;
        validate_entries(&entries)?;

        let audit = if has_audit {
            let audit_bytes = bytes
                .get(entries_end..expected)
                .ok_or(PackError::Truncated {
                    needed: expected,
                    available: bytes.len(),
                })?;
            let mut hashes = Vec::with_capacity(count);
            for chunk in audit_bytes.as_chunks::<AUDIT_ENTRY_LEN>().0 {
                hashes.push(*chunk);
            }
            if hashes.len() != count {
                return Err(PackError::AuditLengthMismatch {
                    entries: count,
                    audit: hashes.len(),
                });
            }
            Some(hashes)
        } else {
            None
        };

        let computed = checksum(bytes)?;
        if computed != stored_checksum {
            return Err(PackError::FooterChecksumMismatch);
        }

        Ok(Self {
            entries,
            audit,
            checksum: stored_checksum,
        })
    }

    fn layout(&self, checksum: u64) -> Result<Vec<u8>, PackError> {
        let count = u32::try_from(self.entries.len()).map_err(|_| PackError::TooManyEntries {
            count: self.entries.len(),
        })?;
        let flags = if self.audit.is_some() {
            FOOTER_FLAG_AUDIT
        } else {
            0
        };

        let entries_len = self
            .entries
            .len()
            .checked_mul(FOOTER_ENTRY_LEN)
            .ok_or(PackError::Overflow)?;
        let audit_len = self.audit.as_ref().map_or(0, Vec::len);
        let audit_len = audit_len
            .checked_mul(AUDIT_ENTRY_LEN)
            .ok_or(PackError::Overflow)?;
        let total = FOOTER_HEADER_LEN
            .checked_add(entries_len)
            .and_then(|length| length.checked_add(audit_len))
            .ok_or(PackError::Overflow)?;

        let mut output = Vec::with_capacity(total);
        output.extend_from_slice(&FOOTER_MAGIC);
        output.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        output.extend_from_slice(&flags.to_le_bytes());
        output.extend_from_slice(&count.to_le_bytes());
        output.extend_from_slice(&0_u32.to_le_bytes());
        output.extend_from_slice(&checksum.to_le_bytes());
        output.extend_from_slice(&0_u32.to_le_bytes());
        output.extend_from_slice(&0_u32.to_le_bytes());

        for entry in &self.entries {
            output.extend_from_slice(&entry.encode());
        }
        if let Some(audit) = &self.audit {
            for hash in audit {
                output.extend_from_slice(hash);
            }
        }

        Ok(output)
    }
}

fn validate_entries(entries: &[FooterEntry]) -> Result<(), PackError> {
    for entry in entries {
        entry.validate()?;
    }
    for pair in entries.windows(2) {
        if pair[0].blake3 >= pair[1].blake3 {
            return Err(PackError::EntriesNotSorted);
        }
    }
    Ok(())
}

fn checksum(bytes: &[u8]) -> Result<u64, PackError> {
    if bytes.len() < FOOTER_HEADER_LEN {
        return Err(PackError::Truncated {
            needed: FOOTER_HEADER_LEN,
            available: bytes.len(),
        });
    }
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&bytes[..CHECKSUM_OFFSET]);
    hasher.update(&[0_u8; 8]);
    hasher.update(&bytes[CHECKSUM_OFFSET + 8..]);
    Ok(hasher.digest())
}

#[cfg(test)]
mod tests {
    use super::{
        AUDIT_ENTRY_LEN, FOOTER_ENTRY_LEN, FOOTER_FLAG_AUDIT, FOOTER_HEADER_LEN, FOOTER_MAGIC,
        Footer, FooterEntry,
    };
    use crate::{ContentHash, FRAME_FLAG_ZSTD, PackError, hash_body};

    fn entry(body: &[u8], offset: u64) -> FooterEntry {
        let content = hash_body(body);
        FooterEntry::new(
            content.blake3,
            offset,
            u32::try_from(body.len()).unwrap(),
            u32::try_from(body.len()).unwrap(),
            0x0123_4567_89ab_cdef,
            FRAME_FLAG_ZSTD,
        )
    }

    fn sample_footer() -> (Footer, Vec<ContentHash>) {
        let bodies: [&[u8]; 3] = [b"charlie", b"alpha", b"bravo"];
        let mut hashes: Vec<ContentHash> = bodies.iter().map(|body| hash_body(body)).collect();
        hashes.sort_by_key(|hash| hash.blake3);

        let entries: Vec<FooterEntry> = hashes
            .iter()
            .enumerate()
            .map(|(index, hash)| {
                FooterEntry::new(
                    hash.blake3,
                    u64::try_from(index * 100).unwrap(),
                    12,
                    34,
                    u64::try_from(index).unwrap(),
                    FRAME_FLAG_ZSTD,
                )
            })
            .collect();
        let audit = Some(hashes.iter().map(|hash| hash.sha256).collect());
        (Footer::from_entries(entries, audit).unwrap(), hashes)
    }

    #[test]
    fn entry_round_trip() {
        let entry = entry(b"entry", 42);
        let encoded = entry.encode();
        assert_eq!(encoded.len(), FOOTER_ENTRY_LEN);
        assert_eq!(FooterEntry::decode(&encoded).unwrap(), entry);
    }

    #[test]
    fn entry_rejects_bad_length_and_reserved() {
        let entry = entry(b"entry", 42);
        let encoded = entry.encode();
        assert!(matches!(
            FooterEntry::decode(&encoded[..FOOTER_ENTRY_LEN - 1]),
            Err(PackError::LengthMismatch { .. })
        ));

        let mut reserved = encoded;
        reserved[58] = 1;
        assert!(matches!(
            FooterEntry::decode(&reserved),
            Err(PackError::ReservedNotZero)
        ));

        let mut reserved2 = encoded;
        reserved2[63] = 1;
        assert!(matches!(
            FooterEntry::decode(&reserved2),
            Err(PackError::ReservedNotZero)
        ));
    }

    #[test]
    fn footer_round_trip_and_lookup() {
        let (footer, hashes) = sample_footer();
        let encoded = footer.encode().unwrap();
        assert_eq!(&encoded[..4], &FOOTER_MAGIC);
        assert_eq!(encoded.len() % 8, 0);

        let decoded = Footer::decode(&encoded).unwrap();
        assert_eq!(decoded, footer);
        assert_eq!(decoded.len(), 3);
        assert!(decoded.has_audit());
        assert_eq!(decoded.audit().unwrap().len(), 3);
        assert!(
            decoded
                .entries()
                .windows(2)
                .all(|pair| pair[0].blake3 < pair[1].blake3)
        );

        for hash in &hashes {
            assert_eq!(decoded.lookup(hash).unwrap().blake3, hash.blake3);
            assert_eq!(
                decoded.lookup_blake3(&hash.blake3).unwrap().blake3,
                hash.blake3
            );
        }

        let mut wrong_sha = hashes[0];
        wrong_sha.sha256[0] ^= 0xff;
        assert!(decoded.lookup(&wrong_sha).is_none());
    }

    #[test]
    fn footer_without_audit_round_trips() {
        let entries = vec![entry(b"one", 0), entry(b"two", 100)];
        let mut entries = entries;
        entries.sort_by_key(|entry| entry.blake3);
        let footer = Footer::from_entries(entries, None).unwrap();
        let encoded = footer.encode().unwrap();

        let decoded = Footer::decode(&encoded).unwrap();
        assert!(!decoded.has_audit());
        assert!(decoded.audit().is_none());
        assert_eq!(decoded, footer);
        assert!(decoded.lookup(&hash_body(b"one")).is_some());
    }

    #[test]
    fn footer_checksum_and_layout_validation() {
        let (footer, _) = sample_footer();
        let encoded = footer.encode().unwrap();

        let mut bad_checksum = encoded.clone();
        bad_checksum[super::CHECKSUM_OFFSET] ^= 0xff;
        assert!(matches!(
            Footer::decode(&bad_checksum),
            Err(PackError::FooterChecksumMismatch)
        ));

        let mut bad_magic = encoded.clone();
        bad_magic[0] ^= 0xff;
        assert!(matches!(
            Footer::decode(&bad_magic),
            Err(PackError::InvalidFooterMagic)
        ));

        let mut bad_version = encoded.clone();
        bad_version[4] = 9;
        assert!(matches!(
            Footer::decode(&bad_version),
            Err(PackError::UnsupportedVersion {
                found: 9,
                expected: 1
            })
        ));

        let mut bad_flags = encoded.clone();
        bad_flags[6] = 0x80;
        assert!(matches!(
            Footer::decode(&bad_flags),
            Err(PackError::UnsupportedFlags(_))
        ));

        let mut bad_reserved = encoded.clone();
        bad_reserved[12] = 1;
        assert!(matches!(
            Footer::decode(&bad_reserved),
            Err(PackError::ReservedNotZero)
        ));

        assert!(matches!(
            Footer::decode(&encoded[..FOOTER_HEADER_LEN - 1]),
            Err(PackError::Truncated { .. })
        ));
        assert!(matches!(
            Footer::decode(&encoded[..encoded.len() - 1]),
            Err(PackError::Truncated { .. })
        ));

        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(
            Footer::decode(&trailing),
            Err(PackError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn footer_rejects_unsorted_and_duplicate_entries() {
        let first = entry(b"alpha", 0);
        let second = entry(b"bravo", 100);
        let (low, high) = if first.blake3 < second.blake3 {
            (first, second)
        } else {
            (second, first)
        };

        assert!(matches!(
            Footer::from_entries(vec![high, low], None),
            Err(PackError::EntriesNotSorted)
        ));
        assert!(matches!(
            Footer::from_entries(vec![low, low], None),
            Err(PackError::EntriesNotSorted)
        ));
        assert!(matches!(
            Footer::from_entries(vec![low, high], Some(vec![[0; 32]])),
            Err(PackError::AuditLengthMismatch {
                entries: 2,
                audit: 1
            })
        ));
    }

    #[test]
    fn footer_encodes_audit_flag_and_table_size() {
        let (footer, _) = sample_footer();
        let encoded = footer.encode().unwrap();
        assert_eq!(
            u16::from_le_bytes([encoded[6], encoded[7]]) & FOOTER_FLAG_AUDIT,
            FOOTER_FLAG_AUDIT
        );
        assert_eq!(
            encoded.len(),
            FOOTER_HEADER_LEN + 3 * FOOTER_ENTRY_LEN + 3 * AUDIT_ENTRY_LEN
        );
    }
}
