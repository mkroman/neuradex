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
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use axum::response::Response;
    use secrecy::SecretString;
    use serde_json::Value;
    use tower::ServiceExt;

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
}
