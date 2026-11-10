//! Storage backends for content packs and individual objects.
//!
//! `LocalStore` writes to a directory and is enabled by the default `local`
//! feature. `S3Store` talks to any S3-compatible endpoint and is enabled by the
//! `s3` feature. Both can be compiled in at once; [`ContentStore`] selects one
//! at runtime.
//!
//! Keys are relative, slash-separated ASCII paths such as
//! `packs/fetch/2026-11-10T00/1234-7.pack` or `objects/<blake3>`. They are
//! validated before they reach a backend, so neither backend needs to trust a
//! caller-built key.

#[cfg(feature = "local")]
mod local;
#[cfg(feature = "s3")]
mod s3;

#[cfg(feature = "local")]
pub use local::LocalStore;
#[cfg(feature = "s3")]
pub use s3::{S3Config, S3Store};

use std::ops::Range;
use std::path::Path;

use crate::StoreError;

/// The runtime-selected content store.
///
/// The async methods of the local variant perform blocking filesystem work on
/// the calling task. Small reads and writes complete quickly; a caller that
/// moves large packs or reads on a latency-sensitive tokio task should wrap the
/// call in `tokio::task::spawn_blocking` at the service layer. The S3 backend
/// also reads local source files with `std::fs` during `put_file`.
#[derive(Clone, Debug)]
pub enum ContentStore {
    /// A local filesystem store.
    #[cfg(feature = "local")]
    Local(LocalStore),
    /// An S3-compatible object store.
    #[cfg(feature = "s3")]
    S3(S3Store),
}

#[cfg_attr(
    not(feature = "s3"),
    allow(clippy::unused_async, clippy::unused_async_trait_impl)
)]
impl ContentStore {
    /// Writes `bytes` to `key`, replacing any existing object.
    ///
    /// # Errors
    ///
    /// Returns an invalid-key error for a key that escapes the store root, or a
    /// backend-specific error when the write fails.
    pub async fn put_bytes(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        match self {
            #[cfg(feature = "local")]
            Self::Local(store) => store.put_bytes(key, bytes),
            #[cfg(feature = "s3")]
            Self::S3(store) => store.put_bytes(key, bytes).await,
        }
    }

    /// Copies the file at `path` to `key`.
    ///
    /// The local backend copies synchronously on the calling task. The S3
    /// backend uses one `PUT` for files up to 16 MiB and a multipart upload
    /// with parts capped at 16 MiB for larger files.
    ///
    /// # Errors
    ///
    /// Returns an invalid-key error, an I/O error, or a backend-specific error
    /// when reading the file or uploading it fails.
    pub async fn put_file(&self, key: &str, path: &Path) -> Result<(), StoreError> {
        match self {
            #[cfg(feature = "local")]
            Self::Local(store) => store.put_file(key, path),
            #[cfg(feature = "s3")]
            Self::S3(store) => store.put_file(key, path).await,
        }
    }

    /// Reads `range` (an exclusive-end byte range) from `key`.
    ///
    /// The local backend clamps ranges to the file length and returns an empty
    /// vector for an empty or out-of-range start. The S3 backend forwards the
    /// range and may return a backend error when it starts past the end.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the object does not exist, an
    /// invalid-key error, or a backend-specific error when the read fails.
    pub async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, StoreError> {
        match self {
            #[cfg(feature = "local")]
            Self::Local(store) => store.get_range(key, range),
            #[cfg(feature = "s3")]
            Self::S3(store) => store.get_range(key, range).await,
        }
    }

    /// Reads the complete object at `key`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the object does not exist, an
    /// invalid-key error, or a backend-specific error when the read fails.
    pub async fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let size = self.object_size(key).await?;
        self.get_range(key, 0..size).await
    }

    /// Returns the length of the object at `key` in bytes.
    ///
    /// This is a metadata operation locally and a single `HEAD` request for
    /// S3. It is intended for pack and index keys, not per-body lookups.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the object does not exist, an
    /// invalid-key error, or a backend-specific error when the metadata request
    /// fails.
    pub async fn object_size(&self, key: &str) -> Result<u64, StoreError> {
        match self {
            #[cfg(feature = "local")]
            Self::Local(store) => store.object_size(key),
            #[cfg(feature = "s3")]
            Self::S3(store) => store.object_size(key).await,
        }
    }

    /// Returns whether `key` exists.
    ///
    /// # Errors
    ///
    /// Returns an invalid-key error, or a backend-specific error other than
    /// "not found" when the check fails.
    pub async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        match self {
            #[cfg(feature = "local")]
            Self::Local(store) => store.exists(key),
            #[cfg(feature = "s3")]
            Self::S3(store) => store.exists(key).await,
        }
    }
}

#[cfg(feature = "local")]
impl From<LocalStore> for ContentStore {
    fn from(store: LocalStore) -> Self {
        Self::Local(store)
    }
}

#[cfg(feature = "s3")]
impl From<S3Store> for ContentStore {
    fn from(store: S3Store) -> Self {
        Self::S3(store)
    }
}

/// Rejects keys that could escape a filesystem root or confuse a store.
///
/// Only non-empty ASCII alphanumeric, `-`, `_`, `.`, and `/` components are
/// accepted, and no component may be `.` or `..`.
pub(crate) fn validate_key(key: &str) -> Result<(), StoreError> {
    let invalid = || StoreError::InvalidKey {
        key: key.to_owned(),
    };

    if key.is_empty() || key.len() > 1024 {
        return Err(invalid());
    }
    for component in key.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(invalid());
        }
        if !component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Joins a validated prefix and key.
#[cfg(feature = "s3")]
pub(crate) fn with_prefix(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}/{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::validate_key;

    #[test]
    fn valid_keys_are_accepted() {
        for key in [
            "objects/abc",
            "packs/fetch/2026-11-10T00/1234-7.pack",
            "index/fetch/2026-11-10T00.ndjson",
        ] {
            assert!(validate_key(key).is_ok(), "{key}");
        }
    }

    #[test]
    fn traversal_and_odd_keys_are_rejected() {
        for key in [
            "",
            "/absolute",
            "../escape",
            "a/../../b",
            "a//b",
            "a/./b",
            "a/b/",
            "a\\b",
            "a b",
            "a?b",
        ] {
            assert!(validate_key(key).is_err(), "{key}");
        }
    }

    #[cfg(feature = "s3")]
    #[test]
    fn prefixes_join_with_one_separator() {
        use super::with_prefix;

        assert_eq!(with_prefix("", "objects/a"), "objects/a");
        assert_eq!(with_prefix("tenant", "objects/a"), "tenant/objects/a");
    }
}
