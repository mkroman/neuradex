//! API v1: routes and handlers.

pub mod error;
pub mod extract;
pub mod fetch;
pub mod openapi;
pub mod peek;
pub mod redirect;
pub mod search;
pub mod stream;

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::http::{Method, StatusCode, Uri};
use axum::routing::get;
use secrecy::SecretString;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;
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

/// Builds the router for the whole service: the v1 API plus the OpenAPI document and Swagger UI.
///
/// Every error response — including the router-level `404` and `405` fallbacks — renders the
/// `{"error": {type, message}}` envelope. The OpenAPI document is served as JSON at
/// `/openapi.json` and as Swagger UI at `/swagger-ui`, both through the merged
/// [`utoipa_swagger_ui::SwaggerUi`] router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/peek", get(peek::peek))
        .route("/v1/fetch", get(fetch::fetch))
        .route("/v1/search", get(search::search))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(Arc::new(state))
        .merge(SwaggerUi::new("/swagger-ui").url("/openapi.json", openapi::ApiDoc::openapi()))
}

/// Handles `GET /healthz`.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "healthz",
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

    #[tokio::test]
    async fn serves_the_swagger_ui() {
        let response = send(router_for_test(), "GET", "/swagger-ui/").await;

        assert_eq!(response.status(), StatusCode::OK);
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
}
