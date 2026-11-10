//! Errors returned by the packfile codec.

/// An error produced while encoding, decoding, or verifying a packfile.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PackError {
    /// The input ended before the requested structure was complete.
    #[error("unexpected end of input: needed {needed} bytes, have {available}")]
    Truncated {
        /// Number of bytes required.
        needed: usize,
        /// Number of bytes available.
        available: usize,
    },
    /// The input length does not match the length required by the format.
    #[error("length mismatch: expected {expected} bytes, found {found}")]
    LengthMismatch {
        /// Expected number of bytes.
        expected: usize,
        /// Observed number of bytes.
        found: usize,
    },
    /// A frame did not start with the frame magic.
    #[error("invalid frame magic")]
    InvalidFrameMagic,
    /// A footer did not start with the footer magic.
    #[error("invalid footer magic")]
    InvalidFooterMagic,
    /// A trailer did not start with the trailer magic.
    #[error("invalid trailer magic")]
    InvalidTrailerMagic,
    /// The structure uses a format version that this crate does not support.
    #[error("unsupported format version {found} (expected {expected})")]
    UnsupportedVersion {
        /// Version found in the input.
        found: u16,
        /// Version supported by this crate.
        expected: u16,
    },
    /// The structure sets flag bits that this crate does not understand.
    #[error("unsupported flags {0:#06x}")]
    UnsupportedFlags(u16),
    /// Bytes documented as reserved were not zero.
    #[error("reserved bytes are not zero")]
    ReservedNotZero,
    /// The footer checksum did not match the footer contents.
    #[error("footer checksum mismatch")]
    FooterChecksumMismatch,
    /// The checksum of the compressed frame bytes did not match the frame header.
    #[error("frame checksum mismatch")]
    FrameChecksumMismatch,
    /// The BLAKE3 hash of the decoded plaintext did not match the frame header.
    #[error("BLAKE3 content hash mismatch")]
    Blake3Mismatch,
    /// The SHA-256 hash of the decoded plaintext did not match the frame header.
    #[error("SHA-256 content hash mismatch")]
    Sha256Mismatch,
    /// The pack hashes in the trailer did not match the pack contents.
    #[error("pack hash mismatch")]
    PackHashMismatch,
    /// Footer entries are not strictly increasing by BLAKE3 hash.
    #[error("footer entries are not strictly sorted by BLAKE3 hash")]
    EntriesNotSorted,
    /// The same BLAKE3 content hash was added to a pack more than once.
    #[error("duplicate content hash")]
    DuplicateHash,
    /// There are more footer entries than the 32-bit entry count can express.
    #[error("too many footer entries: {count}")]
    TooManyEntries {
        /// Number of entries that were supplied.
        count: usize,
    },
    /// The audit table does not have one SHA-256 hash per footer entry.
    #[error("audit table has {audit} hashes for {entries} entries")]
    AuditLengthMismatch {
        /// Number of footer entries.
        entries: usize,
        /// Number of audit hashes.
        audit: usize,
    },
    /// A frame or content length exceeds the limits of the format.
    #[error("content is too large for the packfile format")]
    ContentTooLarge,
    /// A length or offset calculation overflowed.
    #[error("length or offset overflow")]
    Overflow,
    /// The footer, frames, and trailer do not describe a consistent pack layout.
    #[error("invalid pack layout")]
    InvalidLayout,
    /// A byte range lies outside the bounds of the structure that contains it.
    #[error("range {start}..{end} is out of bounds (limit {len})")]
    OutOfBounds {
        /// Inclusive start offset.
        start: u64,
        /// Exclusive end offset.
        end: u64,
        /// Exclusive upper bound.
        len: u64,
    },
    /// A footer entry does not describe the frame header at its offset.
    #[error("footer entry does not match its frame header")]
    EntryMismatch,
    /// A requested footer entry index is out of range.
    #[error("footer entry index {index} is out of bounds (entry count {count})")]
    EntryIndexOutOfBounds {
        /// Requested index.
        index: usize,
        /// Number of entries in the footer.
        count: usize,
    },
    /// The requested compression level is outside the range accepted by zstd.
    #[error("invalid compression level {level} (expected {min}..={max})")]
    InvalidCompressionLevel {
        /// The requested level.
        level: i32,
        /// Lowest accepted level.
        min: i32,
        /// Highest accepted level.
        max: i32,
    },
    /// The frame does not carry the zstd compression flag.
    #[error("frame is not zstd-compressed")]
    FrameNotCompressed,
    /// Compressing a body failed.
    #[error("compression failed")]
    Compression(#[source] std::io::Error),
    /// The compressed bytes are not a valid zstd frame.
    #[error("compressed frame is not valid zstd")]
    InvalidZstd(#[source] std::io::Error),
    /// Writing to the output sink failed.
    #[error("i/o error")]
    Io(#[from] std::io::Error),
    /// The writer was already used after a previous failure or after `finish`.
    #[error("pack writer is no longer usable")]
    WriterPoisoned,
}
