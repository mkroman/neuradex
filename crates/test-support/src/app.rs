//! A high-level harness for the neuradex HTTP service.
//!
//! [`TestApp`] assembles the production router around mock page and Kagi endpoints, then
//! sends requests through the real extraction and handler stack. Every instance owns an
//! isolated [`httpmock::MockServer`], so tests can run in parallel.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use httpmock::{Mock, MockServer};
use kagi::Client;
use kagi::ClientOptions;
use neuradex::api::v1;
use neuradex::api::v1::AppState;
use secrecy::SecretString;
use serde_json::Value;

use crate::kagi as support;
use crate::kagi::{SESSION_COOKIE, TOKEN};

/// The user agent reported by the test application.
pub const USER_AGENT_VALUE: &str = "neuradex-test";

/// A running application wired to a local mock HTTP server.
pub struct TestApp {
    /// The mock server backing both page fetches and Kagi requests.
    pub server: MockServer,
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
        Self::with_kagi_sessions(2)
    }

    /// Starts an application whose Kagi client may establish up to `max_sessions` sessions.
    ///
    /// # Panics
    ///
    /// Panics if the test HTTP or Kagi client fails to build.
    #[must_use]
    pub fn with_kagi_sessions(max_sessions: usize) -> Self {
        let server = MockServer::start();

        let kagi_client = Client::with_token_and_options(
            SecretString::from(TOKEN),
            &ClientOptions {
                base_url: server.base_url(),
                max_sessions,
                timeout: Duration::from_secs(5),
                ..ClientOptions::default()
            },
        )
        .expect("the test Kagi client builds");

        let state = AppState::try_new_with_kagi_client(
            USER_AGENT_VALUE,
            kagi_client,
            Duration::from_secs(5),
        )
        .expect("the test application state builds");

        Self {
            server,
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

    /// Mocks the Kagi token request that establishes a session.
    #[must_use]
    pub fn mock_kagi_session(&self) -> Mock<'_> {
        self.server.mock(|when, then| {
            when.method("GET")
                .path("/search")
                .query_param("token", TOKEN)
                .header("x-kagi-authorization", TOKEN);
            then.status(200).header(
                "set-cookie",
                format!("kagi_session={SESSION_COOKIE}; Path=/; HttpOnly"),
            );
        })
    }

    /// Mocks the Kagi landing page that supplies the session nonce.
    #[must_use]
    pub fn mock_kagi_nonce(&self) -> Mock<'_> {
        self.server.mock(|when, then| {
            when.method("GET")
                .path("/")
                .header("x-kagi-authorization", TOKEN);
            then.status(200)
                .header("content-type", "text/html; charset=utf-8")
                .body_from_file(support::fixture("landing.html"));
        })
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
        let query = query.to_owned();

        self.server.mock(move |when, then| {
            when.method("GET")
                .path("/socket/search")
                .query_param("q", query)
                .header("x-kagi-authorization", TOKEN);
            then.status(200)
                .header("content-type", "text/event-stream")
                .delay(delay)
                .body_from_file(support::fixture(fixture_name));
        })
    }
}
