//! End-to-end tests for Kagi Privacy Pass authentication.
//!
//! The shared [`PrivacyPassHarness`] runs a real VOPRF issuer behind a local HTTP server. Its
//! responders redeem the `Authorization` header of both the document and stream requests, so a
//! token that is not correctly blinded, unblinded, or bound to the challenge fails the test.

use std::sync::Arc;
use std::time::Duration;

use kagi::{Error, PrivacyPassError, SearchResult};
use test_support::httpmock::Mock;
use test_support::kagi::KagiHarness;
use test_support::privacy_pass::PrivacyPassHarness;
use tokio::time::{sleep, timeout};

/// The query used by the stream fixtures.
const QUERY: &str = "hello world";
/// The fixture containing a complete SSE search response.
const SEARCH_FIXTURE: &str = "search_stream_sse.bin";
/// The fixture containing a complete SSE image response.
const IMAGE_FIXTURE: &str = "images_stream_sse.bin";

/// The mocks that make up one Privacy Pass search or image flow.
struct Flow<'a> {
    /// Fails the test if the old session-token path is requested.
    forbidden: Mock<'a>,
    /// Serves the issuer challenge.
    challenge: Mock<'a>,
    /// Serves token issuance.
    issuance: Mock<'a>,
    /// Serves the target document.
    document: Mock<'a>,
    /// Serves the target stream.
    stream: Mock<'a>,
}

impl<'a> Flow<'a> {
    /// Mocks a web search flow.
    fn search(harness: &'a PrivacyPassHarness) -> Self {
        Self::new(harness, harness.mock_search_flow(QUERY, SEARCH_FIXTURE))
    }

    /// Mocks an image search flow.
    fn images(harness: &'a PrivacyPassHarness) -> Self {
        Self::new(harness, harness.mock_images_flow(QUERY, IMAGE_FIXTURE))
    }

    /// Builds a flow from the document and stream mocks.
    fn new(harness: &'a PrivacyPassHarness, (document, stream): (Mock<'a>, Mock<'a>)) -> Self {
        Self {
            forbidden: harness.mock_session_path_forbidden(),
            challenge: harness.mock_challenge(),
            issuance: harness.mock_issuance(),
            document,
            stream,
        }
    }

    /// Asserts that every request succeeded and issuance ran exactly `batches` times.
    fn assert_batches(&self, batches: usize) {
        self.forbidden.assert_calls(0);
        self.challenge.assert_calls(batches);
        self.issuance.assert_calls(batches);
        self.document.assert();
        self.stream.assert();
    }
}

#[tokio::test]
async fn search_issues_and_redeems_privacy_pass_tokens() {
    let harness = PrivacyPassHarness::start();
    let flow = Flow::search(&harness);

    let results = harness
        .client()
        .search(QUERY)
        .await
        .expect("the privacy pass search succeeds");

    assert_eq!(results.len(), 5);
    assert_eq!(
        results[0],
        SearchResult {
            title: "Hello, world - Wikipedia".to_owned(),
            url: "https://en.wikipedia.org/wiki/Hello,_world".to_owned(),
            description: "\"Time to hello world\" (TTHW) is the time it takes to author a \
                          \"Hello, world\" program in a given programming language. This is one \
                          measure of a programming language's ease of use."
                .to_owned(),
            rank: 1,
            source: "en.wikipedia.org".to_owned(),
        }
    );

    flow.assert_batches(1);
}

#[tokio::test]
async fn image_search_issues_and_redeems_privacy_pass_tokens() {
    let harness = PrivacyPassHarness::start();
    let flow = Flow::images(&harness);

    let results = harness
        .client()
        .images(QUERY)
        .await
        .expect("the privacy pass image search succeeds");

    assert_eq!(results.len(), 5);
    assert_eq!(
        results[0].title,
        "How to Do a Reverse Image Search From Your Phone"
    );

    flow.assert_batches(1);
}

#[tokio::test]
async fn search_issues_another_batch_when_the_store_runs_out() {
    let harness = PrivacyPassHarness::start();
    let flow = Flow::search(&harness);

    let results = harness
        .client_with_tokens(1)
        .search(QUERY)
        .await
        .expect("the privacy pass search succeeds");

    assert_eq!(results.len(), 5);

    // A batch of one cannot cover the document and the stream, so issuance runs twice.
    flow.assert_batches(2);
}

#[tokio::test]
async fn concurrent_searches_each_redeem_their_own_tokens() {
    const SEARCHES: usize = 16;

    let harness = PrivacyPassHarness::start();
    let flow = Flow::search(&harness);
    // Each search redeems one token for the document and one for the stream. A batch covers half
    // of them, so the second batch is issued on demand. The session limit matches the search
    // count, so the search gate does not serialize the redemptions and mask a concurrent issuance
    // race.
    let client = Arc::new(harness.client_with_options(SEARCHES, SEARCHES, 0, SEARCHES, false));
    let mut tasks = Vec::with_capacity(SEARCHES);

    for _ in 0..SEARCHES {
        let client = Arc::clone(&client);

        tasks.push(tokio::spawn(async move { client.search(QUERY).await }));
    }

    for task in tasks {
        let results = task
            .await
            .expect("the search task joins")
            .expect("the privacy pass search succeeds");

        assert_eq!(results.len(), 5);
    }

    flow.forbidden.assert_calls(0);
    flow.challenge.assert_calls(2);
    flow.issuance.assert_calls(2);
    flow.document.assert_calls(SEARCHES);
    flow.stream.assert_calls(SEARCHES);
}

#[tokio::test]
async fn search_retries_when_the_document_is_rejected() {
    let harness = PrivacyPassHarness::start();
    let forbidden = harness.mock_session_path_forbidden();
    let challenge = harness.mock_challenge();
    let issuance = harness.mock_issuance();
    let (document, stream) = harness.mock_search_flow_with_rejections(QUERY, SEARCH_FIXTURE, 1, 0);

    let results = harness
        .client()
        .search(QUERY)
        .await
        .expect("the retried privacy pass search succeeds");

    assert_eq!(results.len(), 5);
    forbidden.assert_calls(0);
    challenge.assert_calls(2);
    issuance.assert_calls(2);
    document.assert_calls(2);
    stream.assert_calls(1);
}

#[tokio::test]
async fn search_retries_when_the_stream_is_rejected() {
    let harness = PrivacyPassHarness::start();
    let forbidden = harness.mock_session_path_forbidden();
    let challenge = harness.mock_challenge();
    let issuance = harness.mock_issuance();
    let (document, stream) = harness.mock_search_flow_with_rejections(QUERY, SEARCH_FIXTURE, 0, 1);

    let results = harness
        .client()
        .search(QUERY)
        .await
        .expect("the retried privacy pass search succeeds");

    assert_eq!(results.len(), 5);
    forbidden.assert_calls(0);
    challenge.assert_calls(2);
    issuance.assert_calls(2);
    document.assert_calls(2);
    stream.assert_calls(2);
}

#[tokio::test]
async fn privacy_pass_searches_are_limited_by_max_sessions() {
    let harness = PrivacyPassHarness::start().with_flow_delay(Duration::from_millis(500));
    let flow = Flow::search(&harness);
    let client = Arc::new(harness.client_with_options(4, 8, 0, 1, false));

    let first = tokio::spawn({
        let client = Arc::clone(&client);

        async move { client.search(QUERY).await }
    });
    let second = tokio::spawn({
        let client = Arc::clone(&client);

        async move { client.search(QUERY).await }
    });

    timeout(Duration::from_secs(2), async {
        while flow.document.calls() == 0 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the first search starts");

    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        flow.document.calls(),
        1,
        "the second search must wait for the first to finish"
    );

    let (first, second) = tokio::join!(first, second);

    assert_eq!(
        first
            .expect("the first task joins")
            .expect("the first search succeeds")
            .len(),
        5
    );
    assert_eq!(
        second
            .expect("the second task joins")
            .expect("the second search succeeds")
            .len(),
        5
    );

    flow.forbidden.assert_calls(0);
    flow.challenge.assert_calls(1);
    flow.issuance.assert_calls(1);
    flow.document.assert_calls(2);
    flow.stream.assert_calls(2);
}

#[tokio::test]
async fn the_low_water_mark_starts_a_background_refill() {
    let harness = PrivacyPassHarness::start();
    let flow = Flow::search(&harness);
    // The first search leaves one of the three issued tokens; taking it drops the store below
    // the low-water mark of two and starts the background refill.
    let client = harness.client_with_options(3, 8, 2, kagi::DEFAULT_MAX_SESSIONS, false);

    let results = client
        .search(QUERY)
        .await
        .expect("the privacy pass search succeeds");

    assert_eq!(results.len(), 5);

    let privacy_pass = client.privacy_pass().expect("privacy pass is enabled");

    timeout(Duration::from_secs(5), async {
        while flow.challenge.calls() < 2 || privacy_pass.store().len() < 4 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the background refill completes");

    flow.assert_batches(2);
}

#[tokio::test]
async fn a_new_client_reuses_persisted_tokens_instead_of_issuing_again() {
    let harness = PrivacyPassHarness::start();
    let flow = Flow::search(&harness);
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("neuradex/privacy-pass/tokens.json");

    let first = harness.client_with_state(&path);
    let results = first
        .search(QUERY)
        .await
        .expect("the first privacy pass search succeeds");

    assert_eq!(results.len(), 5);
    assert_eq!(
        first
            .privacy_pass()
            .expect("privacy pass is enabled")
            .store()
            .len(),
        2,
        "the first batch of four leaves two unspent tokens"
    );
    drop(first);

    let second = harness.client_with_state(&path);

    assert_eq!(
        second
            .privacy_pass()
            .expect("privacy pass is enabled")
            .store()
            .len(),
        2,
        "the second client loads the unspent tokens"
    );

    let results = second
        .search(QUERY)
        .await
        .expect("the second privacy pass search succeeds");

    assert_eq!(results.len(), 5);

    flow.forbidden.assert_calls(0);
    flow.challenge.assert_calls(1);
    flow.issuance.assert_calls(1);
    flow.document.assert_calls(2);
    flow.stream.assert_calls(2);
}
/// The session-mode mocks a fallback search uses instead of Privacy Pass.
///
/// The mock matchers of the two harnesses are disjoint — the Privacy Pass responders require
/// the `PrivateToken` authorization header and the `X-Kagi-PrivacyPass-Client` marker, the
/// session responders require the session cookies and `X-Kagi-Authorization` — so both can be
/// registered on one server without shadowing each other.
struct SessionFlow<'a> {
    /// Serves the session cookie for the login token.
    session: Mock<'a>,
    /// Serves the document nonce.
    nonce: Mock<'a>,
    /// Serves the stream, accepting both a first-request nonce and session reuse.
    stream: Mock<'a>,
}

impl<'a> SessionFlow<'a> {
    /// Mocks the session flow on the Privacy Pass harness's server.
    ///
    /// The harness must outlive the returned mocks.
    fn new(sessions: &'a KagiHarness) -> Self {
        Self {
            session: sessions.mock_session(),
            nonce: sessions.mock_nonce(),
            stream: sessions.mock_search_stream_reusable(SEARCH_FIXTURE, Duration::ZERO),
        }
    }

    /// Asserts that no session request was made.
    fn assert_unused(&self) {
        self.session.assert_calls(0);
        self.nonce.assert_calls(0);
        self.stream.assert_calls(0);
    }
}

#[tokio::test]
async fn quota_exhaustion_falls_back_to_the_session_flow_and_degrades() {
    let harness = PrivacyPassHarness::start();
    let challenge = harness.mock_challenge();
    let issuance = harness.mock_issuance_with_status(429);
    let sessions = KagiHarness::with_server(Arc::clone(&harness.server));
    let flow = SessionFlow::new(&sessions);
    let client = harness.client_with_options(4, 8, 0, kagi::DEFAULT_MAX_SESSIONS, true);
    let privacy_pass = client.privacy_pass().expect("privacy pass is enabled");

    let results = client
        .search(QUERY)
        .await
        .expect("the search falls back to the session flow");

    assert_eq!(results.len(), 5);
    assert!(
        privacy_pass.degraded(),
        "quota exhaustion degrades privacy pass"
    );

    // A degraded search skips issuance entirely instead of repeating the rejected request.
    client
        .search(QUERY)
        .await
        .expect("the degraded search uses the session flow");

    challenge.assert_calls(1);
    issuance.assert_calls(1);
    // The second search reuses the parked session, so only the stream runs again: the nonce
    // document is fetched when a session is established, not on reuse.
    flow.session.assert_calls(1);
    flow.nonce.assert_calls(1);
    flow.stream.assert_calls(2);
}

#[tokio::test]
async fn subscription_rejection_falls_back_and_degrades_for_the_client_lifetime() {
    let harness = PrivacyPassHarness::start();
    let challenge = harness.mock_challenge();
    let issuance = harness.mock_issuance_with_status(403);
    let sessions = KagiHarness::with_server(Arc::clone(&harness.server));
    let flow = SessionFlow::new(&sessions);
    let client = harness.client_with_options(4, 8, 0, kagi::DEFAULT_MAX_SESSIONS, true);
    let privacy_pass = client.privacy_pass().expect("privacy pass is enabled");

    let results = client
        .search(QUERY)
        .await
        .expect("the search falls back to the session flow");

    assert_eq!(results.len(), 5);
    assert!(
        privacy_pass.degraded(),
        "a subscription rejection degrades privacy pass"
    );

    challenge.assert_calls(1);
    issuance.assert_calls(1);
    flow.nonce.assert_calls(1);
    flow.stream.assert_calls(1);
}

#[tokio::test]
async fn without_the_fallback_the_quota_error_propagates() {
    let harness = PrivacyPassHarness::start();
    let challenge = harness.mock_challenge();
    let issuance = harness.mock_issuance_with_status(429);
    let sessions = KagiHarness::with_server(Arc::clone(&harness.server));
    let flow = SessionFlow::new(&sessions);
    let client = harness.client_with_options(4, 8, 0, kagi::DEFAULT_MAX_SESSIONS, false);
    let privacy_pass = client.privacy_pass().expect("privacy pass is enabled");

    let error = client
        .search(QUERY)
        .await
        .expect_err("the quota error propagates without a fallback");
    let Error::PrivacyPass(PrivacyPassError::TokenStatus(error)) = &error else {
        panic!("expected a Privacy Pass token status error, got {error:?}");
    };

    assert_eq!(error.status(), Some(reqwest::StatusCode::TOO_MANY_REQUESTS));
    assert!(
        !privacy_pass.degraded(),
        "no degradation without a fallback"
    );

    challenge.assert_calls(1);
    issuance.assert_calls(1);
    flow.assert_unused();
}

#[tokio::test]
async fn rejected_redemptions_fall_back_to_the_session_flow() {
    let harness = PrivacyPassHarness::start();
    let challenge = harness.mock_challenge();
    let issuance = harness.mock_issuance();
    let (document, stream) = harness.mock_search_flow_with_rejections(QUERY, SEARCH_FIXTURE, 1, 1);
    let sessions = KagiHarness::with_server(Arc::clone(&harness.server));
    let flow = SessionFlow::new(&sessions);
    let client = harness.client_with_options(4, 8, 0, kagi::DEFAULT_MAX_SESSIONS, true);
    let privacy_pass = client.privacy_pass().expect("privacy pass is enabled");

    let results = client
        .search(QUERY)
        .await
        .expect("the search falls back to the session flow");

    assert_eq!(results.len(), 5);
    assert!(
        !privacy_pass.degraded(),
        "a rejected redemption does not degrade privacy pass"
    );

    // The next search redeems tokens again: the fallback only covered the failed search.
    client
        .search(QUERY)
        .await
        .expect("the next search uses privacy pass again");

    // The first search rejected its document and its stream, issued a retry batch, and then
    // fell back; the second redeemed the tokens the retry batch had left.
    challenge.assert_calls(2);
    issuance.assert_calls(2);
    document.assert_calls(3);
    stream.assert_calls(2);
    flow.session.assert_calls(1);
    flow.nonce.assert_calls(1);
    flow.stream.assert_calls(1);
}
