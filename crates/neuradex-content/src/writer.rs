//! Pack orchestration: append bodies, seal packs, and upload them.
//!
//! [`ContentWriter::append`] is synchronous because it only writes to local
//! temporary files and a [`PackWriter`]. [`ContentWriter::flush`] is async
//! because it uploads sealed packs and individual objects to the configured
//! store. A caller running on tokio should invoke `append` from
//! `tokio::task::spawn_blocking` or otherwise off the async hot path.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use neuradex_packfile::{ContentHash, PackSummary, PackWriter, hash_body};
use tempfile::NamedTempFile;

use crate::stored_content::StoredContent;
use crate::{CasPolicy, ContentError, ContentStorage, ContentStore};

/// Process-wide pack sequence. Combined with the process id and hour, this
/// keeps pack keys unique without randomness.
static NEXT_PACK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A sealed pack that was uploaded by [`ContentWriter::flush`].
#[derive(Debug, Clone)]
pub struct FlushedPack {
    /// Store key of the uploaded pack.
    pub key: String,
    /// Summary returned by the pack writer.
    pub summary: PackSummary,
    /// References to the bodies in the pack, with `pack_summary` filled in.
    pub contents: Vec<StoredContent>,
}

/// The result of one [`ContentWriter::flush`].
#[derive(Debug, Clone, Default)]
pub struct FlushResult {
    /// Packs uploaded by this flush, in seal order.
    pub packs: Vec<FlushedPack>,
    /// Individual objects uploaded by this flush, in append order.
    pub objects: Vec<StoredContent>,
}

impl FlushResult {
    /// Returns whether the flush had nothing to upload.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.packs.is_empty() && self.objects.is_empty()
    }
}

/// Groups bodies into packfiles and uploads sealed packs on [`flush`](Self::flush).
///
/// Bodies are grouped by operation into separate packs. A pack is opened
/// lazily on the first append for that operation, and its key is fixed at that
/// point: `packs/{operation}/{yyyy-mm-ddTHH}/{pid}-{sequence}.pack`. Bodies at
/// or above the policy's large-body threshold are written to a temporary file
/// and uploaded as `objects/{blake3}` instead.
///
/// Duplicate BLAKE3 hashes within one open pack return the existing reference
/// instead of failing; there is no global deduplication across packs.
///
/// A writer that is dropped without a successful `flush` discards its temporary
/// files.
pub struct ContentWriter {
    store: ContentStore,
    policy: CasPolicy,
    temp_dir: PathBuf,
    packs: HashMap<String, OpenPack>,
    sealed: VecDeque<SealedPack>,
    objects: VecDeque<PendingObject>,
}

struct OpenPack {
    key: String,
    writer: PackWriter<NamedTempFile>,
    plaintext_bytes: u64,
    opened_at: Instant,
    entries: HashMap<[u8; 32], StoredContent>,
    contents: Vec<StoredContent>,
}

struct SealedPack {
    key: String,
    summary: PackSummary,
    temp: NamedTempFile,
    contents: Vec<StoredContent>,
}

struct PendingObject {
    temp: NamedTempFile,
    stored: StoredContent,
}

impl ContentWriter {
    /// Creates a writer that puts temporary files in the system temporary
    /// directory.
    #[must_use]
    pub fn new(store: ContentStore, policy: CasPolicy) -> Self {
        Self::new_in(store, policy, std::env::temp_dir())
    }

    /// Creates a writer that puts temporary files in `temp_dir`.
    ///
    /// The directory is not created; appends fail with an I/O error when it
    /// does not exist.
    #[must_use]
    pub fn new_in(store: ContentStore, policy: CasPolicy, temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            store,
            policy,
            temp_dir: temp_dir.into(),
            packs: HashMap::new(),
            sealed: VecDeque::new(),
            objects: VecDeque::new(),
        }
    }

    /// Returns the policy used by this writer.
    #[must_use]
    pub const fn policy(&self) -> &CasPolicy {
        &self.policy
    }

    /// Returns whether there are no open packs, sealed packs, or objects
    /// waiting to be uploaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packs.is_empty() && self.sealed.is_empty() && self.objects.is_empty()
    }

    /// Appends one body to the pack for `operation`, or queues it as an
    /// individual object when the policy says so.
    ///
    /// The returned reference is final except that a pack reference does not
    /// carry a [`PackSummary`] until [`flush`](Self::flush) seals the pack.
    /// The body is durable only after a successful flush.
    ///
    /// A duplicate body in the same open pack returns the reference created by
    /// the first append, including the first append's content type.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::BodyTooLarge`] when the body exceeds the policy
    /// limit, [`ContentError::Pack`] when compression or pack writing fails, or
    /// an I/O error when a temporary file cannot be created or written.
    pub fn append(
        &mut self,
        operation: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) -> Result<StoredContent, ContentError> {
        let size = u64::try_from(body.len()).map_err(|_| ContentError::BodyTooLarge {
            size: u64::MAX,
            limit: self.policy.max_captured_body_bytes(),
        })?;
        if size > self.policy.max_captured_body_bytes() {
            return Err(ContentError::BodyTooLarge {
                size,
                limit: self.policy.max_captured_body_bytes(),
            });
        }

        let hash = hash_body(body);
        if self.policy.should_store_individually(size) {
            return self.queue_object(hash, size, body, content_type);
        }

        let operation = sanitize_operation(operation);
        if self.packs.get(&operation).is_some_and(|pack| {
            self.policy
                .should_flush(pack.plaintext_bytes, pack.opened_at.elapsed())
        }) {
            self.seal_pack(&operation)?;
        }
        if !self.packs.contains_key(&operation) {
            let pack = self.open_pack(&operation)?;
            self.packs.insert(operation.clone(), pack);
        }

        let (stored, due) = {
            let pack = self
                .packs
                .get_mut(&operation)
                .ok_or(ContentError::InvalidState("open pack disappeared"))?;
            if let Some(existing) = pack.entries.get(&hash.blake3) {
                return Ok(existing.clone());
            }

            let hash = pack.writer.append(body)?;
            pack.plaintext_bytes = pack.plaintext_bytes.saturating_add(size);
            let stored = StoredContent::new(
                ContentStorage::Pack,
                pack.key.clone(),
                hash,
                size,
                content_type.map(str::to_owned),
            );
            pack.entries.insert(hash.blake3, stored.clone());
            pack.contents.push(stored.clone());
            let due = self
                .policy
                .should_flush(pack.plaintext_bytes, pack.opened_at.elapsed());
            (stored, due)
        };

        if due {
            self.seal_pack(&operation)?;
        }
        Ok(stored)
    }

    /// Seals every open pack and uploads all sealed packs and pending objects.
    ///
    /// Packs already sealed by the size or age policy are uploaded too. A failed
    /// upload leaves the remaining temporary files in place so a later flush can
    /// retry; already uploaded items are not uploaded again. If a later item
    /// fails, items uploaded earlier in the same call remain durable and are
    /// not reported by the failed call.
    ///
    /// Uploading through the local backend copies files synchronously on the
    /// calling task.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Pack`] when sealing a pack fails, or a
    /// [`ContentError::Store`] error when an upload fails.
    pub async fn flush(&mut self) -> Result<FlushResult, ContentError> {
        let operations: Vec<String> = self.packs.keys().cloned().collect();
        for operation in operations {
            self.seal_pack(&operation)?;
        }

        let mut packs = Vec::new();
        while let Some(sealed) = self.sealed.pop_front() {
            match self.store.put_file(&sealed.key, sealed.temp.path()).await {
                Ok(()) => packs.push(FlushedPack {
                    key: sealed.key,
                    summary: sealed.summary,
                    contents: sealed.contents,
                }),
                Err(error) => {
                    self.sealed.push_front(sealed);
                    return Err(error.into());
                }
            }
        }

        let mut objects = Vec::new();
        while let Some(pending) = self.objects.pop_front() {
            match self
                .store
                .put_file(pending.stored.key(), pending.temp.path())
                .await
            {
                Ok(()) => objects.push(pending.stored),
                Err(error) => {
                    self.objects.push_front(pending);
                    return Err(error.into());
                }
            }
        }

        Ok(FlushResult { packs, objects })
    }

    fn queue_object(
        &mut self,
        hash: ContentHash,
        size: u64,
        body: &[u8],
        content_type: Option<&str>,
    ) -> Result<StoredContent, ContentError> {
        let key = format!("objects/{}", hash.blake3_hex());
        let mut temp = NamedTempFile::new_in(&self.temp_dir)?;
        temp.write_all(body)?;
        temp.as_file_mut().flush()?;

        let stored = StoredContent::new(
            ContentStorage::Object,
            key,
            hash,
            size,
            content_type.map(str::to_owned),
        );
        self.objects.push_back(PendingObject {
            temp,
            stored: stored.clone(),
        });
        Ok(stored)
    }

    fn open_pack(&self, operation: &str) -> Result<OpenPack, ContentError> {
        let sequence = NEXT_PACK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let key = format!(
            "packs/{operation}/{}/{}-{sequence}.pack",
            current_hour(),
            std::process::id()
        );
        let temp = NamedTempFile::new_in(&self.temp_dir)?;
        let writer = PackWriter::with_compression_level(temp, self.policy.compression_level())?;
        Ok(OpenPack {
            key,
            writer,
            plaintext_bytes: 0,
            opened_at: Instant::now(),
            entries: HashMap::new(),
            contents: Vec::new(),
        })
    }

    fn seal_pack(&mut self, operation: &str) -> Result<(), ContentError> {
        let Some(mut pack) = self.packs.remove(operation) else {
            return Ok(());
        };
        if pack.contents.is_empty() {
            return Ok(());
        }

        let summary = pack.writer.finish()?;
        let temp = pack.writer.into_inner();
        let contents = pack
            .contents
            .into_iter()
            .map(|content| content.with_pack_summary(summary))
            .collect();
        self.sealed.push_back(SealedPack {
            key: pack.key,
            summary,
            temp,
            contents,
        });
        Ok(())
    }
}

/// Reduces an operation name to a safe lowercase key component.
fn sanitize_operation(operation: &str) -> String {
    let mut sanitized: String = operation
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        sanitized.push_str("unknown");
    }
    sanitized
}

/// Returns the current UTC hour as `yyyy-mm-ddTHH`.
fn current_hour() -> String {
    const FORMAT: &[time::format_description::FormatItem<'static>] =
        time::macros::format_description!("[year]-[month]-[day]T[hour]");
    time::OffsetDateTime::now_utc()
        .format(FORMAT)
        .unwrap_or_else(|_| "0000-00-00T00".to_owned())
}

#[cfg(all(test, feature = "local"))]
mod tests {
    use std::time::Duration;

    use neuradex_packfile::PackReader;

    use super::{ContentWriter, sanitize_operation};

    const TEST_MAX_AGE: Duration = Duration::from_millis(1);

    #[test]
    fn writer_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ContentWriter>();
    }
    use crate::{CasPolicy, ContentError, ContentStorage, ContentStore, LocalStore, read_content};

    fn policy(target: u64, max_age: Duration, threshold: u64) -> CasPolicy {
        CasPolicy::new(target, max_age, 1024 * 1024, threshold, 3, 30).unwrap()
    }

    #[tokio::test]
    async fn packs_bodies_and_uploads_one_readable_pack() {
        let directory = tempfile::tempdir().unwrap();
        let local = LocalStore::new(directory.path().join("store")).unwrap();
        let store = ContentStore::Local(local.clone());
        let mut writer =
            ContentWriter::new_in(store.clone(), CasPolicy::default(), directory.path());

        let bodies: [&[u8]; 3] = [b"alpha", b"bravo", b"charlie"];
        let mut references = Vec::new();
        for body in bodies {
            references.push(
                writer
                    .append("fetch", body, Some("text/plain"))
                    .expect("append succeeds"),
            );
        }
        assert!(!writer.is_empty());

        let flushed = writer.flush().await.unwrap();
        assert_eq!(flushed.packs.len(), 1);
        assert_eq!(flushed.objects, Vec::new());
        assert!(writer.is_empty());

        let pack = &flushed.packs[0];
        assert_eq!(pack.summary.entry_count, 3);
        assert_eq!(pack.contents.len(), 3);
        assert!(
            pack.contents
                .iter()
                .all(|content| content.pack_summary() == Some(pack.summary))
        );
        assert!(references.iter().all(|content| content.key() == pack.key));

        let bytes = local.get(&pack.key).unwrap();
        let reader = PackReader::new(&bytes).expect("the pack is valid");
        for (body, reference) in bodies.iter().zip(&references) {
            assert_eq!(
                reader.read(&reference.hash()).unwrap().unwrap(),
                body.to_vec()
            );
            assert_eq!(
                read_content(&store, reference).await.unwrap().unwrap(),
                body.to_vec()
            );
        }
    }

    #[tokio::test]
    async fn duplicate_bodies_in_one_pack_return_the_existing_reference() {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::Local(LocalStore::new(directory.path().join("store")).unwrap());
        let mut writer = ContentWriter::new_in(store, CasPolicy::default(), directory.path());

        let first = writer.append("fetch", b"same body", None).unwrap();
        let second = writer
            .append("fetch", b"same body", Some("text/html"))
            .unwrap();
        assert_eq!(first, second);

        let flushed = writer.flush().await.unwrap();
        assert_eq!(flushed.packs.len(), 1);
        assert_eq!(flushed.packs[0].summary.entry_count, 1);
    }

    #[tokio::test]
    async fn size_policy_seals_a_pack_and_the_next_body_starts_a_new_one() {
        let directory = tempfile::tempdir().unwrap();
        let local = LocalStore::new(directory.path().join("store")).unwrap();
        let store = ContentStore::Local(local);
        let mut writer = ContentWriter::new_in(
            store,
            policy(8, Duration::from_secs(60), 1024),
            directory.path(),
        );

        writer.append("fetch", b"123456", None).unwrap();
        writer.append("fetch", b"abcdef", None).unwrap();
        writer.append("fetch", b"ABCDEF", None).unwrap();

        let flushed = writer.flush().await.unwrap();
        assert_eq!(flushed.packs.len(), 2);
        assert_ne!(flushed.packs[0].key, flushed.packs[1].key);
        assert_eq!(flushed.packs[0].summary.entry_count, 2);
        assert_eq!(flushed.packs[1].summary.entry_count, 1);
    }

    #[tokio::test]
    async fn age_policy_seals_a_pack_before_the_next_append() {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::Local(LocalStore::new(directory.path().join("store")).unwrap());
        let mut writer =
            ContentWriter::new_in(store, policy(1024, TEST_MAX_AGE, 1024), directory.path());

        writer.append("fetch", b"first", None).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        writer.append("fetch", b"second", None).unwrap();

        let flushed = writer.flush().await.unwrap();
        assert_eq!(flushed.packs.len(), 2);
        assert_eq!(flushed.packs[0].summary.entry_count, 1);
        assert_eq!(flushed.packs[1].summary.entry_count, 1);
    }

    #[tokio::test]
    async fn a_body_at_the_threshold_is_stored_as_an_individual_object() {
        let directory = tempfile::tempdir().unwrap();
        let local = LocalStore::new(directory.path().join("store")).unwrap();
        let store = ContentStore::Local(local.clone());
        let mut writer = ContentWriter::new_in(
            store.clone(),
            policy(1024, Duration::from_secs(60), 4),
            directory.path(),
        );

        let stored = writer
            .append("fetch", b"12345", Some("application/octet-stream"))
            .unwrap();
        assert_eq!(stored.storage(), ContentStorage::Object);
        assert_eq!(stored.key(), format!("objects/{}", stored.blake3_hex()));

        let flushed = writer.flush().await.unwrap();
        assert!(flushed.packs.is_empty());
        assert_eq!(flushed.objects.len(), 1);
        assert!(local.exists(stored.key()).unwrap());
        assert_eq!(
            read_content(&store, &stored).await.unwrap().unwrap(),
            b"12345".to_vec()
        );
    }

    #[tokio::test]
    async fn empty_flush_uploads_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::Local(LocalStore::new(directory.path().join("store")).unwrap());
        let mut writer = ContentWriter::new_in(store, CasPolicy::default(), directory.path());

        let flushed = writer.flush().await.unwrap();
        assert!(flushed.is_empty());
    }

    #[tokio::test]
    async fn operations_use_separate_packs() {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::Local(LocalStore::new(directory.path().join("store")).unwrap());
        let mut writer = ContentWriter::new_in(store, CasPolicy::default(), directory.path());

        writer.append("fetch", b"one", None).unwrap();
        writer.append("peek", b"two", None).unwrap();

        let flushed = writer.flush().await.unwrap();
        assert_eq!(flushed.packs.len(), 2);
        assert!(
            flushed
                .packs
                .iter()
                .all(|pack| pack.key.starts_with("packs/"))
        );
        assert!(
            flushed
                .packs
                .iter()
                .any(|pack| pack.key.contains("/fetch/"))
        );
        assert!(flushed.packs.iter().any(|pack| pack.key.contains("/peek/")));
    }

    #[test]
    fn operation_names_cannot_escape_the_pack_prefix() {
        assert_eq!(sanitize_operation("fetch"), "fetch");
        assert_eq!(sanitize_operation("OpenWebUI/Search"), "openwebui_search");
        assert_eq!(sanitize_operation("../escape"), "___escape");
        assert_eq!(sanitize_operation(""), "unknown");
    }

    #[test]
    fn oversized_bodies_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::Local(LocalStore::new(directory.path().join("store")).unwrap());
        let small = CasPolicy::new(1024, Duration::from_secs(60), 2, 1024, 3, 1).unwrap();
        let mut writer = ContentWriter::new_in(store, small, directory.path());

        assert!(matches!(
            writer.append("fetch", b"abc", None),
            Err(ContentError::BodyTooLarge { size: 3, limit: 2 })
        ));
    }
}
