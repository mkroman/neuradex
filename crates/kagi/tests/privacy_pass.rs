//! End-to-end tests for Kagi Privacy Pass authentication.
//!
//! The shared [`PrivacyPassHarness`] runs a real VOPRF issuer behind a local HTTP server. Its
//! responders redeem the `Authorization` header of both the document and stream requests, so a
//! token that is not correctly blinded, unblinded, or bound to the challenge fails the test.

use std::sync::Arc;
use std::time::Duration;

use kagi::SearchResult;
use test_support::httpmock::Mock;
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
    let client = Arc::new(harness.client_with_options(SEARCHES, SEARCHES, 0, SEARCHES));
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
    let client = Arc::new(harness.client_with_options(4, 8, 0, 1));

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
    let client = harness.client_with_options(3, 8, 2, kagi::DEFAULT_MAX_SESSIONS);

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
