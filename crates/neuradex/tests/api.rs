//! End-to-end tests for the HTTP API.
//!
//! The application talks to a local `httpmock` server for both page fetching and Kagi
//! searching, so every test exercises the real routers, extractors, clients, streaming
//! pipeline, and JSON serialization.

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};
use test_support::app::{TestApp, USER_AGENT_VALUE};

/// Asserts that `response` renders the JSON error envelope of `kind`.
async fn assert_error(response: Response, status: StatusCode, kind: &str) -> Value {
    assert_eq!(response.status(), status);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
    );

    let body = TestApp::json(response).await;

    assert_eq!(body["error"]["type"], kind);
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "the error message is empty: {body}"
    );

    body
}

#[tokio::test]
async fn fetch_returns_metadata_body_and_metrics() {
    let app = TestApp::start();
    let html = "<!doctype html><html><head><title>Test page</title>\
                <link rel=\"canonical\" href=\"https://example.com/canonical\">\
                <meta name=\"description\" content=\"A test page\">\
                </head><body><p>Hello</p></body></html>";
    let page = app.server.mock(|when, then| {
        when.method("GET").path("/page");
        then.status(200)
            .header("content-type", "text/html; charset=utf-8")
            .body(html);
    });

    let response = app
        .get(&format!("/v1/fetch?url={}", app.url("/page")))
        .await;

    assert_eq!(response.status(), StatusCode::OK);

    let body = TestApp::json(response).await;

    assert_eq!(body["url"], app.url("/page"));
    assert_eq!(body["metadata"]["title"], "Test page");
    assert_eq!(
        body["metadata"]["canonical"],
        "https://example.com/canonical"
    );
    assert_eq!(body["metadata"]["description"], "A test page");
    assert_eq!(body["body"], html);
    assert_eq!(body["truncated"], false);
    assert!(body.get("request_headers").is_none());
    assert!(body.get("response_headers").is_none());
    assert_eq!(body["metrics"]["status"], 200);
    assert_eq!(body["metrics"]["final_url"], app.url("/page"));
    assert_eq!(body["metrics"]["content_type"], "text/html");
    assert_eq!(body["metrics"]["bytes_read"], html.len());
    assert_eq!(body["metrics"]["redirects_followed"], 0);
    assert_eq!(body["metrics"]["truncated"], false);

    page.assert();
}

#[tokio::test]
async fn fetch_includes_the_headers_only_when_requested() {
    let app = TestApp::start();
    let page = app.server.mock(|when, then| {
        when.method("GET").path("/page");
        then.status(200)
            .header("content-type", "text/plain; charset=utf-8")
            .header("x-test", "value")
            .body("hello");
    });

    let response = app
        .get(&format!(
            "/v1/fetch?url={}&include=headers",
            app.url("/page")
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);

    let body = TestApp::json(response).await;

    assert_eq!(body["request_headers"]["user-agent"], USER_AGENT_VALUE);
    assert_eq!(
        body["request_headers"]["accept-encoding"],
        "gzip, deflate, br, zstd"
    );
    assert_eq!(
        body["response_headers"]["content-type"],
        "text/plain; charset=utf-8"
    );
    assert_eq!(body["response_headers"]["x-test"], "value");

    page.assert();
}

#[tokio::test]
async fn fetch_follows_redirects_and_reports_every_hop() {
    let app = TestApp::start();
    let start = app.server.mock(|when, then| {
        when.method("GET").path("/start");
        then.status(302).header("location", "middle");
    });
    let middle = app.server.mock(|when, then| {
        when.method("GET").path("/middle");
        then.status(301).header("location", "/final");
    });
    let final_page = app.server.mock(|when, then| {
        when.method("GET").path("/final");
        then.status(200)
            .header("content-type", "text/html")
            .body("<html><head><title>Final</title></head></html>");
    });

    let start_url = app.url("/start");
    let response = app
        .get(&format!("/v1/fetch?url={start_url}&include=redirects"))
        .await;

    assert_eq!(response.status(), StatusCode::OK);

    let body = TestApp::json(response).await;

    assert_eq!(body["url"], start_url);
    assert_eq!(body["metadata"]["title"], "Final");
    assert_eq!(body["metrics"]["final_url"], app.url("/final"));
    assert_eq!(body["metrics"]["redirects_followed"], 2);
    assert_eq!(
        body["metrics"]["redirects"],
        json!([
            {"status": 302, "url": start_url},
            {"status": 301, "url": app.url("/middle")},
        ])
    );

    start.assert();
    middle.assert();
    final_page.assert();
}

#[tokio::test]
async fn fetch_stops_when_the_redirect_budget_is_exhausted() {
    let app = TestApp::start();
    let start = app.server.mock(|when, then| {
        when.method("GET").path("/start");
        then.status(302).header("location", "/middle");
    });
    let middle = app.server.mock(|when, then| {
        when.method("GET").path("/middle");
        then.status(301).header("location", "/final");
    });
    let final_page = app.server.mock(|when, then| {
        when.method("GET").path("/final");
        then.status(200).body("should not be reached");
    });

    let response = app
        .get(&format!(
            "/v1/fetch?url={}&redirects=1&include=redirects",
            app.url("/start")
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);

    let body = TestApp::json(response).await;

    // The redirect response itself is returned as the final response.
    assert_eq!(body["metrics"]["status"], 301);
    assert_eq!(body["metrics"]["final_url"], app.url("/middle"));
    assert_eq!(body["metrics"]["redirects_followed"], 1);
    assert_eq!(body["body"], "");

    start.assert();
    middle.assert();
    final_page.assert_calls(0);
}

#[tokio::test]
async fn fetch_maps_connection_failures_to_the_error_envelope() {
    let app = TestApp::start();

    // Reserve a loopback port, then release it so the connection is refused.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let address = listener.local_addr().expect("the bound address");
    drop(listener);

    let response = app.get(&format!("/v1/fetch?url=http://{address}/")).await;
    let body = assert_error(response, StatusCode::BAD_GATEWAY, "upstream_error").await;

    assert_eq!(body["error"]["message"], "the upstream request failed");
}

#[tokio::test]
async fn fetch_and_peek_reject_binary_content() {
    let app = TestApp::start();
    let image = app.server.mock(|when, then| {
        when.method("GET").path("/image");
        then.status(200)
            .header("content-type", "image/png")
            .body([0x89, b'P', b'N', b'G']);
    });

    for endpoint in ["fetch", "peek"] {
        let response = app
            .get(&format!("/v1/{endpoint}?url={}", app.url("/image")))
            .await;

        let body = assert_error(
            response,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        )
        .await;

        assert_eq!(
            body["error"]["message"],
            "refusing to fetch non-text content: image/png"
        );
    }

    image.assert_calls(2);
}

#[tokio::test]
async fn peek_returns_only_the_head_metadata() {
    let app = TestApp::start();
    let page = app.server.mock(|when, then| {
        when.method("GET").path("/page");
        then.status(200).header("content-type", "text/html").body(
            "<html><head><title>Peeked</title>\
                 <meta property=\"og:site_name\" content=\"Example\">\
                 </head><body><p>ignored</p></body></html>",
        );
    });

    let response = app.get(&format!("/v1/peek?url={}", app.url("/page"))).await;

    assert_eq!(response.status(), StatusCode::OK);

    let body = TestApp::json(response).await;

    assert_eq!(body["url"], app.url("/page"));
    assert_eq!(body["metadata"]["title"], "Peeked");
    assert_eq!(body["metadata"]["og"]["site_name"], "Example");
    assert!(body.get("body").is_none());
    assert_eq!(body["metrics"]["status"], 200);
    assert_eq!(body["metrics"]["content_type"], "text/html");

    page.assert();
}

#[tokio::test]
async fn search_returns_kagi_results_and_honors_the_limit() {
    let app = TestApp::start();
    let session = app.mock_kagi_session();
    let nonce = app.mock_kagi_nonce();
    let stream = app.mock_kagi_search_stream("hello world", "search_stream_sse.bin");

    let response = app.get("/v1/search?query=hello%20world&limit=2").await;

    assert_eq!(response.status(), StatusCode::OK);

    let body = TestApp::json(response).await;

    assert_eq!(body["query"], "hello world");
    assert_eq!(body["results"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["results"][0]["title"], "Hello, world - Wikipedia");
    assert_eq!(
        body["results"][0]["url"],
        "https://en.wikipedia.org/wiki/Hello,_world"
    );
    assert_eq!(body["metrics"]["result_count"], 2);

    session.assert();
    nonce.assert();
    stream.assert();
}

#[tokio::test]
async fn concurrent_searches_are_queued_and_reuse_sessions() {
    let app = TestApp::with_kagi_sessions(2);
    let session = app.mock_kagi_session();
    let nonce = app.mock_kagi_nonce();
    let stream = app.mock_kagi_search_stream_delayed(
        "hello world",
        "search_stream_sse.bin",
        std::time::Duration::from_millis(200),
    );

    let responses = futures::future::join_all([
        app.get("/v1/search?query=hello%20world"),
        app.get("/v1/search?query=hello%20world"),
        app.get("/v1/search?query=hello%20world"),
    ])
    .await;

    for response in responses {
        assert_eq!(response.status(), StatusCode::OK);

        let body = TestApp::json(response).await;
        assert_eq!(body["results"].as_array().map(Vec::len), Some(5));
    }

    // Three concurrent searches share the two-session pool: the third request queues on
    // the search gate and then reuses a checked-in session.
    session.assert_calls(2);
    nonce.assert_calls(2);
    stream.assert_calls(3);
}

#[tokio::test]
async fn search_times_out_and_returns_the_error_envelope() {
    let app = TestApp::start();
    let session = app.mock_kagi_session();
    let nonce = app.mock_kagi_nonce();
    let stream = app.mock_kagi_search_stream_delayed(
        "hello world",
        "search_stream_sse.bin",
        std::time::Duration::from_secs(5),
    );

    let response = app.get("/v1/search?query=hello%20world&timeout=1").await;
    let body = assert_error(response, StatusCode::BAD_GATEWAY, "upstream_error").await;

    assert_eq!(body["error"]["message"], "the upstream request failed");

    session.assert();
    nonce.assert();
    stream.assert();
}

#[tokio::test]
async fn search_maps_upstream_failures_to_the_error_envelope() {
    let app = TestApp::start();
    let session = app.mock_kagi_session();
    let nonce = app.mock_kagi_nonce();
    let stream = app.server.mock(|when, then| {
        when.method("GET").path("/socket/search");
        then.status(500);
    });

    let response = app.get("/v1/search?query=hello%20world").await;
    let body = assert_error(response, StatusCode::BAD_GATEWAY, "upstream_error").await;

    assert_eq!(body["error"]["message"], "the upstream request failed");

    session.assert();
    nonce.assert();
    stream.assert();
}

#[tokio::test]
async fn openwebui_search_returns_a_bare_result_array() {
    let app = TestApp::start();
    let session = app.mock_kagi_session();
    let nonce = app.mock_kagi_nonce();
    let stream = app.mock_kagi_search_stream("hello world", "search_stream_sse.bin");

    let response = app
        .post_json("/v1/openwebui_search", &json!({"query": "hello world"}))
        .await;

    assert_eq!(response.status(), StatusCode::OK);

    let body = TestApp::json(response).await;
    let results = body.as_array().expect("the response is a bare array");

    // The fixture has five results; the default count keeps all of them.
    assert_eq!(results.len(), 5);
    assert_eq!(
        results[0]["link"],
        "https://en.wikipedia.org/wiki/Hello,_world"
    );
    assert_eq!(results[0]["title"], "Hello, world - Wikipedia");
    assert!(results[0]["snippet"].is_string());
    assert!(results[0].get("description").is_none());

    let response = app
        .post_json(
            "/v1/openwebui_search",
            &json!({"query": "hello world", "count": 2}),
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);

    let body = TestApp::json(response).await;
    assert_eq!(body.as_array().map(Vec::len), Some(2));

    // Both searches reuse the one Kagi session.
    session.assert_calls(1);
    nonce.assert_calls(1);
    stream.assert_calls(2);
}

#[tokio::test]
async fn openwebui_search_rejects_an_empty_query() {
    let app = TestApp::start();

    let response = app
        .post_json("/v1/openwebui_search", &json!({"query": ""}))
        .await;

    let body = assert_error(response, StatusCode::BAD_REQUEST, "invalid_param").await;

    assert_eq!(body["error"]["message"], "the query is empty");
}
