//! OpenTelemetry-compatible spans for outbound HTTP requests.
//!
//! The spans use the stable HTTP client semantic conventions, so a `tracing-opentelemetry`
//! layer can export them without translation. The plain-text subscriber prints the same
//! fields, and the `otel.*` fields keep the span kind and status meaningful in either sink.
//!
//! `url.full` is sanitized before it reaches telemetry: userinfo, fragments, and query values
//! are redacted, while query keys remain to identify the request shape.
//!
//! The HTTP fields are declared only in [`http_client_span`], [`record_response`], and
//! [`record_error`]. Keep it that way: `tracing` permits duplicate field names on one span,
//! while OpenTelemetry attribute keys must be unique, so callers must not redeclare these
//! fields on the same span.

use tracing::{Span, field, info_span};
use url::Url;

/// Builds an HTTP client span for one request attempt.
///
/// The span name sent to OpenTelemetry exporters is just the method: a fetched page has no
/// low-cardinality target, and the HTTP conventions forbid using its path as the span name.
/// The sanitized URL is still recorded in `url.full`.
#[expect(
    clippy::redundant_pub_crate,
    reason = "the telemetry module is private, so pub(crate) states the effective reachability"
)]
pub(crate) fn http_client_span(method: &'static str, url: &Url, resend_count: u32) -> Span {
    // Outbound requests are part of the service's normal operation, so they are visible
    // at the default INFO level. The Kagi library keeps its per-request spans at DEBUG.
    let span = info_span!(
        target: "neuradex::http",
        "http.client",
        "otel.name" = %method,
        "otel.kind" = "client",
        "http.request.method" = method,
        "url.full" = %sanitized_url(url),
        "url.scheme" = url.scheme(),
        "server.address" = field::Empty,
        "server.port" = field::Empty,
        "network.protocol.name" = "http",
        "http.request.resend_count" = field::Empty,
        "http.response.status_code" = field::Empty,
        "network.protocol.version" = field::Empty,
        "error.type" = field::Empty,
        "otel.status_code" = field::Empty,
    );

    if let Some(host) = url.host_str() {
        span.record("server.address", host);
    }
    if let Some(port) = url.port_or_known_default() {
        span.record("server.port", port);
    }

    if resend_count > 0 {
        span.record("http.request.resend_count", resend_count);
    }

    span
}

/// Records the response facts carried by an HTTP client span.
///
/// A 4xx or 5xx status marks the span as failed for a client, as the stable HTTP semantic
/// conventions require; 1xx–3xx leave the status unset.
#[expect(
    clippy::redundant_pub_crate,
    reason = "the telemetry module is private, so pub(crate) states the effective reachability"
)]
pub(crate) fn record_response(span: &Span, response: &wreq::Response) {
    let status = response.status();
    span.record("http.response.status_code", status.as_u16());

    if let Some(version) = protocol_version(response.version()) {
        span.record("network.protocol.version", version);
    }

    if status.is_client_error() || status.is_server_error() {
        span.record("error.type", field::display(status.as_u16()));
        span.record("otel.status_code", "ERROR");
    }
}

/// Records a failed request on its HTTP client span.
#[expect(
    clippy::redundant_pub_crate,
    reason = "the telemetry module is private, so pub(crate) states the effective reachability"
)]
pub(crate) fn record_error(span: &Span, error: &wreq::Error) {
    if let Some(status) = error.status() {
        span.record("http.response.status_code", status.as_u16());
        span.record("error.type", field::display(status.as_u16()));
    } else {
        span.record("error.type", error_type(error));
    }

    span.record("otel.status_code", "ERROR");
}

/// Maps a transport failure to a low-cardinality `error.type` value.
fn error_type(error: &wreq::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection_error"
    } else if error.is_dns() {
        "dns_error"
    } else if error.is_tls() {
        "tls_error"
    } else if error.is_connection_reset() {
        "connection_reset"
    } else if error.is_body() {
        "body_error"
    } else if error.is_decode() {
        "decode_error"
    } else if error.is_redirect() {
        "redirect_error"
    } else if error.is_request() {
        "request_error"
    } else {
        "other"
    }
}

/// Maps an HTTP version to the `network.protocol.version` value, or `None` when it is unknown.
const fn protocol_version(version: wreq::Version) -> Option<&'static str> {
    match version {
        wreq::Version::HTTP_09 => Some("0.9"),
        wreq::Version::HTTP_10 => Some("1.0"),
        wreq::Version::HTTP_11 => Some("1.1"),
        wreq::Version::HTTP_2 => Some("2"),
        wreq::Version::HTTP_3 => Some("3"),
        _ => None,
    }
}

/// Renders `url` for a span with its userinfo, fragment, and query values redacted.
///
/// Query parameter names are kept because they identify the request shape, but their values
/// never reach telemetry.
fn sanitized_url(url: &Url) -> String {
    let mut sanitized = url.clone();
    sanitized.set_fragment(None);
    let _ = sanitized.set_username("");
    let _ = sanitized.set_password(None);

    if sanitized.query().is_some() {
        let keys: Vec<String> = sanitized
            .query_pairs()
            .map(|(key, _)| key.into_owned())
            .collect();
        let mut query = sanitized.query_pairs_mut();
        query.clear();
        for key in keys {
            query.append_pair(&key, "REDACTED");
        }
    }

    sanitized.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_userinfo_fragments_and_query_values() {
        let url =
            Url::parse("https://user:password@example.com:8443/path?token=secret&q=hello#fragment")
                .expect("the test URL parses");

        let sanitized = sanitized_url(&url);

        assert_eq!(
            sanitized,
            "https://example.com:8443/path?token=REDACTED&q=REDACTED"
        );
        for secret in ["user", "password", "secret", "hello", "fragment"] {
            assert!(!sanitized.contains(secret), "{sanitized}");
        }
    }

    #[test]
    fn maps_known_protocol_versions() {
        assert_eq!(protocol_version(wreq::Version::HTTP_11), Some("1.1"));
        assert_eq!(protocol_version(wreq::Version::HTTP_2), Some("2"));
        assert_eq!(protocol_version(wreq::Version::HTTP_3), Some("3"));
    }

    #[test]
    fn http_client_span_declares_unique_semantic_fields() {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let url = Url::parse("https://example.com/path").expect("the test URL parses");
            let span = http_client_span("GET", &url, 0);
            let metadata = span.metadata().expect("the span is enabled");

            let mut names: Vec<&str> = metadata.fields().iter().map(|field| field.name()).collect();
            names.sort_unstable();

            let mut unique = names.clone();
            unique.dedup();
            assert_eq!(names, unique, "duplicate span field names: {names:?}");

            for required in [
                "otel.name",
                "otel.kind",
                "http.request.method",
                "url.full",
                "url.scheme",
                "server.address",
                "server.port",
                "network.protocol.name",
                "http.request.resend_count",
                "http.response.status_code",
                "network.protocol.version",
                "error.type",
                "otel.status_code",
            ] {
                assert!(names.contains(&required), "missing field {required}");
            }
        });
    }
}
