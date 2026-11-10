//! Error types for the content plane.
//!
//! The types are split by layer so callers can distinguish a policy
//! configuration mistake from a storage failure or a corrupt pack. All of them
//! are re-exported from the crate root.

/// An invalid [`CasPolicy`](crate::CasPolicy) configuration.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyError {
    /// The target pack size was zero.
    #[error("pack target must be greater than zero")]
    ZeroPackTarget,
    /// The maximum pack age was zero.
    #[error("max pack age must be greater than zero")]
    ZeroMaxPackAge,
    /// The maximum captured body size was zero.
    #[error("max captured body must be greater than zero")]
    ZeroMaxCapturedBody,
    /// The large-body threshold was zero.
    #[error("large-body threshold must be greater than zero")]
    ZeroLargeBodyThreshold,
    /// The zstd compression level is outside the range the pack writer accepts.
    #[error("compression level {level} is not accepted by zstd")]
    InvalidCompressionLevel {
        /// The rejected level.
        level: i32,
    },
}

/// A failure from a content store backend.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The key was empty, absolute, contained a parent component, or otherwise
    /// could not be mapped safely into the store.
    #[error("invalid storage key: {key}")]
    InvalidKey {
        /// The rejected key.
        key: String,
    },
    /// The requested object does not exist.
    #[error("object not found: {key}")]
    NotFound {
        /// The missing key.
        key: String,
    },
    /// The store configuration was rejected.
    #[error("invalid store configuration: {message}")]
    InvalidConfig {
        /// A human-readable explanation.
        message: String,
    },
    /// A requested range is larger than the backend's per-read cap.
    #[error("range {start}..{end} exceeds the {limit}-byte read limit")]
    RangeTooLarge {
        /// Inclusive start offset.
        start: u64,
        /// Exclusive end offset.
        end: u64,
        /// Maximum number of bytes the backend will return in one call.
        limit: u64,
    },
    /// A file is too large for the configured multipart upload limits.
    #[error("object is too large for multipart upload: {size} bytes (limit {limit})")]
    MultipartTooLarge {
        /// Size of the file.
        size: u64,
        /// Maximum supported size.
        limit: u64,
    },
    /// A successful S3 response was missing a field required by the caller.
    #[error("S3 response did not include {field}")]
    MissingResponseField {
        /// Name of the missing field.
        field: &'static str,
    },
    /// A backend returned more bytes than the requested range.
    #[error("response for {key} exceeded the {limit}-byte range limit")]
    OversizedResponse {
        /// Key that was read.
        key: String,
        /// Maximum number of bytes that should have been returned.
        limit: u64,
    },
    /// Local filesystem access failed.
    #[error("i/o error")]
    Io(#[from] std::io::Error),
    /// An S3 request failed. The source carries the SDK error chain.
    #[cfg(feature = "s3")]
    #[error("S3 request failed")]
    S3(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// A failure while buffering, sealing, storing, or retrieving content.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ContentError {
    /// The content policy was invalid.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// A storage backend failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The packfile codec rejected or failed to produce a pack.
    #[error(transparent)]
    Pack(#[from] neuradex_packfile::PackError),
    /// A local temporary file operation failed.
    #[error("i/o error")]
    Io(#[from] std::io::Error),
    /// A request index entry could not be serialized.
    #[error("failed to encode a request index entry")]
    Json(#[from] serde_json::Error),
    /// A body exceeded the configured capture limit.
    #[error("body is {size} bytes, exceeding the {limit}-byte capture limit")]
    BodyTooLarge {
        /// Size of the rejected body.
        size: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// A decoded object did not match the hash recorded in its reference.
    #[error("object content hash mismatch at {key}")]
    ObjectHashMismatch {
        /// Store key of the object.
        key: String,
    },
    /// A pack footer exceeded the range-retrieval safety cap.
    #[error("pack footer is {len} bytes, exceeding the {limit}-byte retrieval limit")]
    FooterTooLarge {
        /// Encoded footer length from the trailer.
        len: u64,
        /// Maximum accepted length.
        limit: u64,
    },
    /// A compressed pack frame exceeded the range-retrieval safety cap.
    #[error("pack frame is {len} bytes, exceeding the {limit}-byte retrieval limit")]
    FrameTooLarge {
        /// Encoded compressed frame length from the footer.
        len: u64,
        /// Maximum accepted length.
        limit: u64,
    },
    /// The content writer reached a state that should be impossible through the
    /// public API.
    #[error("internal content writer state is invalid: {0}")]
    InvalidState(&'static str),
}
