//! End-to-end test of the service searching through Privacy Pass.
//!
//! The production router is wired to a Privacy Pass client backed by the shared real-VOPRF test
//! issuer. Its dynamic responders redeem every document and stream token, and the session-path
//! mock fails if the service falls back to the old authenticated search flow.

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;
use test_support::app::TestApp;
use test_support::httpmock::MockServer;
use test_support::privacy_pass::PrivacyPassHarness;

/// The query used by the stream fixture.
const QUERY: &str = "hello world";
/// The fixture containing a complete SSE search response.
const FIXTURE: &str = "search_stream_sse.bin";

#[tokio::test]
async fn service_search_uses_privacy_pass_tokens() {
    let server = Arc::new(MockServer::start());
    let harness = PrivacyPassHarness::with_server(Arc::clone(&server));
    let forbidden = harness.mock_session_path_forbidden();
    let challenge = harness.mock_challenge();
    let issuance = harness.mock_issuance();
    let (document, stream) = harness.mock_search_flow(QUERY, FIXTURE);
    let app = TestApp::with_kagi_client(Arc::clone(&server), harness.client());

    let response = app
        .post_json("/v1/search", &json!({"query": "hello world"}))
        .await;

    assert_eq!(response.status(), StatusCode::OK);

    let body = TestApp::json(response).await;

    assert_eq!(body["results"].as_array().map(Vec::len), Some(5));
    assert_eq!(body["results"][0]["title"], "Hello, world - Wikipedia");

    forbidden.assert_calls(0);
    challenge.assert();
    issuance.assert();
    document.assert();
    stream.assert();
}
