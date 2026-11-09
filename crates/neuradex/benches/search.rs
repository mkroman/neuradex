//! Criterion benchmarks for the two search endpoints.
//!
//! The benchmark runs the production router against a local `httpmock` Kagi server supplied by
//! `test-support`. The session is established during Criterion's warm-up, so steady-state
//! iterations measure the endpoint extraction, the shared search gate, the Kagi client's stream
//! parser, result mapping, and JSON serialization. The live Kagi network is intentionally not
//! part of the measurement.
//!
//! The `search-contended` group runs eight searches at once against one and two search sessions.
//! The one-session case is serialized at the shared search gate; it models an agent burst rather
//! than a warm single request. A cold-session benchmark is not included: the session pool is
//! process-wide and warm-up establishes it, so a per-iteration fresh client would measure
//! application construction rather than the endpoint.

use std::hint::black_box;
use std::time::Duration;

use axum::body::to_bytes;
use axum::http::StatusCode;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use futures::future::join_all;
use serde_json::json;
use test_support::app::TestApp;

/// The maximum response body size read by the benchmark.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// The number of searches submitted together in the contended group.
const CONTENDED_REQUESTS: usize = 8;

/// The mock Kagi stream delay used by the contended group.
///
/// Without a delay the searches complete too quickly for the shared gate to queue them.
const CONTENDED_DELAY: Duration = Duration::from_millis(10);

/// Returns the shared Criterion configuration.
fn config() -> Criterion {
    Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .configure_from_args()
}

/// Benchmarks `/v1/search` and `/v1/openwebui_search`.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the benchmark groups are finished explicitly"
)]
fn search_benchmarks(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .expect("build the benchmark runtime");

    let app = TestApp::start();
    let _session = app.mock_kagi_session();
    let _nonce = app.mock_kagi_nonce();
    let _stream = app.mock_kagi_search_stream("hello world", "search_stream_sse.bin");

    let mut endpoint_group = c.benchmark_group("search");
    endpoint_group.throughput(Throughput::Elements(1));

    endpoint_group.bench_function("v1/search", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = app
                .post_json("/v1/search", &json!({"query": "hello world", "limit": 2}))
                .await;

            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
                .await
                .expect("read the search response");
            black_box(body);
        });
    });

    endpoint_group.bench_function("v1/openwebui_search", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = app
                .post_json(
                    "/v1/openwebui_search",
                    &json!({"query": "hello world", "count": 2}),
                )
                .await;

            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
                .await
                .expect("read the search response");
            black_box(body);
        });
    });

    endpoint_group.finish();

    contended_benchmark(&runtime, c, 1, "eight-searches/one-session");
    contended_benchmark(&runtime, c, 2, "eight-searches/two-sessions");
}

/// Benchmarks eight concurrent searches against `sessions` search slots.
fn contended_benchmark(
    runtime: &tokio::runtime::Runtime,
    c: &mut Criterion,
    sessions: usize,
    name: &str,
) {
    let app = TestApp::with_kagi_sessions(sessions);
    let _session = app.mock_kagi_session();
    let _nonce = app.mock_kagi_nonce();
    let _stream = app.mock_kagi_search_stream_delayed(
        "hello world",
        "search_stream_sse.bin",
        CONTENDED_DELAY,
    );

    let mut contended_group = c.benchmark_group("search-contended");
    contended_group.throughput(Throughput::Elements(
        u64::try_from(CONTENDED_REQUESTS).expect("the request count fits in u64"),
    ));
    contended_group.bench_function(name, |b| {
        b.to_async(runtime).iter(|| async {
            let searches = (0..CONTENDED_REQUESTS).map(|_| async {
                let response = app
                    .post_json("/v1/search", &json!({"query": "hello world", "limit": 2}))
                    .await;

                assert_eq!(response.status(), StatusCode::OK);
                to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
                    .await
                    .expect("read the search response")
            });

            let bodies = join_all(searches).await;
            black_box(bodies);
        });
    });
    contended_group.finish();
}

criterion_group! {
    name = benches;
    config = config();
    targets = search_benchmarks
}
criterion_main!(benches);
