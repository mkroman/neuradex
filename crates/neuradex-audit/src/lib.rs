//! Audit event schema, header redaction, request records, and sinks.
//!
//! This crate owns *what* neuradex records. It deliberately does not depend on the HTTP
//! service, the content plane, or Kagi: an [`AuditEvent`] can describe a request without linking
//! the code that served it, a [`ContentReference`] names stored content
//! without linking the storage implementation, and the operation fields do not use Kagi types.
//!
//! The intended flow is:
//!
//! 1. Start a [`RequestRecord`] with the request id, method, and path.
//! 2. Fill in the route, response, operation-specific fields, content references, and redacted
//!    header captures.
//! 3. Finish the record into an [`AuditEvent`] and emit it through an [`AuditSink`].
//!
//! [`CompletionGuard`] combines steps 3 and the request's scope so every request emits exactly
//! one event, including aborts and panics. The sink trait is synchronous by design; the service
//! is responsible for wrapping a sink in an async queue or background writer.
//!
//! Header capture is deliberately conservative: only allow-listed headers keep their values.
//! Credentials and personal data are redacted, URL-valued headers lose their query strings, and
//! user identifiers and IP addresses are HMAC-hashed when a key is configured. Unknown headers
//! retain only their name and original byte length.

mod digest;

pub mod error;
pub mod event;
pub mod guard;
pub mod header_policy;
pub mod request_record;
pub mod sink;

pub use error::AuditError;
pub use event::{
    AuditEvent, AuthMode, ContentReference, ContentStorage, EventId, EventName, FetchOperation,
    HostCount, HttpInfo, InfrastructureKind, InfrastructureOperation, Operation, Outcome,
    RequestId, SCHEMA_VERSION, SearchOperation, ServiceInfo, Timestamp,
};
pub use guard::CompletionGuard;
pub use header_policy::{
    CapturedHeader, CapturedHeaderValue, HeaderAction, HeaderCapture, HeaderPolicy,
    HeaderValueEncoding, MAX_HEADERS, MAX_TOTAL_BYTES, MAX_VALUE_BYTES,
};
pub use request_record::RequestRecord;
pub use sink::{AuditSink, JsonlSink, MemorySink, NullSink};
