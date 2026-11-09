//! A high-level harness for the neuradex HTTP service.
//!
//! [`TestApp`] assembles the production router around mock page and Kagi endpoints, then
//! sends requests through the real extraction and handler stack. Every instance owns an
//! isolated [`httpmock::MockServer`], so tests can run in parallel.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use httpmock::{Mock, MockServer};
use kagi::DEFAULT_MAX_SESSIONS;
use neuradex::api::v1;
use neuradex::api::v1::AppState;
use serde_json::Value;

use crate::kagi::KagiHarness;

/// The user agent reported by the test application.
pub const USER_AGENT_VALUE: &str = "neuradex-test";

/// A running application wired to a local mock HTTP server.
pub struct TestApp {
    /// The mock server backing both page fetches and Kagi requests.
    pub server: Arc<MockServer>,
    /// The Kagi mock API shared with the client-level harness.
    kagi: KagiHarness,
    router: Router,
}

impl TestApp {
    /// Starts an application with the default test settings.
    ///
    /// # Panics
    ///
    /// Panics if the test HTTP or Kagi client fails to build.
    #[must_use]
    pub fn start() -> Self {
        Self::with_kagi_sessions(DEFAULT_MAX_SESSIONS)
    }

    /// Starts an application whose Kagi client may establish up to `max_sessions` sessions.
    ///
    /// # Panics
    ///
    /// Panics if the test HTTP or Kagi client fails to build.
    #[must_use]
    pub fn with_kagi_sessions(max_sessions: usize) -> Self {
        let server = Arc::new(MockServer::start());
        let kagi = KagiHarness::with_server(Arc::clone(&server));
        let kagi_client = kagi.client_with(max_sessions, Duration::from_secs(60));

        let state = AppState::try_new_with_kagi_client(
            USER_AGENT_VALUE,
            kagi_client,
            Duration::from_secs(5),
        )
        .expect("the test application state builds");

        Self {
            server,
            kagi,
            router: v1::router(state),
        }
    }

    /// Returns the absolute URL of `path` on the mock server.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        self.server.url(path)
    }

    /// Sends a `GET` request through the application router.
    ///
    /// # Panics
    ///
    /// Panics if the constructed test request is invalid.
    pub async fn get(&self, path: &str) -> Response {
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .expect("the test request is valid");

        self.send(request).await
    }

    /// Sends a `POST` request with a JSON body through the application router.
    ///
    /// # Panics
    ///
    /// Panics if the constructed test request is invalid.
    pub async fn post_json(&self, path: &str, body: &Value) -> Response {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(body).expect("the test body is valid JSON"),
            ))
            .expect("the test request is valid");

        self.send(request).await
    }

    async fn send(&self, request: Request<Body>) -> Response {
        use tower::ServiceExt;

        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible")
    }

    /// Reads a response body as JSON.
    ///
    /// # Panics
    ///
    /// Panics if the response body cannot be read or is not valid JSON.
    pub async fn json(response: Response) -> Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the response body is readable");

        serde_json::from_slice(&body).expect("the response body is JSON")
    }

    /// Asserts that `response` is the JSON error envelope of `kind` and returns the body.
    ///
    /// # Panics
    ///
    /// Panics if the status, content type, error type, or message does not match.
    pub async fn assert_error(response: Response, status: StatusCode, kind: &str) -> Value {
        assert_eq!(response.status(), status);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json"),
        );

        let body = Self::json(response).await;

        assert_eq!(body["error"]["type"], kind);
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "the error message is empty: {body}"
        );

        body
    }

    /// Mocks the Kagi token request that establishes a session.
    #[must_use]
    pub fn mock_kagi_session(&self) -> Mock<'_> {
        self.kagi.mock_session()
    }

    /// Mocks the Kagi search document that supplies the session nonce.
    #[must_use]
    pub fn mock_kagi_nonce(&self) -> Mock<'_> {
        self.kagi.mock_nonce()
    }

    /// Mocks a Kagi search stream. The mock accepts both the first request (with a nonce)
    /// and later requests that reuse the session without one.
    #[must_use]
    pub fn mock_kagi_search_stream(&self, query: &str, fixture_name: &str) -> Mock<'_> {
        self.mock_kagi_search_stream_delayed(query, fixture_name, Duration::ZERO)
    }

    /// Mocks a Kagi search stream, delaying its response by `delay`.
    #[must_use]
    pub fn mock_kagi_search_stream_delayed(
        &self,
        query: &str,
        fixture_name: &str,
        delay: Duration,
    ) -> Mock<'_> {
        self.kagi
            .mock_search_stream_reusable_for(query, fixture_name, delay)
    }
}
