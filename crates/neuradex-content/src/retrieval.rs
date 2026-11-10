//! Range-based pack retrieval.
//!
//! [`read_pack_content`] never downloads a whole pack. It performs at most
//! three range reads:
//!
//! 1. the last [`TRAILER_LEN`] bytes, decoded with [`Trailer::decode`];
//! 2. the footer at `trailer.footer_offset` with `trailer.footer_len`, decoded
//!    with [`Footer::decode`];
//! 3. the frame header and compressed payload at the footer entry's
//!    `frame_offset`, decoded with [`FrameHeader::decode`] and
//!    [`decode_frame`].
//!
//! The range reader checks that the footer checksum equals the trailer's copy,
//! that the frame header matches its footer entry, and that the decoded
//! plaintext matches both hashes. It does not verify the pack-level hashes
//! because that requires every byte of the pack; use
//! [`PackReader`](neuradex_packfile::PackReader) when the full pack is already
//! in memory.

use std::ops::Range;

use neuradex_packfile::{
    ContentHash, FRAME_HEADER_LEN, Footer, FrameHeader, PackError, TRAILER_LEN, Trailer,
    decode_frame, hash_body,
};

use crate::{ContentError, ContentStorage, ContentStore, StoreError, StoredContent};

/// Maximum encoded footer size accepted by range retrieval.
const MAX_FOOTER_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum compressed frame size accepted by range retrieval.
const MAX_FRAME_COMPRESSED_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum decoded frame size accepted by range retrieval.
const MAX_FRAME_PLAINTEXT_BYTES: u64 = 256 * 1024 * 1024;

/// Reads one content hash from a pack using range requests.
///
/// Returns `Ok(None)` when the pack is valid but the hash is absent. A missing
/// pack key is reported as [`StoreError::NotFound`] through
/// [`ContentError::Store`].
///
/// # Errors
///
/// Returns a storage error when a range read fails, or a
/// [`ContentError::Pack`] error when a trailer, footer, frame header, compressed
/// frame, or content hash is invalid.
pub async fn read_pack_content(
    store: &ContentStore,
    pack_key: &str,
    hash: &ContentHash,
) -> Result<Option<Vec<u8>>, ContentError> {
    let pack_len = store.object_size(pack_key).await?;
    let trailer_start =
        pack_len
            .checked_sub(TRAILER_LEN as u64)
            .ok_or_else(|| PackError::Truncated {
                needed: TRAILER_LEN,
                available: usize::try_from(pack_len).unwrap_or(usize::MAX),
            })?;

    let trailer_bytes = get_exact(store, pack_key, trailer_start..pack_len).await?;
    let trailer = Trailer::decode(&trailer_bytes)?;

    let footer_end = trailer
        .footer_offset
        .checked_add(trailer.footer_len)
        .ok_or(PackError::Overflow)?;
    if footer_end != trailer_start {
        return Err(PackError::InvalidLayout.into());
    }
    if trailer.footer_len > MAX_FOOTER_BYTES {
        return Err(ContentError::FooterTooLarge {
            len: trailer.footer_len,
            limit: MAX_FOOTER_BYTES,
        });
    }

    let footer_bytes = get_exact(store, pack_key, trailer.footer_offset..footer_end).await?;
    let footer = Footer::decode(&footer_bytes)?;
    if footer.checksum() != trailer.footer_checksum {
        return Err(PackError::FooterChecksumMismatch.into());
    }

    let Some(entry) = footer.lookup(hash) else {
        return Ok(None);
    };

    let header_end = entry
        .frame_offset
        .checked_add(FRAME_HEADER_LEN as u64)
        .ok_or(PackError::Overflow)?;
    let frame_end = header_end
        .checked_add(u64::from(entry.compressed_len))
        .ok_or(PackError::Overflow)?;
    if frame_end > trailer.footer_offset {
        return Err(PackError::OutOfBounds {
            start: entry.frame_offset,
            end: frame_end,
            len: trailer.footer_offset,
        }
        .into());
    }
    if u64::from(entry.compressed_len) > MAX_FRAME_COMPRESSED_BYTES {
        return Err(ContentError::FrameTooLarge {
            len: u64::from(entry.compressed_len),
            limit: MAX_FRAME_COMPRESSED_BYTES,
        });
    }
    if u64::from(entry.plaintext_len) > MAX_FRAME_PLAINTEXT_BYTES {
        return Err(ContentError::FrameTooLarge {
            len: u64::from(entry.plaintext_len),
            limit: MAX_FRAME_PLAINTEXT_BYTES,
        });
    }

    let frame_bytes = get_exact(store, pack_key, entry.frame_offset..frame_end).await?;
    let header_bytes = frame_bytes
        .get(..FRAME_HEADER_LEN)
        .ok_or(PackError::Truncated {
            needed: FRAME_HEADER_LEN,
            available: frame_bytes.len(),
        })?;
    let header = FrameHeader::decode(header_bytes)?;

    if header.flags != entry.flags
        || header.plaintext_len != u64::from(entry.plaintext_len)
        || header.compressed_len != entry.compressed_len
        || header.blake3 != entry.blake3
        || header.checksum != entry.xxh3_64
    {
        return Err(PackError::EntryMismatch.into());
    }
    if header.sha256 != hash.sha256 {
        return Err(PackError::Sha256Mismatch.into());
    }

    let compressed = frame_bytes
        .get(FRAME_HEADER_LEN..)
        .ok_or(PackError::Truncated {
            needed: FRAME_HEADER_LEN,
            available: frame_bytes.len(),
        })?;
    decode_frame(&header, compressed)
        .map(Some)
        .map_err(Into::into)
}

/// Reads a stored-content reference from its store.
///
/// Pack references use [`read_pack_content`]. Object references read the whole
/// object and verify its size and content hash. Returns `Ok(None)` when a pack
/// does not contain the hash or an object key does not exist.
///
/// # Errors
///
/// Returns a storage error when the read fails, or a
/// [`ContentError::Pack`] / [`ContentError::ObjectHashMismatch`] error when the
/// stored bytes are invalid.
pub async fn read_content(
    store: &ContentStore,
    content: &StoredContent,
) -> Result<Option<Vec<u8>>, ContentError> {
    match content.storage() {
        ContentStorage::Pack => read_pack_content(store, content.key(), &content.hash()).await,
        ContentStorage::Object => {
            let expected = usize::try_from(content.size()).map_err(|_| {
                ContentError::InvalidState("stored object size does not fit in memory")
            })?;
            let bytes = match store.get_range(content.key(), 0..content.size()).await {
                Ok(bytes) => bytes,
                Err(StoreError::NotFound { .. }) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            if bytes.len() != expected {
                return Err(PackError::Truncated {
                    needed: expected,
                    available: bytes.len(),
                }
                .into());
            }
            if hash_body(&bytes) != content.hash() {
                return Err(ContentError::ObjectHashMismatch {
                    key: content.key().to_owned(),
                });
            }
            Ok(Some(bytes))
        }
    }
}

async fn get_exact(
    store: &ContentStore,
    key: &str,
    range: Range<u64>,
) -> Result<Vec<u8>, ContentError> {
    let expected = usize::try_from(range.end.saturating_sub(range.start))
        .map_err(|_| ContentError::InvalidState("requested range does not fit in memory"))?;
    let bytes = store.get_range(key, range).await?;
    if bytes.len() != expected {
        return Err(PackError::Truncated {
            needed: expected,
            available: bytes.len(),
        }
        .into());
    }
    Ok(bytes)
}

#[cfg(all(test, feature = "local"))]
mod tests {
    use neuradex_packfile::{ContentHash, FRAME_HEADER_LEN, PackReader, PackWriter, hash_body};

    use super::{read_content, read_pack_content};
    use crate::{ContentStorage, ContentStore, LocalStore, StoredContent};

    struct Fixture {
        _directory: tempfile::TempDir,
        local: LocalStore,
        store: ContentStore,
        key: String,
        bodies: Vec<Vec<u8>>,
        hashes: Vec<ContentHash>,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let local = LocalStore::new(directory.path().join("store")).unwrap();
        let bodies = vec![
            b"first body".to_vec(),
            b"second body with more bytes".to_vec(),
            b"third".to_vec(),
        ];
        let mut bytes = Vec::new();
        let mut writer = PackWriter::new(&mut bytes);
        let hashes: Vec<ContentHash> = bodies
            .iter()
            .map(|body| writer.append(body).unwrap())
            .collect();
        writer.finish().unwrap();

        let key = "packs/fetch/2026-11-10T00/1.pack".to_owned();
        local.put_bytes(&key, &bytes).unwrap();
        let store = ContentStore::Local(local.clone());
        Fixture {
            _directory: directory,
            local,
            store,
            key,
            bodies,
            hashes,
        }
    }

    #[tokio::test]
    async fn range_retrieval_reads_every_body_and_matches_the_full_reader() {
        let fixture = fixture();
        let bytes = fixture.local.get(&fixture.key).unwrap();
        let reader = PackReader::new(&bytes).unwrap();

        for (body, hash) in fixture.bodies.iter().zip(&fixture.hashes) {
            assert_eq!(
                read_pack_content(&fixture.store, &fixture.key, hash)
                    .await
                    .unwrap()
                    .unwrap(),
                *body
            );
            assert_eq!(reader.read(hash).unwrap().unwrap(), *body);
        }
    }

    #[tokio::test]
    async fn absent_hashes_return_none() {
        let fixture = fixture();

        assert!(
            read_pack_content(&fixture.store, &fixture.key, &hash_body(b"absent"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn audit_hash_mismatch_returns_none() {
        let fixture = fixture();
        let mut wrong = fixture.hashes[0];
        wrong.sha256[0] ^= 0xff;

        assert!(
            read_pack_content(&fixture.store, &fixture.key, &wrong)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn corrupt_footer_is_rejected() {
        let fixture = fixture();
        let mut bytes = fixture.local.get(&fixture.key).unwrap();
        let trailer = neuradex_packfile::Trailer::from_pack(&bytes).unwrap();
        let checksum_byte = usize::try_from(trailer.footer_offset).unwrap() + 16;
        bytes[checksum_byte] ^= 0xff;
        fixture.local.put_bytes("packs/corrupt", &bytes).unwrap();

        let error = read_pack_content(&fixture.store, "packs/corrupt", &fixture.hashes[0])
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                crate::ContentError::Pack(neuradex_packfile::PackError::FooterChecksumMismatch)
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn corrupt_frame_is_rejected() {
        let fixture = fixture();
        let mut bytes = fixture.local.get(&fixture.key).unwrap();
        let reader = PackReader::new(&bytes).unwrap();
        let entry = reader.lookup(&fixture.hashes[0]).unwrap();
        let compressed_byte = usize::try_from(entry.frame_offset).unwrap() + FRAME_HEADER_LEN;
        bytes[compressed_byte] ^= 0xff;
        fixture.local.put_bytes("packs/frame", &bytes).unwrap();

        let error = read_pack_content(&fixture.store, "packs/frame", &fixture.hashes[0])
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                crate::ContentError::Pack(
                    neuradex_packfile::PackError::FrameChecksumMismatch
                        | neuradex_packfile::PackError::InvalidZstd(_)
                        | neuradex_packfile::PackError::Blake3Mismatch
                )
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn truncated_pack_is_rejected() {
        let fixture = fixture();
        fixture.local.put_bytes("packs/short", b"short").unwrap();

        let error = read_pack_content(&fixture.store, "packs/short", &fixture.hashes[0])
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                crate::ContentError::Pack(neuradex_packfile::PackError::Truncated { .. })
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn object_references_are_read_and_verified() {
        let fixture = fixture();
        let body = b"object body".to_vec();
        let hash = hash_body(&body);
        let key = "objects/object-body";
        fixture.local.put_bytes(key, &body).unwrap();
        let content =
            StoredContent::new(ContentStorage::Object, key, hash, body.len() as u64, None);

        assert_eq!(
            read_content(&fixture.store, &content)
                .await
                .unwrap()
                .unwrap(),
            body
        );

        let missing = StoredContent::new(
            ContentStorage::Object,
            "objects/missing",
            hash,
            body.len() as u64,
            None,
        );
        assert!(
            read_content(&fixture.store, &missing)
                .await
                .unwrap()
                .is_none()
        );

        let wrong_hash = StoredContent::new(
            ContentStorage::Object,
            key,
            hash_body(b"different"),
            body.len() as u64,
            None,
        );
        assert!(matches!(
            read_content(&fixture.store, &wrong_hash).await,
            Err(crate::ContentError::ObjectHashMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn pack_references_go_through_the_range_path() {
        let fixture = fixture();
        let content = StoredContent::new(
            ContentStorage::Pack,
            &fixture.key,
            fixture.hashes[1],
            fixture.bodies[1].len() as u64,
            Some("text/plain".to_owned()),
        );

        assert_eq!(
            read_content(&fixture.store, &content)
                .await
                .unwrap()
                .unwrap(),
            fixture.bodies[1]
        );
    }
}
