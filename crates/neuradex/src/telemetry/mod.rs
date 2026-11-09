//! OpenTelemetry traces, logs, resource metadata, and outbound HTTP spans.
//!
//! The service always creates OpenTelemetry spans and log records, so trace and span IDs are
//! available for correlation. OTLP export is enabled by the standard `OTEL_*` environment
//! variables and is off when no endpoint is configured, which keeps local runs quiet.
//!
//! Resource attributes come from the standard `OTEL_SERVICE_NAME` and
//! `OTEL_RESOURCE_ATTRIBUTES` variables, plus the runtime resource detectors. `service.name`
//! defaults to `neuradex` and `service.version` to this binary's package version when the
//! environment does not set them.
//!
//! The outbound HTTP spans use the stable HTTP client semantic conventions, so a
//! `tracing-opentelemetry` layer can export them without translation. The plain-text
//! subscriber prints the same fields, and the `otel.*` fields keep the span kind and status
//! meaningful in either sink.
//!
//! `url.full` is sanitized before it reaches telemetry: userinfo, fragments, and query values
//! are redacted, while query keys remain to identify the request shape.
//!
//! The HTTP fields are declared only in `http_client_span`, `record_response`, and
//! `record_error`. Keep it that way: `tracing` permits duplicate field names on one span,
//! while OpenTelemetry attribute keys must be unique, so callers must not redeclare these
//! fields on the same span.

mod init;
pub(crate) mod spans;

pub use init::{Telemetry, init};
pub(crate) use spans::{http_client_span, record_error, record_response};
