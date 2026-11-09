//! A high-level Kagi test harness backed by a local [`httpmock`] server.
//!
//! The harness serves the token, search-document, stream, and image endpoints the client uses.
//! Tests configure only the responses that matter to them and exercise the real client over
//! HTTP, including its cookie jar, session pool, request headers, and parsers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use httpmock::{Mock, MockServer};
use kagi::{Client, ClientOptions, DEFAULT_MAX_SESSIONS, LANGUAGE, USER_AGENT};

/// The session token configured for the test client.
pub const TOKEN: &str = "test-token";
/// The session cookie set by the token request.
pub const SESSION_COOKIE: &str = "test-session";
/// The rotating search cookie set alongside the session cookie.
pub const SEARCH_COOKIE: &str = "test-search";
/// The nonce embedded in the fixture document.
pub const NONCE: &str = "0123456789abcdef0123456789abcdef";
/// The query used by the stream fixtures.
pub const QUERY: &str = "hello world";
/// The document `Accept` header the browser sends.
const DOCUMENT_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

/// Returns the absolute path of a Kagi fixture.
#[must_use]
pub fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/kagi")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Returns the contents of a Kagi fixture.
///
/// # Panics
///
/// Panics if the fixture cannot be read or is not valid UTF-8.
#[must_use]
pub fn read_fixture(name: &str) -> String {
    String::from_utf8(std::fs::read(fixture(name)).expect("the fixture is readable"))
        .expect("the fixture is valid UTF-8")
}

/// Returns a minimal search document containing the fixture nonce.
pub(crate) fn nonce_body() -> String {
    format!(
        r#"<!doctype html><html><head><script>window.sse_nonce = "{NONCE}";</script></head><body></body></html>"#
    )
}

/// A local Kagi stand-in with helpers for the endpoints the client uses.
pub struct KagiHarness {
    /// The mock server backing the Kagi endpoints.
    pub server: Arc<MockServer>,
}

impl KagiHarness {
    /// Starts a harness with a fresh mock server.
    #[must_use]
    pub fn start() -> Self {
        Self::with_server(Arc::new(MockServer::start()))
    }

    /// Wraps an existing mock server, so a harness and an application can share one server.
    #[must_use]
    pub const fn with_server(server: Arc<MockServer>) -> Self {
        Self { server }
    }

    /// Builds a client with the default test session settings.
    #[must_use]
    pub fn client(&self) -> Client {
        self.client_with(DEFAULT_MAX_SESSIONS, Duration::from_secs(60))
    }

    /// Builds a client with the given session-pool settings.
    ///
    /// # Panics
    ///
    /// Panics if the test options are invalid; they are compile-time constants.
    #[must_use]
    pub fn client_with(&self, max_sessions: usize, session_duration: Duration) -> Client {
        Client::with_token_and_options(
            TOKEN,
            &ClientOptions {
                base_url: self.server.base_url(),
                max_sessions,
                session_duration,
                timeout: Duration::from_secs(5),
                ..ClientOptions::default()
            },
        )
        .expect("the test client options are valid")
    }

    /// Mocks the token request that stores the session cookie.
    #[must_use]
    pub fn mock_session(&self) -> Mock<'_> {
        self.mock_session_delayed(Duration::ZERO)
    }

    /// Mocks the token request, delaying its response by `delay`.
    #[must_use]
    pub fn mock_session_delayed(&self, delay: Duration) -> Mock<'_> {
        self.server.mock(|when, then| {
            when.method("GET")
                .path("/search")
                .query_param("token", TOKEN)
                .query_param_missing("q")
                .header("x-kagi-authorization", TOKEN)
                .header("accept-language", LANGUAGE)
                .header("user-agent", USER_AGENT)
                .header("accept", DOCUMENT_ACCEPT)
                .header("sec-fetch-dest", "document")
                .header("sec-fetch-mode", "navigate")
                .header("sec-fetch-site", "none")
                .header("upgrade-insecure-requests", "1")
                .header("priority", "u=0, i")
                .header_missing("origin")
                .header_missing("referer");
            then.status(200)
                .delay(delay)
                .header(
                    "set-cookie",
                    format!("kagi_session={SESSION_COOKIE}; Path=/; HttpOnly"),
                )
                .header(
                    "set-cookie",
                    format!("_kagi_search_={SEARCH_COOKIE}; Path=/; HttpOnly"),
                );
        })
    }

    /// Mocks the search document that returns the session nonce.
    #[must_use]
    pub fn mock_nonce(&self) -> Mock<'_> {
        self.mock_nonce_delayed(Duration::ZERO)
    }

    /// Mocks the search document, delaying its response by `delay`.
    #[must_use]
    pub fn mock_nonce_delayed(&self, delay: Duration) -> Mock<'_> {
        let body = nonce_body();
        self.mock_document_nonce("search", QUERY, &body, delay)
    }

    /// Mocks the images document that returns the session nonce.
    #[must_use]
    pub fn mock_images_nonce(&self) -> Mock<'_> {
        let body = nonce_body();
        self.mock_document_nonce("images", QUERY, &body, Duration::ZERO)
    }

    /// Mocks the search document with an arbitrary body.
    #[must_use]
    pub fn mock_nonce_body(&self, body: &str) -> Mock<'_> {
        self.mock_nonce_body_delayed(body, Duration::ZERO)
    }

    /// Mocks the search document with an arbitrary body, delaying its response by `delay`.
    #[must_use]
    fn mock_nonce_body_delayed(&self, body: &str, delay: Duration) -> Mock<'_> {
        self.mock_document_nonce("search", QUERY, body, delay)
    }

    /// Mocks a document request that returns a nonce for the given endpoint and query.
    fn mock_document_nonce(
        &self,
        endpoint: &str,
        query: &str,
        body: &str,
        delay: Duration,
    ) -> Mock<'_> {
        let path = format!("/{endpoint}");
        let query = query.to_owned();
        let referer = format!("{}/", self.server.base_url());

        self.server.mock(move |when, then| {
            when.method("GET")
                .path(path)
                .query_param("q", query)
                .query_param_missing("token")
                .query_param_missing("nonce")
                .header("x-kagi-authorization", TOKEN)
                .header("accept-language", LANGUAGE)
                .header("user-agent", USER_AGENT)
                .header("accept", DOCUMENT_ACCEPT)
                .header("sec-fetch-dest", "document")
                .header("sec-fetch-mode", "navigate")
                .header("sec-fetch-site", "same-origin")
                .header("sec-fetch-user", "?1")
                .header("upgrade-insecure-requests", "1")
                .header("priority", "u=0, i")
                .header("referer", referer)
                .header_missing("origin")
                .cookie("kagi_session", SESSION_COOKIE)
                .cookie("_kagi_search_", SEARCH_COOKIE);
            then.status(200)
                .header("content-type", "text/html; charset=utf-8")
                .delay(delay)
                .body(body);
        })
    }

    /// Mocks a search stream, asserting whether the first-request nonce is present.
    #[must_use]
    pub fn mock_search_stream(&self, nonce: Option<&str>, fixture_name: &str) -> Mock<'_> {
        self.mock_search_stream_for(QUERY, nonce, fixture_name, Duration::ZERO)
    }

    /// Mocks a search stream for `query`, asserting whether the first-request nonce is present.
    #[must_use]
    fn mock_search_stream_for(
        &self,
        query: &str,
        nonce: Option<&str>,
        fixture_name: &str,
        delay: Duration,
    ) -> Mock<'_> {
        self.mock_stream("search", query, Nonce::from(nonce), fixture_name, delay)
    }

    /// Mocks an images stream, asserting whether the first-request nonce is present.
    #[must_use]
    pub fn mock_images_stream(&self, nonce: Option<&str>, fixture_name: &str) -> Mock<'_> {
        self.mock_stream(
            "images",
            QUERY,
            Nonce::from(nonce),
            fixture_name,
            Duration::ZERO,
        )
    }

    /// Mocks a search stream that accepts both the first request (with a nonce) and later
    /// requests that reuse the session without one.
    #[must_use]
    pub fn mock_search_stream_reusable(&self, fixture_name: &str, delay: Duration) -> Mock<'_> {
        self.mock_search_stream_reusable_for(QUERY, fixture_name, delay)
    }

    /// Mocks a reusable search stream for `query`.
    #[must_use]
    pub(crate) fn mock_search_stream_reusable_for(
        &self,
        query: &str,
        fixture_name: &str,
        delay: Duration,
    ) -> Mock<'_> {
        self.mock_stream("search", query, Nonce::Any, fixture_name, delay)
    }

    fn mock_stream(
        &self,
        endpoint: &str,
        query: &str,
        nonce: Nonce<'_>,
        fixture_name: &str,
        delay: Duration,
    ) -> Mock<'_> {
        let path = format!("/socket/{endpoint}");
        let mut referer = url::Url::parse(&format!("{}/{endpoint}", self.server.base_url()))
            .expect("the mock server base URL is valid");
        referer.query_pairs_mut().append_pair("q", query);
        let referer = referer.to_string();
        let query = query.to_owned();
        let body = fixture(fixture_name);

        self.server.mock(move |when, then| {
            let when = when
                .method("GET")
                .path(path)
                .query_param("q", query)
                .query_param_missing("batch")
                .query_param_missing("piece")
                .query_param_missing("page")
                .query_param_missing("all")
                .query_param_missing("r")
                .query_param_missing("sh")
                .header("x-kagi-authorization", TOKEN)
                .header("accept-language", LANGUAGE)
                .header("user-agent", USER_AGENT)
                .header("accept", "text/event-stream")
                // The client enables reqwest's compression features, which also makes it
                // advertise the header a real browser sends.
                .header_count("accept-encoding", ".*", 1)
                .header("referer", referer)
                .header("sec-fetch-dest", "empty")
                .header("sec-fetch-mode", "cors")
                .header("sec-fetch-site", "same-origin")
                .header_missing("sec-fetch-user")
                .header_missing("origin")
                .header("priority", "u=4")
                // The capture was taken with devtools cache disabled; the client mirrors these
                // two headers on stream requests.
                .header("pragma", "no-cache")
                .header("cache-control", "no-cache")
                .cookie("kagi_session", SESSION_COOKIE)
                .cookie("_kagi_search_", SEARCH_COOKIE);
            match nonce {
                Nonce::Present(nonce) => when.query_param("nonce", nonce),
                Nonce::Absent => when.query_param_missing("nonce"),
                Nonce::Any => when,
            };
            then.status(200)
                .header("content-type", "text/event-stream")
                .header("cache-control", "private, no-store")
                .delay(delay)
                .body_from_file(body);
        })
    }
}

/// How a stream mock treats the optional first-request nonce.
enum Nonce<'a> {
    /// The request must carry this nonce.
    Present(&'a str),
    /// The request must not carry a nonce.
    Absent,
    /// Do not constrain the nonce.
    Any,
}

impl<'a> From<Option<&'a str>> for Nonce<'a> {
    fn from(nonce: Option<&'a str>) -> Self {
        nonce.map_or(Self::Absent, Self::Present)
    }
}
