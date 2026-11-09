//! Integration tests for [`RequestError`] rendering against real HTTP failures.

use kagi::RequestError;
use reqwest::StatusCode;
use test_support::httpmock::MockServer;

/// The credential-looking query parameter placed in every test URL.
const QUERY_SECRET: &str = "query-secret";
/// The credential-looking password placed in the refused URL's userinfo.
const PASSWORD_SECRET: &str = "password-secret";
/// The credential-looking fragment placed in the refused URL.
const FRAGMENT_SECRET: &str = "fragment-secret";

/// Returns a URL whose connection is always refused: TCP port 0 is reserved and can never have a
/// listener.
fn refused_url() -> String {
    format!(
        "http://user:{PASSWORD_SECRET}@127.0.0.1:0/path?login_token={QUERY_SECRET}#\
         {FRAGMENT_SECRET}"
    )
}

/// Asserts that no rendering leaks one of the test credentials.
fn assert_no_secrets(rendered: &str) {
    for secret in [QUERY_SECRET, PASSWORD_SECRET, FRAGMENT_SECRET] {
        assert!(!rendered.contains(secret), "{rendered}");
    }
}

/// The wrapper's guarantees hold for a real connection failure: no rendering leaks the query
/// string, the short form is classified and host-only, the full form carries the redacted URL and
/// the cause, and the chain ends at the wrapper.
#[tokio::test]
async fn request_errors_redact_the_url_but_keep_the_cause() {
    let error = reqwest::Client::new()
        .get(refused_url())
        .send()
        .await
        .expect_err("port zero has no listener");

    // reqwest attaches the request URL, credentials included, to the error.
    let error = RequestError::from(error);
    let message = error.to_string();
    let full = error.full();
    let debug = format!("{error:?}");

    for rendered in [message.as_str(), full, debug.as_str()] {
        assert_no_secrets(rendered);
    }

    assert_eq!(error.url(), Some("http://127.0.0.1:0/path"));
    assert!(full.contains("http://127.0.0.1:0/path"), "{full}");
    assert_eq!(error.error_type(), "connection_error");
    assert!(std::error::Error::source(&error).is_none(), "{debug}");
}

/// A response status is preserved for callers that invalidate a session on it, while the URL
/// remains redacted.
#[tokio::test]
async fn request_errors_keep_the_response_status() {
    let server = MockServer::start();
    let response = server.mock(|when, then| {
        when.method("GET").path("/status");
        then.status(StatusCode::UNAUTHORIZED.as_u16());
    });
    let url = format!("{}/status?token={QUERY_SECRET}", server.base_url());

    let error = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .expect("the mock server responds")
        .error_for_status()
        .expect_err("the status is an error");
    let error = RequestError::from(error);
    let full = error.full();
    let debug = format!("{error:?}");

    assert_eq!(error.status(), Some(StatusCode::UNAUTHORIZED));
    assert_eq!(error.error_type(), "other");
    assert_eq!(
        error.url(),
        Some(format!("{}/status", server.base_url()).as_str())
    );
    assert_no_secrets(&error.to_string());
    assert_no_secrets(full);
    assert_no_secrets(&debug);
    response.assert();
}
