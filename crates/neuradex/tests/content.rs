//! Integration tests for content-plane wiring in the HTTP service.
//!
//! These tests drive the real router, extractors, Kagi client, and content pipeline against
//! local mock servers and a temporary local content store.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use neuradex::audit::AuditConfig;
use neuradex::content::ContentPipeline;
use neuradex::state::AppState;
use neuradex_audit::{
    AuthMode, ContentReference, ContentStorage as AuditContentStorage, FetchOperation,
    HeaderPolicy, MemorySink, Operation, SearchOperation,
};
use neuradex_content::{
    CasPolicy, ContentStorage, ContentStore, ContentWriter, LocalStore, read_content,
    read_pack_content,
};
use neuradex_packfile::{ContentHash, hash_body};
use serde_json::{Value, json};
use tempfile::TempDir;
use test_support::kagi::KagiHarness;
use tower::ServiceExt;

/// The content capture limit used by the audit configuration in these tests.
const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;

/// A service wired to a temporary local content store and an in-memory audit sink.
struct ContentFixture {
    _directory: TempDir,
    store: ContentStore,
    pipeline: Arc<ContentPipeline>,
    sink: Arc<MemorySink>,
    kagi: KagiHarness,
}

impl ContentFixture {
    /// Builds a fixture with the default content policy.
    fn new() -> Self {
        Self::with_policy(CasPolicy::default())
    }

    /// Builds a fixture with an explicit packing and capture policy.
    fn with_policy(policy: CasPolicy) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let local =
            LocalStore::new(directory.path().join("store")).expect("the local store is created");
        let store = ContentStore::Local(local);
        let pipeline = Arc::new(ContentPipeline::new(
            ContentWriter::new(store.clone(), policy),
            store.clone(),
        ));

        Self {
            _directory: directory,
            store,
            pipeline,
            sink: Arc::new(MemorySink::new()),
            kagi: KagiHarness::start(),
        }
    }

    /// Builds a router with content storage enabled for this fixture.
    fn router(&self) -> Router {
        let state = AppState::try_new_with_kagi_client_and_audit(
            "test-agent",
            self.kagi.client_with(2, Duration::from_secs(60)),
            Duration::from_secs(5),
            AuditConfig::new(self.sink.clone(), HeaderPolicy::new(), MAX_BODY_BYTES),
        )
        .expect("the test application state builds")
        .with_content(Arc::clone(&self.pipeline));

        neuradex::api::v1::router(state)
    }

    /// Returns the single audit event emitted by the request.
    fn event(&self) -> neuradex_audit::AuditEvent {
        let events = self.sink.events().expect("the sink is not poisoned");

        assert_eq!(events.len(), 1, "expected exactly one audit event");

        events.into_iter().next().expect("one event")
    }
}

/// Sends `request` through `router`.
async fn send(router: &Router, request: Request<Body>) -> axum::response::Response {
    router
        .clone()
        .oneshot(request)
        .await
        .expect("the router is infallible")
}

/// Builds a `POST` JSON request.
fn post_json(path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .expect("the test request is valid")
}

/// Reads a response body.
async fn response_bytes(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the response body is readable")
        .to_vec()
}

/// Reconstructs a stored-content reference from an audit reference.
fn stored(reference: &ContentReference) -> neuradex_content::StoredContent {
    let hash = format!(
        "blake3:{} sha256:{}",
        reference.blake3(),
        reference.sha256()
    )
    .parse::<ContentHash>()
    .expect("the audit reference carries valid hashes");
    let storage = match reference.storage() {
        AuditContentStorage::Pack => ContentStorage::Pack,
        AuditContentStorage::Object => ContentStorage::Object,
        other => panic!("unsupported content storage: {other:?}"),
    };

    neuradex_content::StoredContent::new(
        storage,
        reference.key(),
        hash,
        reference.size(),
        reference.content_type().map(str::to_owned),
    )
}

/// Reads a stored body back and verifies the range path matches the full reader.
async fn read_back(store: &ContentStore, reference: &ContentReference) -> Vec<u8> {
    let stored = stored(reference);
    let body = read_content(store, &stored)
        .await
        .expect("the content is readable")
        .expect("the content exists");

    assert_eq!(
        body.len() as u64,
        reference.size(),
        "the stored body size matches the reference"
    );
    assert_eq!(
        hash_body(&body).blake3_hex(),
        reference.blake3(),
        "the stored body hashes to the referenced BLAKE3 digest"
    );
    assert_eq!(
        hash_body(&body).sha256_hex(),
        reference.sha256(),
        "the stored body hashes to the referenced SHA-256 digest"
    );

    if matches!(stored.storage(), ContentStorage::Pack) {
        let by_hash = read_pack_content(store, stored.key(), &stored.hash())
            .await
            .expect("the pack is readable")
            .expect("the hash is in the pack");
        assert_eq!(by_hash, body);
    }

    body
}

/// Returns the search operation from an event.
fn search_operation(event: &neuradex_audit::AuditEvent) -> &SearchOperation {
    match event.operation() {
        Some(Operation::Search(operation)) => operation,
        other => panic!("expected a search operation, got {other:?}"),
    }
}

/// Returns the fetch or peek operation from an event.
fn fetch_operation(event: &neuradex_audit::AuditEvent) -> &FetchOperation {
    match event.operation() {
        Some(Operation::Fetch(operation) | Operation::Peek(operation)) => operation,
        other => panic!("expected a fetch or peek operation, got {other:?}"),
    }
}

#[tokio::test]
async fn search_stores_the_request_and_response_bodies() {
    let fixture = ContentFixture::new();
    let _session = fixture.kagi.mock_session();
    let _nonce = fixture.kagi.mock_nonce();
    let _stream = fixture
        .kagi
        .mock_search_stream_reusable("search_stream_sse.bin", Duration::ZERO);
    let router = fixture.router();

    let request_body = json!({"query": "hello world", "limit": 2}).to_string();
    let response = send(&router, post_json("/v1/search", &request_body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = response_bytes(response).await;
    let decoded: Value = serde_json::from_slice(&response_body).expect("valid JSON");
    assert_eq!(decoded["results"].as_array().map(Vec::len), Some(2));

    let event = fixture.event();
    let references = event.content_references();
    assert_eq!(references.len(), 2, "{references:?}");

    assert_eq!(
        references[0].content_type(),
        Some("application/json"),
        "the raw request body is stored first"
    );
    assert_eq!(references[0].size(), request_body.len() as u64);
    assert_eq!(
        references[0].blake3(),
        hash_body(request_body.as_bytes()).blake3_hex()
    );
    assert_eq!(
        references[1].content_type(),
        Some("application/json"),
        "the serialized response body is stored second"
    );
    assert_eq!(references[1].size(), response_body.len() as u64);
    assert_eq!(
        references[1].blake3(),
        hash_body(&response_body).blake3_hex()
    );
    assert_eq!(
        event.http().response_body_bytes(),
        Some(response_body.len() as u64)
    );

    let operation = search_operation(&event);
    assert_eq!(operation.limit(), Some(2));
    assert_eq!(operation.auth_mode(), Some(AuthMode::Session));
    assert!(operation.queue_ms().is_some());
    assert!(operation.upstream_ms().is_some());
    assert_eq!(operation.upstream_result_count(), Some(5));
    assert_eq!(operation.returned_result_count(), Some(2));
    assert!(!operation.zero_results());
    assert_eq!(operation.result_digest().map(str::len), Some(64));
    assert_eq!(
        operation
            .top_hosts()
            .iter()
            .map(neuradex_audit::HostCount::count)
            .sum::<u64>(),
        2
    );

    let encoded = serde_json::to_string(&event).expect("the event serializes");
    assert!(!encoded.contains("hello world"), "{encoded}");

    fixture.pipeline.flush().await.expect("the content flushes");

    assert_eq!(
        read_back(&fixture.store, &references[0]).await,
        request_body.as_bytes()
    );
    assert_eq!(
        read_back(&fixture.store, &references[1]).await,
        response_body
    );
}

#[tokio::test]
async fn fetch_stores_the_page_body_with_the_upstream_content_type() {
    let fixture = ContentFixture::new();
    let page_body = "hello from the page";
    let page = fixture.kagi.server.mock(|when, then| {
        when.method("GET").path("/page");
        then.status(200)
            .header("content-type", "text/plain; charset=utf-8")
            .body(page_body);
    });
    let router = fixture.router();

    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/v1/fetch?url={}",
            fixture.kagi.server.url("/page")
        ))
        .body(Body::empty())
        .expect("the test request is valid");
    let response = send(&router, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = response_bytes(response).await;
    let decoded: Value = serde_json::from_slice(&response_body).expect("valid JSON");
    assert_eq!(decoded["body"], page_body);

    let event = fixture.event();
    let references = event.content_references();
    assert_eq!(references.len(), 1, "{references:?}");
    assert_eq!(references[0].content_type(), Some("text/plain"));
    assert_eq!(references[0].size(), page_body.len() as u64);
    assert_eq!(
        references[0].blake3(),
        hash_body(page_body.as_bytes()).blake3_hex()
    );

    let operation = fetch_operation(&event);
    assert_eq!(operation.redirects(), 0);
    assert_eq!(operation.final_host(), Some("127.0.0.1"));
    assert_eq!(operation.upstream_status(), Some(200));
    assert_eq!(operation.content_type(), Some("text/plain"));
    assert_eq!(operation.bytes_read(), Some(page_body.len() as u64));
    assert!(operation.ttfb_ms().is_some());
    assert!(operation.total_ms().is_some());
    assert!(!operation.truncated());

    let encoded_event = serde_json::to_string(&event).expect("the event serializes");
    assert!(
        !encoded_event.contains(page_body),
        "the fetched body is referenced, never stored in the event: {encoded_event}"
    );

    fixture.pipeline.flush().await.expect("the content flushes");

    assert_eq!(
        read_back(&fixture.store, &references[0]).await,
        page_body.as_bytes()
    );
    page.assert();
}

#[tokio::test]
async fn peek_stores_only_the_bytes_it_read() {
    let fixture = ContentFixture::new();
    let page_body = "<html><head><title>Peeked</title></head><body>ignored</body></html>";
    let page = fixture.kagi.server.mock(|when, then| {
        when.method("GET").path("/page");
        then.status(200)
            .header("content-type", "text/html; charset=utf-8")
            .body(page_body);
    });
    let router = fixture.router();

    let request = Request::builder()
        .method("GET")
        .uri(format!("/v1/peek?url={}", fixture.kagi.server.url("/page")))
        .body(Body::empty())
        .expect("the test request is valid");
    let response = send(&router, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let decoded: Value = serde_json::from_slice(&response_bytes(response).await)
        .expect("the response is valid JSON");
    assert_eq!(decoded["metadata"]["title"], "Peeked");
    assert!(decoded.get("body").is_none());

    let event = fixture.event();
    let references = event.content_references();
    assert_eq!(references.len(), 1, "{references:?}");
    assert_eq!(references[0].content_type(), Some("text/html"));

    let operation = fetch_operation(&event);
    assert!(operation.stopped_at_head());
    assert_eq!(operation.bytes_read(), Some(references[0].size()));
    assert!(!operation.truncated());

    fixture.pipeline.flush().await.expect("the content flushes");

    let body = read_back(&fixture.store, &references[0]).await;
    assert!(
        body.starts_with(b"<html>"),
        "peek stored the head it read: {body:?}"
    );
    assert!(
        body.len() <= page_body.len(),
        "peek stores only bytes it read, never more than the page"
    );
    page.assert();
}

#[tokio::test]
async fn disabled_content_writes_no_references() {
    let kagi = KagiHarness::start();
    let _session = kagi.mock_session();
    let _nonce = kagi.mock_nonce();
    let _stream = kagi.mock_search_stream_reusable("search_stream_sse.bin", Duration::ZERO);
    let sink = Arc::new(MemorySink::new());
    let state = AppState::try_new_with_kagi_client_and_audit(
        "test-agent",
        kagi.client_with(2, Duration::from_secs(60)),
        Duration::from_secs(5),
        AuditConfig::new(sink.clone(), HeaderPolicy::new(), MAX_BODY_BYTES),
    )
    .expect("the test application state builds");
    let router = neuradex::api::v1::router(state);

    let response = send(
        &router,
        post_json("/v1/search", &json!({"query": "hello world"}).to_string()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let events = sink.events().expect("the sink is not poisoned");
    assert_eq!(events.len(), 1);
    assert!(
        events[0].content_references().is_empty(),
        "content is disabled, so no reference was recorded"
    );
}

#[tokio::test]
async fn a_content_append_failure_does_not_fail_the_request() {
    let policy = CasPolicy::new(
        128 * 1024 * 1024,
        Duration::from_secs(600),
        1,
        30 * 1024 * 1024,
        3,
        365,
    )
    .expect("the test policy is valid");
    let fixture = ContentFixture::with_policy(policy);
    let _session = fixture.kagi.mock_session();
    let _nonce = fixture.kagi.mock_nonce();
    let _stream = fixture
        .kagi
        .mock_search_stream_reusable("search_stream_sse.bin", Duration::ZERO);
    let router = fixture.router();

    let response = send(
        &router,
        post_json(
            "/v1/search",
            &json!({"query": "hello world", "limit": 2}).to_string(),
        ),
    )
    .await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "content failures must not turn a successful search into an error"
    );
    let event = fixture.event();
    assert!(
        event.content_references().is_empty(),
        "the oversized bodies could not be stored"
    );
    let operation = search_operation(&event);
    assert_eq!(operation.returned_result_count(), Some(2));

    let flushed = fixture
        .pipeline
        .flush()
        .await
        .expect("an empty flush succeeds");
    assert!(flushed.is_empty());
}
