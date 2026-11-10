//! Streaming pack writer.

use std::collections::HashSet;
use std::io::Write;

use sha2::Digest as _;

use crate::footer::FooterEntry;
use crate::frame::{FRAME_HEADER_LEN, FrameHeader};
use crate::trailer::TRAILER_LEN;
use crate::{ContentHash, Footer, PackError, Trailer, frame_checksum, hash_body};

/// Default zstd compression level used by [`PackWriter::new`].
pub const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

/// Summary of a completed pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackSummary {
    /// Number of content entries in the footer.
    pub entry_count: u32,
    /// Offset of the footer from the start of the pack.
    pub footer_offset: u64,
    /// Length of the encoded footer.
    pub footer_len: u64,
    /// BLAKE3-256 hash of all bytes before the trailer.
    pub pack_blake3: [u8; 32],
    /// SHA-256 hash of all bytes before the trailer.
    pub pack_sha256: [u8; 32],
    /// Total pack length in bytes, including the trailer.
    pub bytes: u64,
}

/// Appends content frames and finishes a pack.
///
/// The writer buffers one compressed frame at a time. Frames are written in
/// append order; the footer is sorted by BLAKE3 hash when the pack is finished.
/// After [`PackWriter::finish`] (or after an I/O failure) the writer rejects
/// further appends.
///
/// # Example
///
/// ```
/// use neuradex_packfile::PackWriter;
///
/// let mut output = Vec::new();
/// let summary = {
///     let mut writer = PackWriter::new(&mut output);
///     let hash = writer.append(b"hello").unwrap();
///     assert_eq!(hash, neuradex_packfile::hash_body(b"hello"));
///     writer.finish().unwrap()
/// };
/// assert_eq!(summary.bytes, u64::try_from(output.len()).unwrap());
/// ```
pub struct PackWriter<W: Write> {
    inner: W,
    compression_level: i32,
    entries: Vec<(FooterEntry, [u8; 32])>,
    seen: HashSet<[u8; 32]>,
    offset: u64,
    pack_blake3: blake3::Hasher,
    pack_sha256: sha2::Sha256,
    poisoned: bool,
    finished: bool,
}

impl<W: Write> PackWriter<W> {
    /// Creates a writer using [`DEFAULT_COMPRESSION_LEVEL`].
    #[must_use]
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            compression_level: DEFAULT_COMPRESSION_LEVEL,
            entries: Vec::new(),
            seen: HashSet::new(),
            offset: 0,
            pack_blake3: blake3::Hasher::new(),
            pack_sha256: sha2::Sha256::new(),
            poisoned: false,
            finished: false,
        }
    }

    /// Creates a writer with an explicit zstd compression level.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::InvalidCompressionLevel`] when `level` is outside
    /// the range accepted by zstd.
    pub fn with_compression_level(inner: W, level: i32) -> Result<Self, PackError> {
        let range = zstd::compression_level_range();
        if !range.contains(&level) {
            return Err(PackError::InvalidCompressionLevel {
                level,
                min: *range.start(),
                max: *range.end(),
            });
        }
        let mut writer = Self::new(inner);
        writer.compression_level = level;
        Ok(writer)
    }

    /// Returns the number of frames appended so far.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether no frames have been appended.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the zstd compression level.
    #[must_use]
    pub const fn compression_level(&self) -> i32 {
        self.compression_level
    }

    /// Appends one plaintext body as a zstd frame.
    ///
    /// Returns the computed [`ContentHash`]. The writer holds at most the
    /// compressed form of this one body while writing.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::DuplicateHash`] when the body's BLAKE3 hash is
    /// already in this pack, [`PackError::ContentTooLarge`] when the plaintext
    /// does not fit the footer's 32-bit length, or when compression or writing
    /// fails.
    pub fn append(&mut self, body: &[u8]) -> Result<ContentHash, PackError> {
        self.ensure_usable()?;

        let content = hash_body(body);
        if self.seen.contains(&content.blake3) {
            return Err(PackError::DuplicateHash);
        }

        let plaintext_len = u32::try_from(body.len()).map_err(|_| PackError::ContentTooLarge)?;
        let compressed = zstd::stream::encode_all(body, self.compression_level)
            .map_err(PackError::Compression)?;
        let compressed_len =
            u32::try_from(compressed.len()).map_err(|_| PackError::ContentTooLarge)?;
        let header = FrameHeader::new(
            u64::from(plaintext_len),
            compressed_len,
            content.blake3,
            content.sha256,
            frame_checksum(&compressed),
        );

        let frame_len = u64::try_from(FRAME_HEADER_LEN)
            .map_err(|_| PackError::Overflow)?
            .checked_add(u64::from(compressed_len))
            .ok_or(PackError::Overflow)?;
        let next_offset = self
            .offset
            .checked_add(frame_len)
            .ok_or(PackError::Overflow)?;
        let frame_offset = self.offset;

        self.write_hashed(&header.encode())?;
        self.write_hashed(&compressed)?;
        self.offset = next_offset;

        let entry = FooterEntry::new(
            content.blake3,
            frame_offset,
            compressed_len,
            plaintext_len,
            header.checksum,
            header.flags,
        );
        self.seen.insert(content.blake3);
        self.entries.push((entry, content.sha256));
        Ok(content)
    }

    /// Writes the footer and trailer and returns a summary of the pack.
    ///
    /// The writer cannot be used again after this call. Use
    /// [`PackWriter::into_inner`] to recover an owned sink.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer was already finished or poisoned, when
    /// the footer cannot be encoded, or when writing or flushing fails.
    pub fn finish(&mut self) -> Result<PackSummary, PackError> {
        self.ensure_usable()?;
        self.finished = true;

        let mut entries = std::mem::take(&mut self.entries);
        entries.sort_unstable_by_key(|entry| entry.0.blake3);
        let (footer_entries, audit): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
        let footer = Footer::from_entries(footer_entries, Some(audit))?;
        let footer_bytes = footer.encode()?;
        let footer_offset = self.offset;
        let footer_len = u64::try_from(footer_bytes.len()).map_err(|_| PackError::Overflow)?;

        self.write_hashed(&footer_bytes)?;
        self.offset = footer_offset
            .checked_add(footer_len)
            .ok_or(PackError::Overflow)?;

        let pack_blake3 = *self.pack_blake3.clone().finalize().as_bytes();
        let pack_sha256: [u8; 32] = self.pack_sha256.clone().finalize().into();
        let trailer = Trailer::new(
            footer_offset,
            footer_len,
            pack_blake3,
            pack_sha256,
            footer.checksum(),
        );
        self.inner.write_all(&trailer.encode())?;
        self.inner.flush()?;

        let bytes = self
            .offset
            .checked_add(u64::try_from(TRAILER_LEN).map_err(|_| PackError::Overflow)?)
            .ok_or(PackError::Overflow)?;
        let entry_count = u32::try_from(footer.len()).map_err(|_| PackError::TooManyEntries {
            count: footer.len(),
        })?;

        Ok(PackSummary {
            entry_count,
            footer_offset,
            footer_len,
            pack_blake3,
            pack_sha256,
            bytes,
        })
    }

    /// Consumes the writer and returns the underlying sink.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner
    }

    const fn ensure_usable(&self) -> Result<(), PackError> {
        if self.poisoned || self.finished {
            return Err(PackError::WriterPoisoned);
        }
        Ok(())
    }

    fn write_hashed(&mut self, bytes: &[u8]) -> Result<(), PackError> {
        if let Err(error) = self.inner.write_all(bytes) {
            self.poisoned = true;
            return Err(PackError::Io(error));
        }
        self.pack_blake3.update(bytes);
        self.pack_sha256.update(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_COMPRESSION_LEVEL, PackWriter};
    use crate::{PackError, hash_body};

    #[test]
    fn duplicate_content_is_rejected() {
        let mut output = Vec::new();
        let mut writer = PackWriter::new(&mut output);
        writer.append(b"same body").unwrap();
        assert!(matches!(
            writer.append(b"same body"),
            Err(PackError::DuplicateHash)
        ));
        assert_eq!(writer.len(), 1);

        writer.append(b"different body").unwrap();
        assert_eq!(writer.len(), 2);
        let summary = writer.finish().unwrap();
        assert_eq!(summary.entry_count, 2);
    }

    #[test]
    fn writer_is_unusable_after_finish() {
        let mut output = Vec::new();
        let mut writer = PackWriter::new(&mut output);
        writer.append(b"body").unwrap();
        writer.finish().unwrap();
        assert!(matches!(
            writer.append(b"another"),
            Err(PackError::WriterPoisoned)
        ));
        assert!(matches!(writer.finish(), Err(PackError::WriterPoisoned)));
    }

    #[test]
    fn compression_level_is_validated_and_retained() {
        let mut writer = PackWriter::with_compression_level(Vec::new(), 7).unwrap();
        assert_eq!(writer.compression_level(), 7);
        assert_eq!(writer.append(b"body").unwrap(), hash_body(b"body"));
        assert!(writer.finish().is_ok());

        assert!(matches!(
            PackWriter::with_compression_level(Vec::new(), i32::MAX),
            Err(PackError::InvalidCompressionLevel { .. })
        ));
    }

    #[test]
    fn default_level_is_used() {
        let writer = PackWriter::new(Vec::new());
        assert_eq!(writer.compression_level(), DEFAULT_COMPRESSION_LEVEL);
        assert!(writer.is_empty());
    }

    #[test]
    fn into_inner_returns_the_sink() {
        let writer = PackWriter::new(Vec::new());
        assert_eq!(writer.into_inner(), Vec::<u8>::new());
    }
}
