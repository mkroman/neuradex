//! API v1: routes and handlers.

pub(crate) mod error;
pub(crate) mod extract;
pub(crate) mod fetch;
pub(crate) mod openapi;
pub(crate) mod openwebui_search;
pub(crate) mod peek;
pub(crate) mod redirect;
pub(crate) mod search;
pub(crate) mod stream;

use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::MatchedPath;
use axum::extract::Request;
use axum::http::header;
use axum::http::{Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing;
use axum::{Json, Router};
#[cfg(feature = "docs")]
use std::sync::LazyLock;
use tracing::Instrument;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub use error::ApiError;
use error::ErrorBody;

use crate::metrics::duration_ms;
use crate::state::AppState;

/// Builds the v1 API router together with the OpenAPI document derived from its routes.
///
/// The paths are collected from the `#[utoipa::path]` handlers at build time through
/// [`utoipa_axum`], so the router and the document cannot drift apart; the document's metadata
/// (info, tags, schemas) comes from [`openapi::base`]. The request tracing middleware is not
/// applied here: [`router`] applies it once to the finished router, after the infrastructure
/// endpoints and the fallbacks are registered, so those requests are audited too.
fn api() -> (Router<Arc<AppState>>, utoipa::openapi::OpenApi) {
    let (router, openapi) = OpenApiRouter::with_openapi(openapi::base())
        .routes(routes!(healthz))
        .routes(routes!(peek::peek))
        .routes(routes!(fetch::fetch))
        .routes(routes!(search::search))
        .routes(routes!(openwebui_search::openwebui_search))
        .split_for_parts();

    (router, openapi)
}

/// The path of the health endpoint, exempt from request tracing and audit events: probes are
/// infrastructure traffic, and auditing each one would only add noise to the log pipeline.
const HEALTHZ_PATH: &str = "/healthz";

/// Wraps a request in a server span and emits one audit event for it.
///
/// The event is emitted at `INFO` on every request — API calls, infrastructure endpoints, and
/// the `404`/`405` fallbacks — including failures, so a log pipeline can retain an unsampled
/// request record independently of trace sampling.
///
/// `http.route` is recorded only for matched routes. The fallbacks have no route, and the
/// conventions forbid falling back to the raw path, which would make the attribute
/// high-cardinality.
async fn trace_request(request: Request, next: Next) -> Response {
    if request.uri().path() == HEALTHZ_PATH {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned());
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
    let response = next.run(request).instrument(span.clone()).await;
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
        );
    });

    response
}

/// Returns the OpenAPI document derived from the API routes.
#[must_use]
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    api().1
}

/// Returns the OpenAPI document as the JSON value `/docs` renders from and `export-docs --spec`
/// writes.
///
/// # Panics
///
/// Panics if the OpenAPI document cannot be serialized to JSON, which cannot happen for a
/// document derived by `utoipa`.
#[must_use]
pub fn openapi_spec() -> serde_json::Value {
    serde_json::to_value(openapi_document()).expect("the OpenAPI document is always serializable")
}

/// Builds the router for the whole service: the v1 API plus the OpenAPI document and,
/// with the `docs` feature, the documentation page.
///
/// Every error response — including the router-level `404` and `405` fallbacks — renders the
/// `{"error": {type, message}}` envelope. The API operations' routes come from `api()`
/// together with the document served at `/openapi.json`; with the `docs` feature the page at
/// `/docs` and the legacy `/swagger-ui` redirect are added. These infrastructure endpoints are
/// hand-registered like the fallbacks — they are not OpenAPI-documented operations.
///
/// # Panics
///
/// Panics if the OpenAPI document cannot be serialized to JSON, which cannot happen for a
/// document derived by `utoipa`.
pub fn router(state: AppState) -> Router {
    let (router, openapi) = api();

    // Serialize once at startup: the document is static, and the extension clone per request
    // is then just a refcount bump instead of a deep clone plus serialization.
    let document =
        serde_json::to_vec(&openapi).expect("the OpenAPI document is always serializable");
    let router = router.route(
        "/openapi.json",
        routing::get(serve_openapi).layer(Extension(Bytes::from(document))),
    );

    #[cfg(feature = "docs")]
    let router = router
        .route("/docs", routing::get(docs))
        .route("/swagger-ui", routing::get(swagger_ui_redirect))
        .route("/swagger-ui/", routing::get(swagger_ui_redirect));

    // The tracing middleware is applied last so it wraps every route and fallback, including
    // the hand-registered infrastructure endpoints and the 404/405 handlers above.
    router
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn(trace_request))
        .with_state(Arc::new(state))
}

/// Renders the documentation page from the OpenAPI document.
#[cfg(feature = "docs")]
fn render_docs_page() -> String {
    neuradex_docs::render(&openapi_spec())
}

/// Serves the API documentation page at `GET /docs`.
///
/// Release builds render the page once and cache it; debug builds render per request, so
/// template and theme edits show up on refresh during development.
#[cfg(feature = "docs")]
async fn docs() -> impl IntoResponse {
    static PAGE: LazyLock<String> = LazyLock::new(render_docs_page);

    let body = if neuradex_docs::RELOAD_SOURCES {
        axum::body::Body::from(render_docs_page())
    } else {
        axum::body::Body::from(Bytes::from_static(PAGE.as_bytes()))
    };

    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body)
}

/// Serves the OpenAPI document as JSON at `GET /openapi.json`.
///
/// The document is the one `api()` derives from the routes, serialized once when the router is
/// built and injected as an extension, so what is served cannot drift from the API operations.
async fn serve_openapi(Extension(document): Extension<Bytes>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], document)
}

/// Redirects the legacy Swagger UI paths to [`docs`].
#[cfg(feature = "docs")]
async fn swagger_ui_redirect() -> impl IntoResponse {
    (StatusCode::FOUND, [(header::LOCATION, "/docs")])
}

/// Handles `GET /healthz`.
#[utoipa::path(
    get,
    path = "/healthz",
    operation_id = "healthz",
    tag = "healthz",
    summary = "Check the service health.",
    description = "Returns `204 No Content` when the service is up. Used as the liveness probe.",
    responses((status = 204, description = "The service is healthy."))
)]
pub(crate) async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// Handles requests that do not match any route.
async fn not_found(uri: Uri) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody::not_found(uri.path())),
    )
}

/// Handles requests for a valid route with an unsupported method.
async fn method_not_allowed(method: Method, uri: Uri) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(ErrorBody::method_not_allowed(&method, uri.path())),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use axum::response::Response;
    use secrecy::SecretString;
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use tracing::field::{Field, Visit};
    use tracing::{Event, Id, Metadata, Subscriber};

    use super::*;

    /// One captured event: its name and its `field name -> rendered value` fields.
    type CapturedEvent = (String, HashMap<String, String>);

    /// Captures emitted events. Installed as the process-global subscriber.
    #[derive(Clone, Default)]
    struct EventLog(Arc<Mutex<Vec<CapturedEvent>>>);

    impl EventLog {
        /// Takes the events captured so far.
        fn events(&self) -> Vec<CapturedEvent> {
            self.0
                .lock()
                .expect("the event log is not poisoned")
                .clone()
        }
    }

    impl Subscriber for EventLog {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut fields = HashMap::new();
            event.record(&mut FieldCollector(&mut fields));

            self.0
                .lock()
                .expect("the event log is not poisoned")
                .push((event.metadata().name().to_owned(), fields));
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    /// Collects an event's fields into a map of `name -> rendered value`.
    struct FieldCollector<'a>(&'a mut HashMap<String, String>);

    impl Visit for FieldCollector<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0
                .entry(field.name().to_owned())
                .or_insert_with(|| format!("{value:?}"));
        }
    }

    /// The shared audit log, installed as the process-global subscriber on first use.
    ///
    /// A global subscriber is required for determinism: the `tracing` macros gate dispatch
    /// through a process-wide callsite-interest cache that concurrent tests rebuild from
    /// whatever dispatcher is registered at the time, so a thread-local subscriber misses
    /// events whose callsites were rebuilt on other threads. A global subscriber takes part
    /// in every rebuild, and events from parallel tests only ever add entries, which the
    /// assertions below account for.
    static AUDIT_LOG: OnceLock<EventLog> = OnceLock::new();

    /// Returns the shared audit log, installing it as the global subscriber once.
    fn audit_log() -> &'static EventLog {
        AUDIT_LOG.get_or_init(|| {
            let log = EventLog::default();

            // The first install wins; nothing else in the test binary sets a subscriber.
            let _ = tracing::subscriber::set_global_default(log.clone());

            log
        })
    }

    /// Whether a captured event is a request audit event for `path`.
    fn is_audit_for(event: &CapturedEvent, path: &str) -> bool {
        event.0 == "neuradex.http.request"
            && event.1.get("url.path").map(String::as_str) == Some(path)
    }

    /// Builds a router around a state constructed with test settings.
    fn router_for_test() -> Router {
        let state = AppState::try_new(
            "test-agent",
            SecretString::from("test-token"),
            Duration::from_secs(30),
            2,
            false,
        )
        .expect("valid state");

        router(state)
    }

    /// Sends a request with `method` to `path` through `router`.
    async fn send(router: Router, method: &str, path: &str) -> Response {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .expect("valid request");

        router
            .oneshot(request)
            .await
            .expect("the router is infallible")
    }

    /// Sends a request with `method` to `path` carrying a raw `body` of the given content
    /// type through `router`.
    async fn send_body(
        router: Router,
        method: &str,
        path: &str,
        content_type: &str,
        body: &str,
    ) -> Response {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", content_type)
            .body(Body::from(body.to_owned()))
            .expect("valid request");

        router
            .oneshot(request)
            .await
            .expect("the router is infallible")
    }

    /// Sends `body` as JSON to `path` through `router`.
    async fn send_json(router: Router, path: &str, body: Value) -> Response {
        send_body(router, "POST", path, "application/json", &body.to_string()).await
    }

    /// Reads `response`'s body as a JSON value.
    async fn body_json(response: Response) -> Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");

        serde_json::from_slice(&body).expect("valid json")
    }

    /// Asserts that `response` is a JSON error of `kind` with the given `message`.
    async fn assert_error(response: Response, status: StatusCode, kind: &str, message: &str) {
        assert_eq!(response.status(), status);

        let error = body_json(response).await;

        assert_eq!(error["error"]["type"], kind);
        assert_eq!(error["error"]["message"], message);
    }

    /// Asserts that `response` is a JSON error of `kind`, without pinning the wording.
    async fn assert_error_kind(response: Response, status: StatusCode, kind: &str) {
        assert_eq!(response.status(), status);

        let error = body_json(response).await;

        assert_eq!(error["error"]["type"], kind);
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "the error message is empty: {error}"
        );
    }

    #[tokio::test]
    async fn serves_the_openapi_document() {
        let response = send(router_for_test(), "GET", "/openapi.json").await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let document: Value = serde_json::from_slice(&body).expect("valid json");

        assert!(
            document["paths"]
                .as_object()
                .is_some_and(|paths| !paths.is_empty()),
            "the served document has no paths"
        );
    }

    #[cfg(feature = "docs")]
    #[tokio::test]
    async fn serves_the_documentation_page() {
        let response = send(router_for_test(), "GET", "/docs").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/html; charset=utf-8"),
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let page = String::from_utf8(body.to_vec()).expect("the page is utf-8");

        assert!(
            page.contains(r#"data-docs="neuradex""#),
            "the page is missing the data-docs marker"
        );
    }

    #[cfg(feature = "docs")]
    #[test]
    fn renders_every_documented_path_and_schema() {
        let document = openapi_spec();
        let page = neuradex_docs::render(&document);

        for path in document["paths"].as_object().expect("paths").keys() {
            assert!(page.contains(path), "the page is missing {path}");
        }

        for schema in document["components"]["schemas"]
            .as_object()
            .expect("schemas")
            .keys()
        {
            assert!(
                page.contains(&format!("id=\"schema-{schema}\"")),
                "the page is missing the {schema} schema"
            );
        }
    }

    #[cfg(feature = "docs")]
    #[tokio::test]
    async fn redirects_the_legacy_swagger_ui_paths_to_the_docs() {
        for path in ["/swagger-ui", "/swagger-ui/"] {
            let response = send(router_for_test(), "GET", path).await;

            assert_eq!(response.status(), StatusCode::FOUND, "for {path}");
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok()),
                Some("/docs"),
                "for {path}",
            );
        }
    }

    #[tokio::test]
    async fn serves_the_health_endpoint() {
        let response = send(router_for_test(), "GET", "/healthz").await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn renders_unmatched_routes_as_json_errors() {
        let response = send(router_for_test(), "GET", "/nope").await;

        assert_error(
            response,
            StatusCode::NOT_FOUND,
            "not_found",
            "no route for /nope",
        )
        .await;
    }

    #[tokio::test]
    async fn renders_unsupported_methods_as_json_errors() {
        let response = send(router_for_test(), "GET", "/v1/search").await;

        assert_error(
            response,
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method GET is not allowed for /v1/search",
        )
        .await;
    }

    #[tokio::test]
    async fn audits_every_request_except_health_probes() {
        let log = audit_log();
        let router = router_for_test();

        send(router.clone(), "GET", "/healthz").await;
        send(router.clone(), "GET", "/nope").await;
        send(router, "POST", "/v1/search").await;

        let events = log.events();

        // Health probes are never audited. No other test requests /healthz, so any such
        // event could only come from this request.
        assert!(
            !events.iter().any(|event| is_audit_for(event, "/healthz")),
            "the health probe was audited: {events:?}"
        );

        // Unmatched requests are audited, but the raw path never leaks into `http.route`:
        // the fallback has no route, and the attribute must stay low-cardinality.
        let unmatched: Vec<_> = events
            .iter()
            .filter(|event| is_audit_for(event, "/nope"))
            .collect();

        assert!(
            !unmatched.is_empty(),
            "unmatched requests are not audited: {events:?}"
        );
        assert!(
            unmatched
                .iter()
                .all(|(_, fields)| fields.get("http.route").map(String::as_str) != Some("/nope")),
            "the raw path leaked into http.route: {events:?}"
        );

        // Matched API routes carry their route template.
        assert!(
            events
                .iter()
                .any(|(_, fields)| fields.get("url.path").map(String::as_str)
                    == Some("/v1/search")
                    && fields.get("http.route").map(String::as_str) == Some("/v1/search")),
            "API requests are not audited with their route: {events:?}"
        );
    }

    /// Asserts that `response` is the JSON `invalid_param` envelope for a rejected query or
    /// body, without pinning the parser's exact message.
    async fn assert_invalid(response: Response, prefix: &str) {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json"),
            "expected the JSON error envelope, not a plain-text rejection",
        );

        let error = body_json(response).await;

        assert_eq!(error["error"]["type"], "invalid_param");
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|message| message.starts_with(prefix)),
            "unexpected message: {error}"
        );
    }

    #[tokio::test]
    async fn renders_malformed_queries_as_json_errors() {
        // Each of these fails extraction — an unknown field, a missing required field, and a
        // non-parseable value — and must render the envelope, not axum's plain-text 400.
        for path in ["/v1/fetch?nope=1", "/v1/peek?include=redirects"] {
            let response = send(router_for_test(), "GET", path).await;

            assert_invalid(response, "invalid query parameters: ").await;
        }
    }

    #[tokio::test]
    async fn renders_invalid_query_values_as_json_errors() {
        // Each of these extracts cleanly but fails the handlers' own validation — before any
        // network I/O — and must render the envelope.
        for path in [
            "/v1/fetch?url=not%20a%20url",
            "/v1/fetch?url=file:///etc/passwd",
            "/v1/fetch?url=https://example.com&redirects=6",
            "/v1/fetch?url=https://example.com&include=nope",
            "/v1/peek?url=https://example.com&include=headers",
        ] {
            let response = send(router_for_test(), "GET", path).await;

            assert_error_kind(response, StatusCode::BAD_REQUEST, "invalid_param").await;
        }
    }

    #[tokio::test]
    async fn renders_invalid_search_bodies_as_json_errors() {
        for body in [
            json!({"query": ""}),
            json!({"query": "rust", "timeout": 0}),
            json!({"query": "rust", "timeout": 31}),
        ] {
            let response = send_json(router_for_test(), "/v1/search", body).await;

            assert_error_kind(response, StatusCode::BAD_REQUEST, "invalid_param").await;
        }
    }

    #[tokio::test]
    async fn renders_malformed_search_bodies_as_json_errors() {
        for body in [
            "not json",
            r#"{"query": "rust", "nope": 1}"#,
            r#"{"limit": 10}"#,
            r#"{"query": "rust", "timeout": "ten"}"#,
            r#"{"query": "rust", "limit": 1.5}"#,
        ] {
            let response = send_body(
                router_for_test(),
                "POST",
                "/v1/search",
                "application/json",
                body,
            )
            .await;

            assert_invalid(response, "invalid request body: ").await;
        }
    }

    #[tokio::test]
    async fn renders_malformed_openwebui_search_bodies_as_json_errors() {
        // Each of these fails extraction — a syntax error, an unknown field, a missing or
        // non-parseable field — and must render the envelope, not axum's plain-text rejection.
        for body in [
            "not json",
            r#"{"query": "rust", "count": 5, "nope": 1}"#,
            r#"{"count": 5}"#,
            r#"{"query": "rust", "count": "five"}"#,
            r#"{"query": "rust", "count": -1}"#,
            r#"{"query": "rust", "count": 1.5}"#,
        ] {
            let response = send_body(
                router_for_test(),
                "POST",
                "/v1/openwebui_search",
                "application/json",
                body,
            )
            .await;

            assert_invalid(response, "invalid request body: ").await;
        }
    }

    #[tokio::test]
    async fn renders_a_missing_json_content_type_as_an_error() {
        for path in ["/v1/search", "/v1/openwebui_search"] {
            let response = send_body(
                router_for_test(),
                "POST",
                path,
                "text/plain",
                r#"{"query": "rust"}"#,
            )
            .await;

            assert_error(
                response,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "expected the request body to be JSON (Content-Type: application/json)",
            )
            .await;
        }
    }
}
