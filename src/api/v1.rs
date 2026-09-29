//! API v1: routes and handlers.

pub mod error;
pub mod extract;
pub mod fetch;
#[cfg(feature = "utoipa")]
pub mod openapi;
pub mod peek;
pub mod redirect;
pub mod search;
pub mod stream;

use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "utoipa")]
use axum::Extension;
use axum::Json;
use axum::Router;
#[cfg(feature = "docs")]
use axum::http::header;
use axum::http::{Method, StatusCode, Uri};
#[cfg(feature = "utoipa")]
use axum::response::IntoResponse;
use axum::routing::get;
use secrecy::SecretString;
#[cfg(all(feature = "docs", not(debug_assertions)))]
use std::sync::OnceLock;
#[cfg(feature = "utoipa")]
use utoipa_axum::router::OpenApiRouter;
#[cfg(feature = "utoipa")]
use utoipa_axum::routes;
use wreq::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue, USER_AGENT};
use wreq::redirect::Policy;
use wreq_util::Emulation;

pub use error::ApiError;
use error::ErrorBody;

/// The maximum number of redirects the fetch endpoints will follow.
pub const MAX_REDIRECTS: u32 = 5;

/// The maximum number of searches that may be in flight at once; additional requests queue.
pub const MAX_CONCURRENT_SEARCHES: usize = 2;

/// The maximum value accepted by the search endpoint's `timeout` parameter, in seconds.
pub const MAX_SEARCH_TIMEOUT_SECS: u64 = 30;

/// Errors that can occur while constructing the application state.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// A configured header value is invalid.
    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] wreq::header::InvalidHeaderValue),
    /// The HTTP client failed to build.
    #[error("could not build http client: {0}")]
    Client(#[from] wreq::Error),
    /// The Kagi client failed to build.
    #[error("could not build kagi client: {0}")]
    Kagi(#[from] kagi::Error),
}

/// The shared application state passed to the handlers.
pub struct AppState {
    /// The HTTP client used for fetching pages.
    ///
    /// Redirects are disabled: the fetch handlers follow them manually so that each hop can be
    /// counted and reported in the response metrics.
    pub client: wreq::Client,
    /// The client used for searching with Kagi.
    pub kagi_client: kagi::Client,
    /// Gates the number of searches in flight; each search acquires a permit, extra requests
    /// wait here until a slot frees up.
    pub search_gate: Arc<tokio::sync::Semaphore>,
    /// The default request headers sent with every fetch, kept for reporting.
    pub default_headers: HeaderMap,
}

impl AppState {
    /// Constructs the application state: the shared HTTP client and the Kagi client.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP or Kagi clients fail to build.
    pub fn new(
        user_agent: &str,
        kagi_token: SecretString,
        timeout: Duration,
        max_sessions: usize,
    ) -> Result<Self, BuildError> {
        let default_headers = default_headers(user_agent)?;

        let client = wreq::Client::builder()
            .emulation(Emulation::Firefox142)
            .default_headers(default_headers.clone())
            // Redirects are followed manually by the fetch handlers, which count each hop and
            // report the chain in the response metrics.
            .redirect(Policy::none())
            .timeout(timeout)
            .build()?;

        let kagi_client = kagi::Client::with_token_and_options(
            kagi_token,
            &kagi::ClientOptions {
                max_sessions,
                ..kagi::ClientOptions::default()
            },
        )?;

        Ok(Self {
            client,
            kagi_client,
            search_gate: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_SEARCHES)),
            default_headers,
        })
    }
}

/// Returns the request headers layered on top of the emulated browser profile.
///
/// `Accept-Encoding` must be set explicitly: like reqwest, wreq does not advertise the header
/// itself even though it decompresses responses — and its absence is enough to get flagged.
fn default_headers(user_agent: &str) -> Result<HeaderMap, BuildError> {
    let mut headers = HeaderMap::new();

    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br, zstd"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(user_agent).map_err(BuildError::from)?,
    );

    Ok(headers)
}

/// Builds the v1 API router together with the OpenAPI document derived from its routes.
///
/// The paths are collected from the `#[utoipa::path]` handlers at build time through
/// [`utoipa_axum`], so the router and the document cannot drift apart; the document's metadata
/// (info, tags, schemas) comes from [`openapi::base`].
#[cfg(feature = "utoipa")]
fn api() -> (Router<Arc<AppState>>, utoipa::openapi::OpenApi) {
    OpenApiRouter::with_openapi(openapi::base())
        .routes(routes!(healthz))
        .routes(routes!(peek::peek))
        .routes(routes!(fetch::fetch))
        .routes(routes!(search::search))
        .split_for_parts()
}

/// Builds the v1 API router for builds without `utoipa`.
///
/// Without the `utoipa` feature there is no OpenAPI document, so the routes are registered by
/// hand. The router-level tests are the drift pin: they assert that both router shapes register
/// the same four paths.
#[cfg(not(feature = "utoipa"))]
fn api() -> Router<Arc<AppState>> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/peek", get(peek::peek))
        .route("/v1/fetch", get(fetch::fetch))
        .route("/v1/search", get(search::search))
}

/// Returns the OpenAPI document derived from the API routes.
#[cfg(feature = "utoipa")]
#[must_use]
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    api().1
}

/// Builds the router for the whole service: the v1 API plus the documentation endpoints.
///
/// Every error response — including the router-level `404` and `405` fallbacks — renders the
/// `{"error": {type, message}}` envelope. With the `utoipa` feature the API operations' routes
/// come from [`api`] together with the document served at `/openapi.json`; with the `docs`
/// feature the page at `/docs` and the legacy `/swagger-ui` redirect are added. These
/// infrastructure endpoints are hand-registered like the fallbacks — they are not
/// OpenAPI-documented operations.
pub fn router(state: AppState) -> Router {
    #[cfg(feature = "utoipa")]
    let (router, openapi) = api();
    #[cfg(not(feature = "utoipa"))]
    let router = api();

    #[cfg(feature = "utoipa")]
    let router = router.route(
        "/openapi.json",
        get(serve_openapi).layer(Extension(openapi)),
    );

    #[cfg(feature = "docs")]
    let router = router
        .route("/docs", get(docs))
        .route("/swagger-ui", get(swagger_ui_redirect))
        .route("/swagger-ui/", get(swagger_ui_redirect));

    router
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(Arc::new(state))
}

/// Renders the documentation page from the OpenAPI document.
#[cfg(feature = "docs")]
fn render_docs_page() -> String {
    neuradex_docs::render(&openapi_document())
}

/// Serves the API documentation page at `GET /docs`.
///
/// Release builds render the page once and cache it; debug builds render per request, so
/// template and theme edits show up on refresh during development.
#[cfg(feature = "docs")]
async fn docs() -> impl IntoResponse {
    #[cfg(not(debug_assertions))]
    static PAGE: OnceLock<String> = OnceLock::new();

    #[cfg(not(debug_assertions))]
    let body = PAGE.get_or_init(render_docs_page).clone();
    #[cfg(debug_assertions)]
    let body = render_docs_page();

    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body)
}

/// Serves the OpenAPI document as JSON at `GET /openapi.json`.
///
/// The document is the one [`api`] derives from the routes, injected as an extension at router
/// construction, so what is served cannot drift from the API operations.
#[cfg(feature = "utoipa")]
async fn serve_openapi(
    Extension(document): Extension<utoipa::openapi::OpenApi>,
) -> impl IntoResponse {
    Json(document)
}

/// Redirects the legacy Swagger UI paths to [`docs`].
#[cfg(feature = "docs")]
async fn swagger_ui_redirect() -> impl IntoResponse {
    (StatusCode::FOUND, [(header::LOCATION, "/docs")])
}

/// Handles `GET /healthz`.
#[cfg_attr(
    feature = "utoipa",
    utoipa::path(
        get,
        path = "/healthz",
        tag = "healthz",
        summary = "Check the service health.",
        description = "Returns `204 No Content` when the service is up. Used as the liveness probe.",
        responses((status = 204, description = "The service is healthy."))
    )
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
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use axum::response::Response;
    use secrecy::SecretString;
    use serde_json::Value;
    use tower::ServiceExt;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use wreq::redirect::Policy;

    use super::*;

    /// Builds a router around a state constructed with test settings.
    fn router_for_test() -> Router {
        let state = AppState::new(
            "test-agent",
            SecretString::from("test-token"),
            Duration::from_secs(30),
            2,
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

    /// Asserts that `response` is a JSON error of `kind` with the given `message`.
    async fn assert_error(response: Response, status: StatusCode, kind: &str, message: &str) {
        assert_eq!(response.status(), status);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let error: Value = serde_json::from_slice(&body).expect("valid json");

        assert_eq!(error["error"]["type"], kind);
        assert_eq!(error["error"]["message"], message);
    }

    #[cfg(feature = "utoipa")]
    #[tokio::test]
    async fn serves_the_openapi_document() {
        let response = send(router_for_test(), "GET", "/openapi.json").await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let document: Value = serde_json::from_slice(&body).expect("valid json");

        for path in ["/healthz", "/v1/fetch", "/v1/peek", "/v1/search"] {
            assert!(document["paths"].get(path).is_some(), "missing {path}");
        }
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
        let response = send(router_for_test(), "POST", "/v1/search").await;

        assert_error(
            response,
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method POST is not allowed for /v1/search",
        )
        .await;
    }

    /// Asserts that `response` is the JSON `invalid_param` envelope for a query that failed to
    /// deserialize, without pinning the parser's exact message.
    async fn assert_invalid_param(response: Response) {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json"),
            "expected the JSON error envelope, not a plain-text rejection",
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let error: Value = serde_json::from_slice(&body).expect("valid json");

        assert_eq!(error["error"]["type"], "invalid_param");
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("invalid query parameters: ")),
            "unexpected message: {error}"
        );
    }

    #[tokio::test]
    async fn renders_malformed_queries_as_json_errors() {
        // Each of these fails extraction — an unknown field, a missing required field, and a
        // non-parseable value — and must render the envelope, not axum's plain-text 400.
        for path in [
            "/v1/fetch?nope=1",
            "/v1/peek?include=redirects",
            "/v1/search?timeout=abc",
        ] {
            let response = send(router_for_test(), "GET", path).await;

            assert_invalid_param(response).await;
        }
    }

    #[tokio::test]
    async fn renders_invalid_query_values_as_json_errors() {
        // Each of these extracts cleanly but fails the handlers' own validation — before any
        // network I/O — so the exact messages can be pinned through the router.
        for (path, message) in [
            ("/v1/search?query=", "the query parameter is empty"),
            (
                "/v1/fetch?url=not%20a%20url",
                "invalid url: relative URL without a base",
            ),
            (
                "/v1/fetch?url=https://example.com&redirects=6",
                "redirects must be between 0 and 5",
            ),
            (
                "/v1/fetch?url=https://example.com&include=nope",
                "unknown include: nope",
            ),
            (
                "/v1/peek?url=https://example.com&include=headers",
                "unknown include: headers",
            ),
            (
                "/v1/search?query=rust&timeout=31",
                "the timeout parameter must be between 1 and 30 seconds",
            ),
        ] {
            let response = send(router_for_test(), "GET", path).await;

            assert_error(response, StatusCode::BAD_REQUEST, "invalid_param", message).await;
        }
    }

    /// A minimal page served by the mock server: a head with metadata and a body with a
    /// marker that must never reach the peek response.
    const PAGE: &str = r#"<!doctype html>
        <html>
            <head>
                <title>Hello world</title>
                <meta property="og:site_name" content="Example">
                <link rel="canonical" href="https://example.com/canonical">
            </head>
            <body><h1>Body content</h1></body>
        </html>"#;

    /// Builds a wreq client for the fetch handlers, mirroring the production policy:
    /// redirects are followed by hand, so the client itself never follows them.
    fn fetch_client() -> wreq::Client {
        wreq::Client::builder()
            .emulation(Emulation::Firefox142)
            .default_headers(default_headers("test-agent").expect("valid headers"))
            .redirect(Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("valid client")
    }

    /// Builds a kagi client whose requests go to `base_url` — a mock server in tests.
    fn kagi_client_at(base_url: &str) -> kagi::Client {
        kagi::Client::with_token_and_options(
            "test-token",
            &kagi::ClientOptions {
                base_url: base_url.to_owned(),
                ..kagi::ClientOptions::default()
            },
        )
        .expect("valid client")
    }

    /// Builds a router around the given clients.
    fn router_with(client: wreq::Client, kagi_client: kagi::Client) -> Router {
        router(AppState {
            client,
            kagi_client,
            search_gate: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_SEARCHES)),
            default_headers: default_headers("test-agent").expect("valid headers"),
        })
    }

    /// Builds a router whose fetch handlers use a plain test client.
    ///
    /// The kagi client is never used by the fetch endpoints; the loopback port is only a
    /// placeholder that would fail fast if a handler ever reached for it.
    fn fetch_router() -> Router {
        router_with(fetch_client(), kagi_client_at("http://127.0.0.1:1"))
    }

    /// Builds the `/v1/fetch` request path with the given query parameters.
    fn fetch_path(url: &str, extra: &[(&str, &str)]) -> String {
        endpoint_path("/v1/fetch", &[("url", url)], extra)
    }

    /// Builds the `/v1/peek` request path with the given query parameters.
    fn peek_path(url: &str, extra: &[(&str, &str)]) -> String {
        endpoint_path("/v1/peek", &[("url", url)], extra)
    }

    /// Builds the `/v1/search` request path with the given query parameters.
    fn search_path(query: &str, extra: &[(&str, &str)]) -> String {
        endpoint_path("/v1/search", &[("query", query)], extra)
    }

    /// Encodes `params` and `extra` as the query string of `endpoint`'s request path.
    fn endpoint_path(endpoint: &str, params: &[(&str, &str)], extra: &[(&str, &str)]) -> String {
        let mut all = params.to_vec();
        all.extend_from_slice(extra);

        let query = url::Url::parse_with_params("https://neuradex.test", &all)
            .expect("valid")
            .query()
            .expect("parameters")
            .to_owned();

        format!("{endpoint}?{query}")
    }

    /// Reads the JSON body of `response`.
    async fn json_body(response: Response) -> Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");

        serde_json::from_slice(&body).expect("valid json")
    }

    /// Mounts a page at `at` on `server` with the given content type and body.
    async fn mount_page(server: &MockServer, at: &str, content_type: &str, body: &str) {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(
                // `set_body_raw` keeps the given content type, which `set_body_string` would
                // override with `text/plain`.
                ResponseTemplate::new(200).set_body_raw(body, content_type),
            )
            .mount(server)
            .await;
    }

    /// The `search_results_json` payload served by the kagi socket mock.
    const KAGI_RESULTS: &str = r#"{"items": [
        {"title": "First result", "url": "https://example.com/first", "snippet": "first snippet"},
        {"title": "Second result", "url": "https://example.com/second", "snippet": "second snippet"}
    ]}"#;

    /// A successful SSE stream response carrying two structured results.
    fn kagi_results_response() -> ResponseTemplate {
        let payload = Value::String(KAGI_RESULTS.to_owned());

        ResponseTemplate::new(200).set_body_raw(
            format!(
                "data: {}\n",
                serde_json::to_string(&serde_json::json!([
                    {"tag": "search_results_json", "payload": payload},
                ]))
                .expect("the sse payload is valid json"),
            ),
            "text/event-stream",
        )
    }

    /// Mounts the mocked kagi service on `server`: the session establishment and the socket
    /// stream, which responds with `response`.
    async fn mount_kagi_service(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("token", "test-token"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("Set-Cookie", "kagi_session=test-session"),
            )
            .expect(1)
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<html><head><script>window.sse_nonce = "0123456789abcdef0123456789abcdef";</script></head></html>"#,
            ))
            .expect(1)
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/socket/search"))
            .respond_with(response)
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn fetch_returns_the_metadata_body_and_metrics() {
        let server = MockServer::start().await;
        let page = format!("{}/page", server.uri());
        mount_page(&server, "/page", "text/html; charset=utf-8", PAGE).await;

        let response = send(fetch_router(), "GET", &fetch_path(&page, &[])).await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;

        assert_eq!(body["url"], page);
        assert_eq!(body["metadata"]["title"], "Hello world");
        assert_eq!(body["metadata"]["og"]["site_name"], "Example");
        assert_eq!(
            body["metadata"]["canonical"],
            "https://example.com/canonical"
        );
        assert_eq!(body["body"], PAGE);
        assert_eq!(body["truncated"], false);

        let metrics = &body["metrics"];
        assert_eq!(metrics["status"], 200);
        assert_eq!(metrics["final_url"], page);
        assert_eq!(metrics["content_type"], "text/html");
        assert_eq!(metrics["bytes_read"].as_u64(), Some(PAGE.len() as u64));
        assert_eq!(metrics["redirects_followed"], 0);
        assert_eq!(metrics["truncated"], false);

        // Without `include=headers` the header sections are absent.
        assert!(body.get("request_headers").is_none());
        assert!(body.get("response_headers").is_none());
    }

    #[tokio::test]
    async fn fetch_reports_the_request_and_response_headers_when_included() {
        let server = MockServer::start().await;
        let page = format!("{}/page", server.uri());
        mount_page(&server, "/page", "text/html; charset=utf-8", PAGE).await;

        let response = send(
            fetch_router(),
            "GET",
            &fetch_path(&page, &[("include", "headers")]),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;

        // The request the mock received must carry the layered default headers: the
        // browser-emulated client only advertises `Accept-Encoding` because it is set
        // explicitly, so both are pinned on the request the mock actually saw.
        let request = &server.received_requests().await.expect("requests")[0];

        assert_eq!(
            request
                .headers
                .get("user-agent")
                .and_then(|value| value.to_str().ok()),
            Some("test-agent"),
        );
        assert_eq!(
            request
                .headers
                .get("accept-encoding")
                .and_then(|value| value.to_str().ok()),
            Some("gzip, deflate, br, zstd"),
        );

        // The response headers are reported as they were received from the mock.
        assert_eq!(
            body["response_headers"]["content-type"],
            "text/html; charset=utf-8",
        );
        assert_eq!(body["request_headers"]["user-agent"], "test-agent");
    }

    #[tokio::test]
    async fn fetch_follows_redirects_and_reports_the_hops() {
        let server = MockServer::start().await;
        let base = server.uri();

        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/mid"))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/mid"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", format!("{base}/end")),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/end"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE, "text/html"))
            .expect(1)
            .mount(&server)
            .await;

        let response = send(
            fetch_router(),
            "GET",
            &fetch_path(&format!("{base}/start"), &[("include", "redirects")]),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;

        assert_eq!(body["metrics"]["status"], 200);
        assert_eq!(body["metrics"]["redirects_followed"], 2);
        assert_eq!(body["metrics"]["final_url"], format!("{base}/end"));

        let hops: Vec<(u16, String)> = body["metrics"]["redirects"]
            .as_array()
            .expect("the hop chain is included")
            .iter()
            .map(|hop| {
                (
                    u16::try_from(hop["status"].as_u64().expect("a status")).expect("a status"),
                    hop["url"].as_str().expect("a url").to_owned(),
                )
            })
            .collect();

        assert_eq!(
            hops,
            [(302, format!("{base}/start")), (302, format!("{base}/mid")),],
        );
    }

    #[tokio::test]
    async fn fetch_rejects_binary_content_with_the_error_envelope() {
        let server = MockServer::start().await;
        mount_page(&server, "/image", "image/png", "not really a png").await;

        let response = send(
            fetch_router(),
            "GET",
            &fetch_path(&format!("{}/image", server.uri()), &[]),
        )
        .await;

        assert_error(
            response,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "refusing to fetch non-text content: image/png",
        )
        .await;
    }

    #[tokio::test]
    async fn fetch_renders_upstream_failures_as_the_error_envelope() {
        // Reserve a loopback port and free it: connecting to it is refused immediately.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let response = send(
            fetch_router(),
            "GET",
            &fetch_path(&format!("http://127.0.0.1:{port}/page"), &[]),
        )
        .await;

        // 5xx bodies stay generic: the connection failure detail is logged, not returned.
        assert_error(
            response,
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "the upstream request failed",
        )
        .await;
    }

    #[tokio::test]
    async fn peek_returns_only_the_document_head() {
        let server = MockServer::start().await;
        let page = format!("{}/page", server.uri());
        mount_page(&server, "/page", "text/html; charset=utf-8", PAGE).await;

        let response = send(fetch_router(), "GET", &peek_path(&page, &[])).await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;

        assert_eq!(body["url"], page);
        assert_eq!(body["metadata"]["title"], "Hello world");
        assert_eq!(body["metadata"]["og"]["site_name"], "Example");
        assert_eq!(body["metrics"]["status"], 200);
        assert_eq!(body["metrics"]["truncated"], false);

        // The body of the page is not part of the peek response — in any form.
        assert!(body.get("body").is_none());
        assert!(body.get("request_headers").is_none());
        assert!(body.get("response_headers").is_none());
    }

    #[tokio::test]
    async fn search_returns_the_kagi_results_truncated_to_the_limit() {
        let server = MockServer::start().await;
        mount_kagi_service(&server, kagi_results_response()).await;

        let response = send(
            router_with(fetch_client(), kagi_client_at(&server.uri())),
            "GET",
            &search_path("rust", &[("limit", "1")]),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;

        assert_eq!(body["query"], "rust");
        assert_eq!(body["results"].as_array().expect("results").len(), 1);
        assert_eq!(body["results"][0]["title"], "First result");
        assert_eq!(body["results"][0]["url"], "https://example.com/first");
        assert_eq!(body["metrics"]["result_count"], 1);
        assert!(body["metrics"]["queue_ms"].is_u64());
        assert!(body["metrics"]["total_ms"].is_u64());
    }

    #[tokio::test]
    async fn search_times_out_past_its_deadline() {
        let server = MockServer::start().await;
        mount_kagi_service(
            &server,
            // The stream answers long after the deadline expires, so the search can only
            // finish successfully if the timeout did not fire.
            kagi_results_response().set_delay(Duration::from_secs(3)),
        )
        .await;

        let response = send(
            router_with(fetch_client(), kagi_client_at(&server.uri())),
            "GET",
            &search_path("rust", &[("timeout", "1")]),
        )
        .await;

        // The search — session establishment included — is bounded by the requested
        // timeout, and renders the upstream envelope with its generic 5xx message: the
        // timeout detail goes to the logs, not to the client.
        assert_error(
            response,
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "the upstream request failed",
        )
        .await;

        // The search was underway — all three kagi requests were made — and none of them
        // completed within the deadline.
        let received = server.received_requests().await.expect("requests");
        let paths: Vec<&str> = received.iter().map(|request| request.url.path()).collect();

        assert_eq!(paths, ["/search", "/", "/socket/search"]);
    }
}
