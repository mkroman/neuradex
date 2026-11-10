//! Pure-bytes pack reader.

use crate::footer::FooterEntry;
use crate::frame::{FRAME_HEADER_LEN, FrameHeader, decode_frame};
use crate::trailer::TRAILER_LEN;
use crate::{ContentHash, Footer, PackError, Trailer, hash_body};

/// A borrowed frame inside a pack.
#[derive(Debug, Clone, Copy)]
pub struct FrameRef<'a> {
    /// Decoded frame header.
    pub header: FrameHeader,
    /// Compressed zstd bytes that follow the header.
    pub compressed: &'a [u8],
}

impl FrameRef<'_> {
    /// Returns the content hash recorded in the frame header.
    #[must_use]
    pub const fn content_hash(&self) -> ContentHash {
        self.header.content_hash()
    }

    /// Decodes and verifies this frame's plaintext.
    ///
    /// # Errors
    ///
    /// Returns any error from [`decode_frame`], including checksum, zstd,
    /// length, or content hash mismatches.
    pub fn decode(&self) -> Result<Vec<u8>, PackError> {
        decode_frame(&self.header, self.compressed)
    }
}

/// A validated view over the bytes of one complete pack.
///
/// Construction verifies the trailer, footer checksum, footer entry bounds,
/// and the pack-level hashes. Frame payloads are verified lazily when a frame
/// is decoded.
#[derive(Debug)]
pub struct PackReader<'a> {
    bytes: &'a [u8],
    trailer: Trailer,
    footer: Footer,
}

impl<'a> PackReader<'a> {
    /// Opens and validates a complete pack from its bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the trailer or footer is truncated, malformed, or
    /// has a bad magic, version, flags, or checksum; when the footer and
    /// trailer disagree about the footer location; when a footer entry points
    /// outside the frame region; or when the pack hashes do not match.
    pub fn new(bytes: &'a [u8]) -> Result<Self, PackError> {
        let trailer = Trailer::from_pack(bytes)?;
        let trailer_start = bytes
            .len()
            .checked_sub(TRAILER_LEN)
            .ok_or(PackError::Truncated {
                needed: TRAILER_LEN,
                available: bytes.len(),
            })?;
        let pack_len = u64::try_from(bytes.len()).map_err(|_| PackError::Overflow)?;
        let footer_range_end = trailer
            .footer_offset
            .checked_add(trailer.footer_len)
            .ok_or(PackError::Overflow)?;

        let footer_start =
            usize::try_from(trailer.footer_offset).map_err(|_| PackError::OutOfBounds {
                start: trailer.footer_offset,
                end: footer_range_end,
                len: pack_len,
            })?;
        let footer_len =
            usize::try_from(trailer.footer_len).map_err(|_| PackError::OutOfBounds {
                start: trailer.footer_offset,
                end: footer_range_end,
                len: pack_len,
            })?;
        let footer_end = footer_start
            .checked_add(footer_len)
            .ok_or(PackError::Overflow)?;

        if footer_end != trailer_start {
            return Err(PackError::InvalidLayout);
        }

        let footer_bytes = bytes
            .get(footer_start..footer_end)
            .ok_or(PackError::OutOfBounds {
                start: trailer.footer_offset,
                end: footer_range_end,
                len: pack_len,
            })?;
        let footer = Footer::decode(footer_bytes)?;
        if footer.checksum() != trailer.footer_checksum {
            return Err(PackError::FooterChecksumMismatch);
        }
        validate_entry_bounds(&footer, trailer.footer_offset)?;

        let pack_bytes = bytes.get(..trailer_start).ok_or(PackError::OutOfBounds {
            start: 0,
            end: u64::try_from(trailer_start).map_err(|_| PackError::Overflow)?,
            len: pack_len,
        })?;
        let actual = hash_body(pack_bytes);
        if actual.blake3 != trailer.pack_blake3 || actual.sha256 != trailer.pack_sha256 {
            return Err(PackError::PackHashMismatch);
        }

        Ok(Self {
            bytes,
            trailer,
            footer,
        })
    }

    /// Returns the decoded footer.
    #[must_use]
    pub const fn footer(&self) -> &Footer {
        &self.footer
    }

    /// Returns the decoded trailer.
    #[must_use]
    pub const fn trailer(&self) -> &Trailer {
        &self.trailer
    }

    /// Looks up an entry by content hash.
    #[must_use]
    pub fn lookup(&self, hash: &ContentHash) -> Option<&FooterEntry> {
        self.footer.lookup(hash)
    }

    /// Returns the frame at a footer entry.
    ///
    /// The frame header is decoded and checked against the entry, but the
    /// compressed payload is not decompressed until [`FrameRef::decode`] is
    /// called.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry points outside the frame region, when
    /// the frame header is malformed, or when the header does not match the
    /// footer entry.
    pub fn frame(&self, entry: &FooterEntry) -> Result<FrameRef<'a>, PackError> {
        let header_len = u64::try_from(FRAME_HEADER_LEN).map_err(|_| PackError::Overflow)?;
        let end = entry
            .frame_offset
            .checked_add(header_len)
            .and_then(|offset| offset.checked_add(u64::from(entry.compressed_len)))
            .ok_or(PackError::Overflow)?;
        if end > self.trailer.footer_offset {
            return Err(PackError::OutOfBounds {
                start: entry.frame_offset,
                end,
                len: self.trailer.footer_offset,
            });
        }

        let start = usize::try_from(entry.frame_offset).map_err(|_| PackError::OutOfBounds {
            start: entry.frame_offset,
            end,
            len: self.trailer.footer_offset,
        })?;
        let header_end = start
            .checked_add(FRAME_HEADER_LEN)
            .ok_or(PackError::Overflow)?;
        let compressed_start = header_end;
        let compressed_end = compressed_start
            .checked_add(
                usize::try_from(entry.compressed_len).map_err(|_| PackError::ContentTooLarge)?,
            )
            .ok_or(PackError::Overflow)?;

        let header_bytes = self
            .bytes
            .get(start..header_end)
            .ok_or(PackError::OutOfBounds {
                start: entry.frame_offset,
                end,
                len: self.trailer.footer_offset,
            })?;
        let compressed =
            self.bytes
                .get(compressed_start..compressed_end)
                .ok_or(PackError::OutOfBounds {
                    start: entry.frame_offset,
                    end,
                    len: self.trailer.footer_offset,
                })?;

        let header = FrameHeader::decode(header_bytes)?;
        if header.flags != entry.flags
            || header.plaintext_len != u64::from(entry.plaintext_len)
            || header.compressed_len != entry.compressed_len
            || header.blake3 != entry.blake3
            || header.checksum != entry.xxh3_64
        {
            return Err(PackError::EntryMismatch);
        }

        if let Some(audit) = self.footer.audit() {
            let index = self
                .footer
                .entries()
                .binary_search_by(|candidate| candidate.blake3.cmp(&entry.blake3))
                .ok();
            if let Some(index) = index
                && audit.get(index) != Some(&header.sha256)
            {
                return Err(PackError::EntryMismatch);
            }
        }

        Ok(FrameRef { header, compressed })
    }

    /// Returns the frame at a footer index.
    ///
    /// # Errors
    ///
    /// Returns [`PackError::EntryIndexOutOfBounds`] when `index` is not a valid
    /// footer entry, or any error from [`PackReader::frame`].
    pub fn frame_at(&self, index: usize) -> Result<FrameRef<'a>, PackError> {
        let entry = self
            .footer
            .entries()
            .get(index)
            .ok_or(PackError::EntryIndexOutOfBounds {
                index,
                count: self.footer.len(),
            })?;
        self.frame(entry)
    }

    /// Decodes and verifies the plaintext referenced by a footer entry.
    ///
    /// # Errors
    ///
    /// Returns any error from [`PackReader::frame`] or [`FrameRef::decode`].
    pub fn read_entry(&self, entry: &FooterEntry) -> Result<Vec<u8>, PackError> {
        self.frame(entry)?.decode()
    }

    /// Looks up and decodes content by hash.
    ///
    /// Returns `Ok(None)` when the hash is not in the footer.
    ///
    /// # Errors
    ///
    /// Returns any error from [`PackReader::read_entry`] when the hash is
    /// present but its frame is corrupt.
    pub fn read(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>, PackError> {
        let Some(entry) = self.lookup(hash) else {
            return Ok(None);
        };
        self.read_entry(entry).map(Some)
    }
}

fn validate_entry_bounds(footer: &Footer, footer_offset: u64) -> Result<(), PackError> {
    let header_len = u64::try_from(FRAME_HEADER_LEN).map_err(|_| PackError::Overflow)?;
    for entry in footer.entries() {
        let end = entry
            .frame_offset
            .checked_add(header_len)
            .and_then(|offset| offset.checked_add(u64::from(entry.compressed_len)))
            .ok_or(PackError::Overflow)?;
        if end > footer_offset {
            return Err(PackError::OutOfBounds {
                start: entry.frame_offset,
                end,
                len: footer_offset,
            });
        }
    }
    Ok(())
}
