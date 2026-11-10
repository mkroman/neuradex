//! A local Kagi Privacy Pass issuer and HTTP harness.
//!
//! The harness implements the batched-token wire format with real VOPRF keys, so tests exercise
//! blinding, proof verification, unblinding, and redemption instead of accepting arbitrary
//! tokens. Its dynamic responders redeem the `Authorization` header of each search request and
//! fail the request when a token is invalid or reused. The issuer and wire codecs live in a
//! private `issuer` submodule.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use httpmock::{HttpMockRequest, HttpMockResponse, Mock, MockServer};
use kagi::{Client, ClientOptions, DEFAULT_MAX_SESSIONS, PrivacyPassOptions};

use self::issuer::TestIssuer;
use crate::kagi::{NONCE, TOKEN};

mod issuer;

/// A local Kagi stand-in that issues and redeems real batched VOPRF tokens.
pub struct PrivacyPassHarness {
    /// The mock server backing the Kagi endpoints.
    pub server: Arc<MockServer>,
    /// The issuer shared by the issuance and stream responders.
    issuer: Arc<TestIssuer>,
    /// How long each search document and stream response is delayed.
    flow_delay: Duration,
}

impl PrivacyPassHarness {
    /// Starts a harness with a fresh mock server and issuer.
    #[must_use]
    pub fn start() -> Self {
        Self::with_server(Arc::new(MockServer::start()))
    }

    /// Wraps an existing mock server, so a harness and an application can share one server.
    #[must_use]
    pub fn with_server(server: Arc<MockServer>) -> Self {
        Self {
            server,
            issuer: Arc::new(TestIssuer::new()),
            flow_delay: Duration::ZERO,
        }
    }

    /// Delays every search document and stream response by `delay`.
    #[must_use]
    pub const fn with_flow_delay(mut self, delay: Duration) -> Self {
        self.flow_delay = delay;

        self
    }

    /// Builds a client with Privacy Pass enabled and the default test batch size.
    ///
    /// # Panics
    ///
    /// Panics if the test options are invalid; they are compile-time constants.
    #[must_use]
    pub fn client(&self) -> Client {
        self.client_with_tokens(4)
    }

    /// Builds a Privacy Pass client with the given issuance batch size.
    ///
    /// # Panics
    ///
    /// Panics if the test client cannot be built.
    #[must_use]
    pub fn client_with_tokens(&self, tokens_to_stash: usize) -> Client {
        self.client_with_options(
            tokens_to_stash,
            tokens_to_stash.max(8),
            0,
            DEFAULT_MAX_SESSIONS,
        )
    }

    /// Builds a Privacy Pass client that persists its unspent tokens at `path`.
    ///
    /// The store holds four tokens from one batch and never starts a background refill, so a
    /// test can consume part of the batch, construct another client from the same path, and
    /// observe that the remaining tokens are reused instead of issued again.
    ///
    /// # Panics
    ///
    /// Panics if the test client cannot be built.
    #[must_use]
    pub fn client_with_state(&self, path: impl Into<PathBuf>) -> Client {
        self.build_client(4, 8, 0, DEFAULT_MAX_SESSIONS, Some(path.into()))
    }

    /// Builds a Privacy Pass client with explicit batch size, capacity, low-water mark, and
    /// maximum simultaneous searches.
    ///
    /// # Panics
    ///
    /// Panics if the test client cannot be built.
    #[must_use]
    pub fn client_with_options(
        &self,
        tokens_to_stash: usize,
        max_tokens: usize,
        low_token_count: usize,
        max_sessions: usize,
    ) -> Client {
        self.build_client(
            tokens_to_stash,
            max_tokens,
            low_token_count,
            max_sessions,
            None,
        )
    }

    /// Builds a client from explicit options and an optional state file.
    fn build_client(
        &self,
        tokens_to_stash: usize,
        max_tokens: usize,
        low_token_count: usize,
        max_sessions: usize,
        state_path: Option<PathBuf>,
    ) -> Client {
        Client::with_token_and_options(
            TOKEN,
            &ClientOptions {
                base_url: self.server.base_url(),
                timeout: Duration::from_secs(5),
                max_sessions,
                privacy_pass: Some(PrivacyPassOptions {
                    tokens_to_stash,
                    max_tokens,
                    low_token_count,
                    state_path,
                }),
                ..ClientOptions::default()
            },
        )
        .expect("the test client options are valid")
    }

    /// Mocks the old session-token document request and fails it if it is reached.
    ///
    /// Privacy Pass mode must never request `/search?token=...`; the mock turns that path into
    /// a failing response so a regression cannot silently fall back to sessions.
    #[must_use]
    pub fn mock_session_path_forbidden(&self) -> Mock<'_> {
        self.server.mock(|when, then| {
            when.method("GET")
                .path("/search")
                .query_param("token", TOKEN);
            then.status(500);
        })
    }

    /// Mocks the challenge endpoint.
    #[must_use]
    pub fn mock_challenge(&self) -> Mock<'_> {
        let header = self.issuer.challenge_header.clone();

        self.server.mock(move |when, then| {
            when.method("GET")
                .path("/pp/wwwa")
                .header("x-kagi-privacypass-client", "true")
                .header_missing("authorization")
                .header_missing("cookie");
            then.status(200)
                .header("www-authenticate", header)
                .header("content-type", "text/plain");
        })
    }

    /// Mocks the token issuance endpoint with a real VOPRF responder.
    #[must_use]
    pub fn mock_issuance(&self) -> Mock<'_> {
        let issuer = Arc::clone(&self.issuer);

        self.server.mock(move |when, then| {
            when.method("POST")
                .path("/pp/gettokens")
                .header("content-type", "application/private-token-request")
                .header("authorization", TOKEN)
                .header("x-kagi-privacypass-client", "true")
                .header_missing("cookie");
            then.respond_with(move |request: &HttpMockRequest| {
                let body = request.body().to_maybe_lossy_str();

                match issuer.issue(body.as_ref()) {
                    Ok(body) => HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "text/plain")
                        .body(body)
                        .build(),
                    Err(error) => HttpMockResponse::builder().status(500).body(error).build(),
                }
            });
        })
    }

    /// Mocks the search document and stream, redeeming a token for each request.
    #[must_use]
    pub fn mock_search_flow(&self, query: &str, fixture: &str) -> (Mock<'_>, Mock<'_>) {
        self.mock_flow("search", query, fixture, 0, 0)
    }

    /// Mocks a search flow that rejects the first `document_rejections` document requests and
    /// the first `stream_rejections` stream requests with 401.
    #[must_use]
    pub fn mock_search_flow_with_rejections(
        &self,
        query: &str,
        fixture: &str,
        document_rejections: usize,
        stream_rejections: usize,
    ) -> (Mock<'_>, Mock<'_>) {
        self.mock_flow(
            "search",
            query,
            fixture,
            document_rejections,
            stream_rejections,
        )
    }

    /// Mocks the images document and stream, redeeming a token for each request.
    #[must_use]
    pub fn mock_images_flow(&self, query: &str, fixture: &str) -> (Mock<'_>, Mock<'_>) {
        self.mock_flow("images", query, fixture, 0, 0)
    }

    /// Mocks a document and its socket stream for `endpoint`.
    fn mock_flow(
        &self,
        endpoint: &str,
        query: &str,
        fixture: &str,
        document_rejections: usize,
        stream_rejections: usize,
    ) -> (Mock<'_>, Mock<'_>) {
        let document_query = query.to_owned();
        let stream_query = query.to_owned();
        let document_path = format!("/{endpoint}");
        let stream_path = format!("/socket/{endpoint}");
        let delay = self.flow_delay;
        let document_responder = rejecting_response(
            document_rejections,
            authenticated_response(
                Arc::clone(&self.issuer),
                crate::kagi::nonce_body(),
                "text/html; charset=utf-8",
            ),
        );
        let stream_responder = rejecting_response(
            stream_rejections,
            authenticated_response(
                Arc::clone(&self.issuer),
                crate::kagi::read_fixture(fixture),
                "text/event-stream",
            ),
        );

        let document = self.server.mock(move |when, then| {
            when.method("GET")
                .path(document_path)
                .query_param("q", &document_query)
                .query_param_missing("token")
                .header("x-kagi-privacypass-client", "true")
                .header_matches("authorization", "^PrivateToken token=\".+\"$")
                .header_excludes("authorization", TOKEN)
                .header_missing("cookie")
                .header_missing("x-kagi-authorization")
                .header_missing("referer")
                .header_count("sec-fetch-site", ".*", 1);
            then.delay(delay).respond_with(document_responder);
        });

        let stream = self.server.mock(move |when, then| {
            when.method("GET")
                .path(stream_path)
                .query_param("q", &stream_query)
                .query_param("nonce", NONCE)
                .query_param_missing("token")
                .header("x-kagi-privacypass-client", "true")
                .header_matches("authorization", "^PrivateToken token=\".+\"$")
                .header_excludes("authorization", TOKEN)
                .header_missing("cookie")
                .header_missing("x-kagi-authorization")
                .header_missing("referer")
                .header("sec-fetch-site", "none")
                .header_count("sec-fetch-site", ".*", 1);
            then.delay(delay).respond_with(stream_responder);
        });

        (document, stream)
    }
}

/// Returns a responder that redeems the request header before serving `body`.
fn authenticated_response(
    issuer: Arc<TestIssuer>,
    body: String,
    content_type: &'static str,
) -> impl Fn(&HttpMockRequest) -> HttpMockResponse + Send + Sync + 'static {
    move |request| {
        if issuer.redeem_request(request) {
            HttpMockResponse::builder()
                .status(200)
                .header("content-type", content_type)
                .body(body.clone())
                .build()
        } else {
            HttpMockResponse::builder().status(401).build()
        }
    }
}

/// Returns a responder that rejects the first `rejections` requests with 401, then delegates.
fn rejecting_response(
    rejections: usize,
    inner: impl Fn(&HttpMockRequest) -> HttpMockResponse + Send + Sync + 'static,
) -> impl Fn(&HttpMockRequest) -> HttpMockResponse + Send + Sync + 'static {
    let remaining = AtomicUsize::new(rejections);

    move |request| {
        let rejected = remaining
            .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();

        if rejected {
            HttpMockResponse::builder().status(401).build()
        } else {
            inner(request)
        }
    }
}
