//! End-to-end tests for the Kagi client against a real, local HTTP server.
//!
//! The shared [`KagiHarness`] serves the session, nonce, and stream endpoints, so these tests
//! exercise the client's real request construction, cookie jar, session pool, and response
//! parsing without touching the network.

use std::assert_matches;
use std::sync::Arc;
use std::time::Duration;

use kagi::{Error, SearchResult};
use test_support::kagi as support;
use test_support::kagi::{KagiHarness, NONCE, QUERY, TOKEN};

#[tokio::test]
async fn search_fetches_a_session_and_parses_the_results() {
    let harness = KagiHarness::start();
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let stream = harness.mock_search_stream(Some(NONCE), "search_stream_sse.bin");

    let results = harness
        .client()
        .search(QUERY)
        .await
        .expect("the search succeeds");

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

    session.assert();
    nonce.assert();
    stream.assert();
}

#[tokio::test]
async fn images_fetch_a_session_and_parse_the_results() {
    let harness = KagiHarness::start();
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let stream = harness.mock_images_stream(Some(NONCE), "images_stream_sse.bin");

    let results = harness
        .client()
        .images(QUERY)
        .await
        .expect("the image search succeeds");

    assert_eq!(results.len(), 5);

    let result = &results[0];
    assert_eq!(
        result.title,
        "How to Do a Reverse Image Search From Your Phone"
    );
    assert_eq!(
        result.page_url,
        "https://www.entrepreneur.com/business-news/how-to-do-a-reverse-image-search-from-your-phone/297541"
    );
    assert_eq!(
        result.image_url,
        "https://assets.entrepreneur.com/images/misc/1500561136_1.jpg"
    );
    assert!(
        result
            .thumbnail_url
            .starts_with("https://p.kagi.com/proxy/")
    );
    assert_eq!(result.width, 740);
    assert_eq!(result.height, 475);
    assert_eq!(result.host, "www.entrepreneur.com");
    assert_eq!(result.rank, 0);

    session.assert();
    nonce.assert();
    stream.assert();
}

#[tokio::test]
async fn sequential_searches_reuse_the_session_and_send_the_nonce_once() {
    let harness = KagiHarness::start();
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let first = harness.mock_search_stream(Some(NONCE), "search_stream_sse.bin");
    let second = harness.mock_search_stream(None, "search_stream_sse.bin");

    let client = harness.client();
    client
        .search(QUERY)
        .await
        .expect("the first search succeeds");
    client
        .search(QUERY)
        .await
        .expect("the second search succeeds");

    session.assert_calls(1);
    nonce.assert_calls(1);
    first.assert();
    second.assert();
}

#[tokio::test]
async fn an_expired_session_is_refreshed() {
    let harness = KagiHarness::start();
    let client = harness.client_with(1, Duration::from_millis(50));
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let stream = harness.mock_search_stream_reusable("search_stream_sse.bin", Duration::ZERO);

    client
        .search(QUERY)
        .await
        .expect("the first search succeeds");
    tokio::time::sleep(Duration::from_millis(60)).await;
    client
        .search(QUERY)
        .await
        .expect("the second search succeeds");

    session.assert_calls(2);
    nonce.assert_calls(2);
    stream.assert_calls(2);
}

#[tokio::test]
async fn concurrent_searches_refresh_an_expired_session_once() {
    let harness = KagiHarness::start();
    let client = harness.client_with(1, Duration::from_millis(200));
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let stream =
        harness.mock_search_stream_reusable("search_stream_sse.bin", Duration::from_millis(10));

    client
        .search(QUERY)
        .await
        .expect("the first search succeeds");
    tokio::time::sleep(Duration::from_millis(220)).await;

    let (a, b) = tokio::join!(client.search(QUERY), client.search(QUERY));

    a.expect("the first concurrent search succeeds");
    b.expect("the second concurrent search succeeds");

    // Both requests race on the one expired session: it is refreshed once and the
    // other request reuses the result.
    session.assert_calls(2);
    nonce.assert_calls(2);
    stream.assert_calls(3);
}

#[tokio::test]
async fn a_rejected_session_is_reestablished() {
    let harness = KagiHarness::start();
    let client = harness.client_with(1, Duration::from_secs(60));
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let rejected = harness.server.mock(|when, then| {
        when.method("GET")
            .path("/socket/search")
            .query_param("q", QUERY);
        then.status(401).up_to_n_times(1);
    });
    let stream = harness.mock_search_stream(Some(NONCE), "search_stream_sse.bin");

    let error = client
        .search(QUERY)
        .await
        .expect_err("the first search is rejected");
    let redacted = format!("{}/socket/search", harness.server.base_url());
    assert_matches!(
        error,
        Error::StreamStatus(ref error)
            if error.status() == Some(reqwest::StatusCode::UNAUTHORIZED)
                && error.url() == Some(redacted.as_str())
    );

    client
        .search(QUERY)
        .await
        .expect("the retry establishes a fresh session");

    rejected.assert();
    stream.assert();
    session.assert_calls(2);
    nonce.assert_calls(2);
}

#[tokio::test]
async fn a_non_credential_stream_error_keeps_the_session() {
    let harness = KagiHarness::start();
    let client = harness.client();
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let server_error = harness.server.mock(|when, then| {
        when.method("GET")
            .path("/socket/search")
            .query_param("q", QUERY)
            .query_param("nonce", NONCE);
        then.status(500).up_to_n_times(1);
    });
    let stream = harness.mock_search_stream(None, "search_stream_sse.bin");

    let error = client
        .search(QUERY)
        .await
        .expect_err("the first search fails upstream");
    assert_matches!(
        error,
        Error::StreamStatus(ref error)
            if error.status() == Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR)
    );

    client
        .search(QUERY)
        .await
        .expect("the second search reuses the session");

    server_error.assert();
    stream.assert();
    session.assert_calls(1);
    nonce.assert_calls(1);
}

#[tokio::test]
async fn a_failed_session_fetch_is_retried_after_backoff() {
    let harness = KagiHarness::start();
    let client = harness.client_with(1, Duration::from_secs(60));
    let failed = harness.server.mock(|when, then| {
        when.method("GET")
            .path("/search")
            .query_param("token", TOKEN);
        then.status(200).up_to_n_times(1);
    });
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let stream = harness.mock_search_stream(Some(NONCE), "search_stream_sse.bin");

    let error = client
        .search(QUERY)
        .await
        .expect_err("the first fetch returns no session cookie");
    assert_matches!(error, Error::SessionCookies);

    let results = client
        .search(QUERY)
        .await
        .expect("the retry succeeds after the backoff");

    assert_eq!(results.len(), 5);
    failed.assert();
    session.assert();
    nonce.assert();
    stream.assert();
}

#[tokio::test]
async fn a_missing_nonce_is_an_error() {
    let harness = KagiHarness::start();
    let session = harness.mock_session();
    let _nonce = harness.mock_nonce_body("<html><head></head><body></body></html>");

    let error = harness
        .client()
        .search(QUERY)
        .await
        .expect_err("the session has no nonce");

    assert_matches!(error, Error::Nonce);
    session.assert();
}

#[tokio::test]
async fn an_inrush_grows_the_pool_to_its_capacity() {
    let harness = KagiHarness::start();
    let client = harness.client_with(2, Duration::from_secs(60));
    let session = harness.mock_session_delayed(Duration::from_millis(20));
    let nonce = harness.mock_nonce();
    let stream =
        harness.mock_search_stream_reusable("search_stream_sse.bin", Duration::from_millis(200));

    let (a, b, c, d) = tokio::join!(
        client.search(QUERY),
        client.search(QUERY),
        client.search(QUERY),
        client.search(QUERY),
    );

    a.expect("the first search succeeds");
    b.expect("the second search succeeds");
    c.expect("the third search succeeds");
    d.expect("the fourth search succeeds");

    // Four concurrent searches grow the pool to two sessions, one establishment at a time,
    // and then reuse them for the remaining requests.
    session.assert_calls(2);
    nonce.assert_calls(2);
    stream.assert_calls(4);
}

#[tokio::test]
async fn session_fetches_are_serialized() {
    let harness = KagiHarness::start();
    let client = Arc::new(harness.client_with(2, Duration::from_secs(60)));
    let session = harness.mock_session();
    let nonce = harness.mock_nonce_delayed(Duration::from_millis(200));
    let stream = harness.mock_search_stream_reusable("search_stream_sse.bin", Duration::ZERO);

    let first = tokio::spawn({
        let client = Arc::clone(&client);

        async move { client.search(QUERY).await }
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        while session.calls() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the first session fetch starts");

    let second = tokio::spawn({
        let client = Arc::clone(&client);

        async move { client.search(QUERY).await }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        session.calls(),
        1,
        "a second request waits for the in-flight creation"
    );

    first
        .await
        .expect("the first task does not panic")
        .expect("the first search succeeds");
    second
        .await
        .expect("the second task does not panic")
        .expect("the second search succeeds");

    session.assert_calls(2);
    nonce.assert_calls(2);
    stream.assert_calls(2);
}

#[tokio::test]
async fn a_cancelled_search_releases_the_session() {
    let harness = KagiHarness::start();
    let client = Arc::new(harness.client_with(1, Duration::from_secs(60)));
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let mut slow = harness.server.mock(|when, then| {
        when.method("GET")
            .path("/socket/search")
            .query_param("q", QUERY);
        then.status(200)
            .header("content-type", "text/event-stream")
            .delay(Duration::from_secs(5))
            .body_from_file(support::fixture("search_stream_sse.bin"));
    });
    let retry = harness.mock_search_stream(None, "search_stream_sse.bin");

    let task = tokio::spawn({
        let client = Arc::clone(&client);

        async move { client.search(QUERY).await }
    });

    // Wait until the request is actually being served before cancelling it.
    tokio::time::timeout(Duration::from_secs(2), async {
        while slow.calls() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the first request reaches the server");

    task.abort();
    assert!(
        task.await
            .expect_err("the task is cancelled")
            .is_cancelled()
    );
    assert_eq!(slow.calls(), 1);

    // The cancelled search dropped its lease: the retry reuses the session and does not
    // establish a second one.
    slow.delete();
    let results = client.search(QUERY).await.expect("the retry succeeds");

    assert_eq!(results.len(), 5);
    retry.assert();
    session.assert_calls(1);
    nonce.assert_calls(1);
}

#[tokio::test]
async fn requests_beyond_capacity_wait_for_a_session() {
    let harness = KagiHarness::start();
    let client = harness.client_with(1, Duration::from_secs(60));
    let session = harness.mock_session();
    let nonce = harness.mock_nonce();
    let stream =
        harness.mock_search_stream_reusable("search_stream_sse.bin", Duration::from_millis(200));

    let (a, b) = tokio::join!(client.search(QUERY), client.search(QUERY));

    a.expect("the first search succeeds");
    b.expect("the second search succeeds");

    session.assert_calls(1);
    nonce.assert_calls(1);
    stream.assert_calls(2);
}
