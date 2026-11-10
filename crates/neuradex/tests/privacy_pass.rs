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

#[tokio::test]
async fn service_reuses_persisted_tokens_after_a_restart() {
    let server = Arc::new(MockServer::start());
    let harness = PrivacyPassHarness::with_server(Arc::clone(&server));
    let forbidden = harness.mock_session_path_forbidden();
    let challenge = harness.mock_challenge();
    let issuance = harness.mock_issuance();
    let (document, stream) = harness.mock_search_flow(QUERY, FIXTURE);
    let directory = tempfile::tempdir().expect("a temporary directory");
    let state = directory.path().join("neuradex/privacy-pass/tokens.json");

    let app = TestApp::with_kagi_client(
        Arc::clone(&server),
        harness.client_with_state(state.clone()),
    );
    let response = app
        .post_json("/v1/search", &json!({"query": "hello world"}))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    drop(app);

    // A restart builds a new Kagi client and AppState from the same state file. The first
    // search reused two of the four issued tokens; the second search must reuse the other two
    // instead of calling the issuer again.
    let app = TestApp::with_kagi_client(Arc::clone(&server), harness.client_with_state(state));
    let response = app
        .post_json("/v1/search", &json!({"query": "hello world"}))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = TestApp::json(response).await;

    assert_eq!(body["results"].as_array().map(Vec::len), Some(5));

    forbidden.assert_calls(0);
    challenge.assert_calls(1);
    issuance.assert_calls(1);
    document.assert_calls(2);
    stream.assert_calls(2);
}
