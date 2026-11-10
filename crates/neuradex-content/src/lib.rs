//! Content plane for neuradex: pack orchestration, object storage, the hourly
//! request index, and range-based retrieval.
//!
//! This crate owns *where* content lives. It does not depend on the HTTP
//! service, the audit event schema, or Kagi.
//!
//! # Layout
//!
//! Captured bodies are appended to a [`ContentWriter`]. Bodies are grouped by
//! operation into packfiles; a body at or above the policy's large-body
//! threshold is written as an individual object instead. [`ContentWriter::append`]
//! is synchronous and writes temporary files, while [`ContentWriter::flush`] is
//! asynchronous and uploads sealed packs and objects to a [`ContentStore`].
//!
//! [`RequestIndexWriter`] buffers one newline-delimited JSON row per stored
//! body. It intentionally emits plain NDJSON so a caller can compress and
//! upload it through any object pipeline.
//!
//! # Retrieval
//!
//! [`read_pack_content`] reads one frame using range requests: the trailer, the
//! footer, then the frame header and compressed payload. A 128 MiB pack is
//! never downloaded just to read one body. [`read_content`] dispatches on the
//! storage class and verifies object hashes as well.
//!
//! # Features
//!
//! - `local` (default): `LocalStore`, a directory-backed store with atomic
//!   writes and path-traversal protection.
//! - `s3`: `S3Store`, an official `aws-sdk-s3` client for AWS S3 and
//!   S3-compatible services such as Backblaze B2.
//!
//! Both backends can be compiled in at once. [`ContentStore`] selects one at
//! runtime.

#![forbid(unsafe_code)]

#[cfg(not(any(feature = "local", feature = "s3")))]
compile_error!("neuradex-content requires at least one of the `local` or `s3` features");

mod error;
mod policy;
mod request_index;
mod retrieval;
pub mod store;
mod stored_content;
mod writer;

pub use error::{ContentError, PolicyError, StoreError};
pub use policy::{
    CasPolicy, DEFAULT_COMPRESSION_LEVEL, DEFAULT_LARGE_BODY_THRESHOLD_BYTES,
    DEFAULT_MAX_CAPTURED_BODY_BYTES, DEFAULT_MAX_PACK_AGE, DEFAULT_PACK_TARGET_BYTES,
    DEFAULT_RETENTION_DAYS,
};
pub use request_index::{RequestIndexEntry, RequestIndexWriter};
pub use retrieval::{read_content, read_pack_content};
pub use store::ContentStore;
#[cfg(feature = "local")]
pub use store::LocalStore;
#[cfg(feature = "s3")]
pub use store::{S3Config, S3Store};
pub use stored_content::{ContentStorage, StoredContent};
pub use writer::{ContentWriter, FlushResult, FlushedPack};
