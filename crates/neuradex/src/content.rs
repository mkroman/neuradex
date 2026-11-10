//! The content plane: body appends, pack flushing, and audit references.
//!
//! The service owns one [`ContentPipeline`] when content storage is enabled. The pipeline wraps
//! the synchronous [`ContentWriter`] in a Tokio mutex: appends copy the body, acquire the lock,
//! and run the synchronous compression and temporary-file write on Tokio's blocking pool, so the
//! async worker is never blocked by a large body. The guard is from [`tokio::sync::Mutex`] and is
//! deliberately never a `std::sync::MutexGuard`, which must not be held across an `.await`.

use std::sync::Arc;
use std::time::Duration;

use neuradex_audit::{ContentReference, ContentStorage as AuditContentStorage};
use neuradex_content::{
    ContentError, ContentStorage, ContentStore, ContentWriter, FlushResult, LocalStore, S3Config,
    S3Store, StoreError, StoredContent,
};
use secrecy::ExposeSecret;

use crate::audit::RequestAuditHandle;
use crate::config::{ContentBackend, ContentConfig, S3ContentSettings};

/// Errors that can occur while building the content plane at startup.
#[derive(Debug, thiserror::Error)]
pub enum ContentBuildError {
    /// The local store directory could not be created or opened.
    #[error("could not open the local content store at {}: {source}", path.display())]
    Local {
        /// The configured store root.
        path: std::path::PathBuf,
        /// The underlying store error.
        #[source]
        source: StoreError,
    },
    /// The S3 client configuration was rejected.
    #[error("could not configure the S3 content store: {0}")]
    S3(#[source] StoreError),
}

/// Builds the content pipeline from a validated configuration.
///
/// Returns `None` when content storage is disabled.
///
/// # Errors
///
/// Returns an error when the local store cannot be opened or the S3 client configuration is
/// invalid.
pub fn build(config: &ContentConfig) -> Result<Option<Arc<ContentPipeline>>, ContentBuildError> {
    let store = match config.backend() {
        ContentBackend::Disabled => return Ok(None),
        ContentBackend::Local { path } => {
            let local = LocalStore::new(path).map_err(|source| ContentBuildError::Local {
                path: path.clone(),
                source,
            })?;
            ContentStore::Local(local)
        }
        ContentBackend::S3(settings) => {
            ContentStore::S3(S3Store::new(s3_config(settings)).map_err(ContentBuildError::S3)?)
        }
    };

    let writer = ContentWriter::new(store.clone(), config.policy().clone());

    Ok(Some(Arc::new(ContentPipeline::new(writer, store))))
}

/// Builds the S3 client configuration from the service settings.
fn s3_config(settings: &S3ContentSettings) -> S3Config {
    let mut config = S3Config::new(
        settings.endpoint(),
        settings.region(),
        settings.bucket(),
        settings.access_key_id().expose_secret(),
        settings.secret_access_key().expose_secret(),
    );

    if let Some(prefix) = settings.prefix() {
        prefix.clone_into(&mut config.prefix);
    }
    config.force_path_style = settings.force_path_style();

    config
}

/// Serializes the appends and flushes of one content writer.
pub struct ContentPipeline {
    writer: Arc<tokio::sync::Mutex<ContentWriter>>,
    store: ContentStore,
}

impl ContentPipeline {
    /// Wraps a content writer and a clone of its store.
    #[must_use]
    pub fn new(writer: ContentWriter, store: ContentStore) -> Self {
        Self {
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
            store,
        }
    }

    /// Returns the store the writer uploads to.
    #[must_use]
    pub const fn store(&self) -> &ContentStore {
        &self.store
    }

    /// Appends one body and returns its final reference.
    ///
    /// The synchronous append runs on the blocking pool so pack compression and temporary-file
    /// writes do not stall an async worker. The body is copied into the task; appends are short
    /// and serialized behind the writer lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the body exceeds the configured limit, compression fails, or a
    /// temporary file cannot be written.
    pub async fn append(
        &self,
        operation: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) -> Result<StoredContent, ContentError> {
        let operation = operation.to_owned();
        let body = body.to_vec();
        let content_type = content_type.map(str::to_owned);
        let writer = Arc::clone(&self.writer);

        tokio::task::spawn_blocking(move || {
            let mut writer = writer.blocking_lock();

            writer.append(&operation, &body, content_type.as_deref())
        })
        .await
        .map_err(|_| ContentError::InvalidState("the content append task failed"))?
    }

    /// Seals open packs and uploads every sealed pack and pending object.
    ///
    /// A failed upload stays queued in the writer for a later flush.
    ///
    /// # Errors
    ///
    /// Returns an error when sealing a pack or uploading an item fails.
    pub async fn flush(&self) -> Result<FlushResult, ContentError> {
        self.writer.lock().await.flush().await
    }

    /// Starts a background task that flushes the pipeline every `period`.
    ///
    /// The returned handle stops the task gracefully and performs one final flush on shutdown.
    #[must_use]
    pub fn spawn_flush_task(self: &Arc<Self>, period: Duration) -> FlushTask {
        let pipeline = Arc::clone(self);
        let (shutdown, mut receiver) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first interval tick is immediate; skip it so startup does not flush an empty
            // writer before the first request.
            ticker.tick().await;

            loop {
                tokio::select! {
                    _ = ticker.tick() => flush_and_log(&pipeline).await,
                    changed = receiver.changed() => {
                        if changed.is_err() || *receiver.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        FlushTask {
            shutdown,
            handle,
            pipeline: Arc::clone(self),
        }
    }
}

/// A handle to a running periodic content flush task.
pub struct FlushTask {
    shutdown: tokio::sync::watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
    pipeline: Arc<ContentPipeline>,
}

impl FlushTask {
    /// Stops the periodic task and performs one final best-effort flush.
    pub async fn shutdown(self) {
        let Self {
            shutdown,
            handle,
            pipeline,
        } = self;

        if shutdown.send(true).is_err() {
            tracing::debug!("the content flush task stopped before shutdown");
        }

        if let Err(error) = handle.await
            && !error.is_cancelled()
        {
            tracing::warn!(
                "error.type" = "content_flush_task",
                "exception.message" = %error,
                "the content flush task failed"
            );
        }

        flush_and_log(&pipeline).await;
    }
}

/// Flushes the pipeline, logging failures without failing the caller.
async fn flush_and_log(pipeline: &ContentPipeline) {
    match pipeline.flush().await {
        Ok(result) if result.is_empty() => {}
        Ok(_) => tracing::debug!("content packs flushed"),
        Err(error) => tracing::warn!(
            "error.type" = "content_flush",
            "exception.message" = %error,
            "could not flush content; sealed packs are retained for retry"
        ),
    }
}

/// Converts a stored-content reference into the audit event representation.
#[must_use]
pub(crate) fn reference(stored: &StoredContent) -> ContentReference {
    let storage = match stored.storage() {
        ContentStorage::Pack => AuditContentStorage::Pack,
        ContentStorage::Object => AuditContentStorage::Object,
    };
    let reference = ContentReference::new(
        storage,
        stored.key(),
        stored.blake3_hex(),
        stored.sha256_hex(),
        stored.size(),
    );

    match stored.content_type() {
        Some(content_type) => reference.with_content_type(content_type),
        None => reference,
    }
}

/// Appends a body and records its reference on the request audit handle.
///
/// Content storage is best-effort: failures are logged and never fail the request. The body and
/// its content type are not logged.
pub(crate) async fn capture(
    pipeline: Option<&ContentPipeline>,
    audit: Option<&RequestAuditHandle>,
    operation: &str,
    body: &[u8],
    content_type: Option<&str>,
) {
    let Some(pipeline) = pipeline else {
        return;
    };

    match pipeline.append(operation, body, content_type).await {
        Ok(stored) => {
            if let Some(audit) = audit {
                audit.add_content_reference(reference(&stored));
            }
        }
        Err(error) => tracing::warn!(
            "error.type" = "content_write",
            "content.operation" = operation,
            "content.size_bytes" = u64::try_from(body.len()).unwrap_or(u64::MAX),
            "exception.message" = %error,
            "could not store request content; continuing without it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuradex_content::CasPolicy;

    #[tokio::test]
    async fn appends_and_flushes_readable_content() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let store =
            ContentStore::Local(LocalStore::new(directory.path().join("store")).expect("a store"));
        let pipeline = ContentPipeline::new(
            ContentWriter::new(store.clone(), CasPolicy::default()),
            store.clone(),
        );

        let stored = pipeline
            .append("fetch-response", b"hello", Some("text/plain"))
            .await
            .expect("the append succeeds");
        pipeline.flush().await.expect("the flush succeeds");

        assert_eq!(stored.content_type(), Some("text/plain"));
        assert_eq!(
            neuradex_content::read_content(&store, &stored)
                .await
                .expect("the content is readable")
                .expect("the content exists"),
            b"hello"
        );
    }

    #[test]
    fn converts_stored_content_to_an_audit_reference() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let store =
            ContentStore::Local(LocalStore::new(directory.path().join("store")).expect("a store"));
        let mut writer = ContentWriter::new(store, CasPolicy::default());
        let stored = writer
            .append("search-request", b"{}", Some("application/json"))
            .expect("the append succeeds");

        let reference = reference(&stored);

        assert_eq!(reference.storage(), AuditContentStorage::Pack);
        assert_eq!(reference.key(), stored.key());
        assert_eq!(reference.blake3(), stored.blake3_hex());
        assert_eq!(reference.sha256(), stored.sha256_hex());
        assert_eq!(reference.size(), 2);
        assert_eq!(reference.content_type(), Some("application/json"));
    }
}
