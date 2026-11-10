//! The request audit middleware and its server-span replacement.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use neuradex_audit::{
    InfrastructureKind, InfrastructureOperation, Operation, RequestId, RequestRecord,
};
use tracing::Instrument;

use crate::api::v1::body_capture::BodyCaptureLimit;
use crate::audit::guard::RequestAudit;
use crate::metrics::duration_ms;
use crate::state::AppState;

/// The path of the health endpoint, exempt from request tracing and audit events: probes are
/// infrastructure traffic, and auditing each one would only add noise to the log pipeline.
const HEALTHZ_PATH: &str = "/healthz";

/// The response header carrying the request identifier.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// The maximum accepted inbound request identifier length.
const MAX_REQUEST_ID_LEN: usize = 128;

/// Wraps a request in a server span and emits one audit event for it.
///
/// The event is produced for every request — API calls, infrastructure endpoints, and the
/// `404`/`405` fallbacks — including failures, so a sink can retain an unsampled request record
/// independently of trace sampling. Health probes are skipped.
///
/// `http.route` is recorded only for matched routes. The fallbacks have no route, and the
/// conventions forbid falling back to the raw path, which would make the attribute
/// high-cardinality.
pub async fn request_audit(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == HEALTHZ_PATH {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned());
    let request_id = request_id(request.headers());
    let audit = state.audit();

    let mut record = RequestRecord::new(request_id.clone(), method.clone(), path.clone());
    if let Some(route) = &route {
        record.set_route(route);
    }
    record.set_request_headers(audit.header_policy().capture(request.headers()));
    if let Some((trace_id, span_id)) = trace_context(request.headers()) {
        record.set_trace_id(trace_id);
        record.set_span_id(span_id);
    }
    if let Some(operation) = infrastructure_operation(&path, route.as_deref()) {
        record.set_operation(operation);
    }

    let guard = RequestAudit::new(record, Arc::clone(audit.sink()));
    let handle = guard.handle();
    request.extensions_mut().insert(handle.clone());
    request
        .extensions_mut()
        .insert(BodyCaptureLimit(audit.max_body_bytes()));

    // The OpenTelemetry HTTP conventions name a server span `{method} {route}`, falling back
    // to the method when the fallback handlers have no matched route.
    let span_name = route
        .as_deref()
        .map_or_else(|| method.to_string(), |route| format!("{method} {route}"));
    let span = tracing::info_span!(
        target: "neuradex::http",
        "http.server",
        "otel.name" = %span_name,
        "otel.kind" = "server",
        "http.request.method" = %method,
        "http.route" = tracing::field::Empty,
        "url.path" = %path,
        "http.response.status_code" = tracing::field::Empty,
        "error.type" = tracing::field::Empty,
        "otel.status_code" = tracing::field::Empty,
    );
    if let Some(route) = &route {
        span.record("http.route", route.as_str());
    }

    let started = Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;
    let status = response.status();
    let error_type =
        (status.is_client_error() || status.is_server_error()).then(|| status.as_u16().to_string());

    span.record("http.response.status_code", status.as_u16());
    if let Some(error_type) = &error_type {
        span.record("error.type", error_type.as_str());
    }
    // Server-span status follows the HTTP conventions: 5xx is an error, 4xx is a client
    // outcome and leaves the status unset. The audit event still records both.
    if status.is_server_error() {
        span.record("otel.status_code", "ERROR");
    }

    response.headers_mut().insert(
        HeaderName::from_static(REQUEST_ID_HEADER),
        HeaderValue::from_str(request_id.as_str()).expect("the request id is visible ASCII"),
    );

    span.in_scope(|| {
        tracing::event!(
            name: "neuradex.http.request",
            tracing::Level::INFO,
            "http.request.method" = %method,
            "http.route" = route.as_deref(),
            "url.path" = %path,
            "http.response.status_code" = status.as_u16(),
            "neuradex.http.request.duration_ms" = duration_ms(started.elapsed()),
            "error.type" = error_type.as_deref(),
            "neuradex.http.request.id" = request_id.as_str(),
        );
    });

    let response_headers = audit.header_policy().capture(response.headers());

    // A `405` can still carry a matched route because axum selected the path before the method
    // router rejected it. The audit crate classifies both router fallbacks as infrastructure.
    if matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ) {
        handle.update(|record| {
            if record.operation().is_none() {
                record.set_operation(Operation::Infrastructure(InfrastructureOperation::new(
                    InfrastructureKind::Fallback,
                )));
            }
        });
    }

    if let Err(error) = guard.complete(status, started.elapsed(), response_headers) {
        tracing::warn!(
            "error.type" = "audit_sink",
            "exception.message" = %error,
            "could not emit the request audit event"
        );
    }

    response
}

/// Returns the inbound request identifier when it is usable, or generates a new one.
///
/// The header is untrusted input: only non-empty ASCII values of at most
/// [`MAX_REQUEST_ID_LEN`] bytes are preserved. Anything else is replaced so a client cannot
/// inject control characters or grow the event without bound.
fn request_id(headers: &HeaderMap) -> RequestId {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_REQUEST_ID_LEN
                && value.is_ascii()
                && !value.bytes().any(|byte| byte.is_ascii_control())
        })
        .map_or_else(RequestId::generate, RequestId::from)
}

/// Parses a W3C `traceparent` value into its trace and span identifiers.
///
/// Only version `00` with a fixed four-field layout is accepted; all-zero identifiers are
/// invalid per the specification. The value is recorded for correlation only and is never used
/// for authentication.
fn trace_context(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get("traceparent")?.to_str().ok()?.trim();
    let mut fields = value.split('-');

    let version = fields.next()?;
    let trace_id = fields.next()?;
    let span_id = fields.next()?;
    let flags = fields.next()?;

    if fields.next().is_some()
        || version != "00"
        || !is_hex_id(trace_id, 32)
        || !is_hex_id(span_id, 16)
        || !is_hex_id(flags, 2)
        || is_zero_id(trace_id)
        || is_zero_id(span_id)
    {
        return None;
    }

    Some((trace_id.to_ascii_lowercase(), span_id.to_ascii_lowercase()))
}

/// Whether `value` is a lowercase-capable hexadecimal identifier of exactly `len` characters.
fn is_hex_id(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether an identifier consists only of zeroes.
fn is_zero_id(value: &str) -> bool {
    value.bytes().all(|byte| byte == b'0')
}

/// Returns the infrastructure operation for a path, or `None` when a handler owns the detail.
fn infrastructure_operation(path: &str, route: Option<&str>) -> Option<Operation> {
    let kind = match path {
        "/openapi.json" => InfrastructureKind::OpenApi,
        "/docs" | "/swagger-ui" | "/swagger-ui/" => InfrastructureKind::Docs,
        _ if route.is_none() => InfrastructureKind::Fallback,
        _ => return None,
    };

    Some(Operation::Infrastructure(InfrastructureOperation::new(
        kind,
    )))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::middleware;
    use axum::routing::get;
    use neuradex_audit::{
        AuditSink, HeaderAction, HeaderPolicy, InfrastructureKind, MemorySink, Outcome,
    };
    use secrecy::SecretString;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::audit::AuditConfig;

    fn test_state(sink: &Arc<MemorySink>, policy: HeaderPolicy) -> AppState {
        let sink: Arc<dyn AuditSink> = sink.clone();

        AppState::try_new_with_audit(
            "test-agent",
            SecretString::from("test-token"),
            Duration::from_secs(30),
            2,
            false,
            None,
            AuditConfig::new(sink, policy, crate::config::DEFAULT_CONTENT_MAX_BODY_BYTES),
        )
        .expect("valid state")
    }

    fn service_router(state: AppState) -> Router {
        crate::api::v1::router(state)
    }

    async fn send(router: Router, request: Request<Body>) -> Response {
        router
            .oneshot(request)
            .await
            .expect("the router is infallible")
    }

    fn request(path: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .body(Body::empty())
            .expect("valid request")
    }

    #[tokio::test]
    async fn emits_one_event_with_the_response_request_id() {
        let sink = Arc::new(MemorySink::new());
        let response = send(
            service_router(test_state(&sink, HeaderPolicy::new())),
            request("/openapi.json"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let request_id = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .expect("the response carries a request id")
            .to_owned();
        assert!(request_id.starts_with("req-"), "{request_id}");

        let events = sink.events().expect("the sink is not poisoned");
        assert_eq!(events.len(), 1);
        let event = &events[0];

        assert_eq!(event.request_id().as_str(), request_id);
        assert_eq!(event.http().method(), "GET");
        assert_eq!(event.http().path(), "/openapi.json");
        assert_eq!(event.http().route(), Some("/openapi.json"));
        assert_eq!(event.http().status(), Some(200));
        assert_eq!(event.outcome(), Outcome::Success);
        assert!(matches!(
            event.operation(),
            Some(Operation::Infrastructure(operation))
                if operation.kind() == InfrastructureKind::OpenApi
        ));
    }

    #[tokio::test]
    async fn preserves_a_valid_inbound_request_id() {
        let sink = Arc::new(MemorySink::new());
        let router = service_router(test_state(&sink, HeaderPolicy::new()));
        let mut inbound = request("/openapi.json");
        inbound.headers_mut().insert(
            REQUEST_ID_HEADER,
            HeaderValue::from_static("client-request-1"),
        );

        let response = send(router, inbound).await;

        assert_eq!(
            response
                .headers()
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("client-request-1")
        );
        let events = sink.events().expect("the sink is not poisoned");
        assert_eq!(events[0].request_id().as_str(), "client-request-1");
    }

    #[tokio::test]
    async fn replaces_an_oversized_inbound_request_id() {
        let sink = Arc::new(MemorySink::new());
        let router = service_router(test_state(&sink, HeaderPolicy::new()));
        let mut inbound = request("/openapi.json");
        let oversized = "x".repeat(MAX_REQUEST_ID_LEN + 1);
        inbound.headers_mut().insert(
            REQUEST_ID_HEADER,
            HeaderValue::from_str(&oversized).expect("valid header"),
        );

        let response = send(router, inbound).await;
        let request_id = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .expect("the response carries a request id");

        assert!(request_id.starts_with("req-"), "{request_id}");
        let events = sink.events().expect("the sink is not poisoned");
        assert_eq!(events[0].request_id().as_str(), request_id);
    }

    #[tokio::test]
    async fn skips_health_probes() {
        let sink = Arc::new(MemorySink::new());
        let response = send(
            service_router(test_state(&sink, HeaderPolicy::new())),
            request(HEALTHZ_PATH),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.headers().get(REQUEST_ID_HEADER).is_none());
        assert_eq!(sink.events().expect("the sink is not poisoned").len(), 0);
    }

    #[tokio::test]
    async fn audits_not_found_and_method_not_allowed_fallbacks() {
        let sink = Arc::new(MemorySink::new());
        let router = service_router(test_state(&sink, HeaderPolicy::new()));

        let not_found = send(router.clone(), request("/nope")).await;
        let method_not_allowed = send(router, request("/v1/search")).await;

        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        assert_eq!(method_not_allowed.status(), StatusCode::METHOD_NOT_ALLOWED);

        let events = sink.events().expect("the sink is not poisoned");
        assert_eq!(events.len(), 2);

        assert_eq!(events[0].http().path(), "/nope");
        assert_eq!(events[0].http().status(), Some(404));
        assert_eq!(events[0].outcome(), Outcome::ClientError);
        assert_eq!(events[0].error_type(), Some("404"));
        assert!(matches!(
            events[0].operation(),
            Some(Operation::Infrastructure(operation))
                if operation.kind() == InfrastructureKind::Fallback
        ));

        assert_eq!(events[1].http().path(), "/v1/search");
        assert_eq!(events[1].http().status(), Some(405));
        assert_eq!(events[1].outcome(), Outcome::ClientError);
        assert!(matches!(
            events[1].operation(),
            Some(Operation::Infrastructure(operation))
                if operation.kind() == InfrastructureKind::Fallback
        ));
    }

    #[tokio::test]
    async fn captures_a_valid_traceparent_and_ignores_an_invalid_one() {
        let sink = Arc::new(MemorySink::new());
        let router = service_router(test_state(&sink, HeaderPolicy::new()));

        let mut traced = request("/openapi.json");
        traced.headers_mut().insert(
            "traceparent",
            HeaderValue::from_static("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        );
        let mut invalid = request("/openapi.json");
        invalid.headers_mut().insert(
            "traceparent",
            HeaderValue::from_static("00-00000000000000000000000000000000-0000000000000000-01"),
        );

        send(router.clone(), traced).await;
        send(router, invalid).await;

        let events = sink.events().expect("the sink is not poisoned");
        assert_eq!(
            events[0].trace_id(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        assert_eq!(events[0].span_id(), Some("00f067aa0ba902b7"));
        assert_eq!(events[1].trace_id(), None);
        assert_eq!(events[1].span_id(), None);
    }

    #[tokio::test]
    async fn a_restrictive_tracing_filter_does_not_suppress_audit_events() {
        let sink = Arc::new(MemorySink::new());
        let router = service_router(test_state(&sink, HeaderPolicy::new()));

        // The audit sink is deliberately independent of the tracing subscriber: a filter that
        // drops the INFO request event must not drop the JSONL audit event. `LOG_FORMAT=text`
        // selects a formatter, not the audit path, so it cannot affect this either.
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::ERROR)
                .finish(),
        );
        let response = send(router, request("/openapi.json")).await;
        drop(guard);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(sink.events().expect("the sink is not poisoned").len(), 1);
    }

    async fn leaky_header_handler() -> impl axum::response::IntoResponse {
        ([(header::SET_COOKIE, "session=secret")], "ok")
    }

    #[tokio::test]
    async fn redacts_sensitive_request_and_response_headers() {
        let sink = Arc::new(MemorySink::new());
        let state = Arc::new(test_state(
            &sink,
            HeaderPolicy::new().with_hmac_key(b"test-key".to_vec()),
        ));
        let router = Router::new()
            .route("/leaky", get(leaky_header_handler))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                request_audit,
            ))
            .with_state(state);

        let mut inbound = request("/leaky");
        inbound.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer super-secret"),
        );
        inbound
            .headers_mut()
            .insert("x-openwebui-user-id", HeaderValue::from_static("user-123"));
        inbound
            .headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));

        assert_eq!(send(router, inbound).await.status(), StatusCode::OK);

        let events = sink.events().expect("the sink is not poisoned");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        let encoded = serde_json::to_string(event).expect("the event serializes");

        let request_headers = event.request_headers().expect("request headers");
        let authorization = request_headers
            .get("authorization")
            .expect("the authorization header is named");
        assert!(authorization.redacted());
        assert_eq!(authorization.action(), HeaderAction::Redact);

        let user_id = request_headers
            .get("x-openwebui-user-id")
            .and_then(|header| header.first_value())
            .expect("the user id is hashed");
        assert_eq!(user_id.len(), 64);
        assert_ne!(user_id, "user-123");

        let response_headers = event.response_headers().expect("response headers");
        assert!(
            response_headers
                .get("set-cookie")
                .is_some_and(neuradex_audit::CapturedHeader::redacted)
        );

        for secret in ["super-secret", "session=secret", "203.0.113.7"] {
            assert!(!encoded.contains(secret), "{encoded}");
        }
        let parsed: Value = serde_json::from_str(&encoded).expect("the event is JSON");
        assert_eq!(parsed["event"], "neuradex.request");
    }

    async fn panicking_handler() {
        panic!("intentional test panic");
    }

    #[tokio::test]
    async fn a_panicking_handler_emits_one_panic_event() {
        let sink = Arc::new(MemorySink::new());
        let state = Arc::new(test_state(&sink, HeaderPolicy::new()));
        let router = Router::new()
            .route("/panic", get(panicking_handler))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                request_audit,
            ))
            .with_state(state);

        let error = tokio::spawn(router.oneshot(request("/panic")))
            .await
            .expect_err("the handler panics");

        assert!(error.is_panic());
        let events = sink.events().expect("the sink is not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome(), Outcome::Panic);
    }

    async fn wait_for_abort(axum::Extension(barrier): axum::Extension<Arc<tokio::sync::Barrier>>) {
        barrier.wait().await;
        std::future::pending::<()>().await;
    }

    #[tokio::test]
    async fn a_cancelled_request_emits_one_aborted_event() {
        let sink = Arc::new(MemorySink::new());
        let state = Arc::new(test_state(&sink, HeaderPolicy::new()));
        let router = Router::new()
            .route("/wait", get(wait_for_abort))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                request_audit,
            ))
            .with_state(state);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut inbound = request("/wait");
        inbound.extensions_mut().insert(Arc::clone(&barrier));

        let task = tokio::spawn(router.oneshot(inbound));
        barrier.wait().await;
        task.abort();
        let _ = task.await;

        let events = sink.events().expect("the sink is not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome(), Outcome::Aborted);
        assert_eq!(events[0].http().path(), "/wait");
    }
}
