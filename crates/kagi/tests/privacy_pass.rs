//! End-to-end tests for Kagi Privacy Pass authentication.
//!
//! The shared [`PrivacyPassHarness`] runs a real VOPRF issuer behind a local HTTP server. Its
//! responders redeem the `Authorization` header of both the document and stream requests, so a
//! token that is not correctly blinded, unblinded, or bound to the challenge fails the test.

use kagi::SearchResult;
use test_support::httpmock::Mock;
use test_support::privacy_pass::PrivacyPassHarness;

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
