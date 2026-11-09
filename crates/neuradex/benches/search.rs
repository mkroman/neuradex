//! Criterion benchmarks for the two search endpoints.
//!
//! The benchmark runs the production router against a local `httpmock` Kagi server supplied by
//! `test-support`. The session is established during Criterion's warm-up, so steady-state
//! iterations measure the endpoint extraction, the shared search gate, the Kagi client's stream
//! parser, result mapping, and JSON serialization. The live Kagi network is intentionally not
//! part of the measurement.

use std::hint::black_box;
use std::time::Duration;

use axum::body::to_bytes;
use axum::http::StatusCode;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use serde_json::json;
use test_support::app::TestApp;

/// The maximum response body size read by the benchmark.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Returns the shared Criterion configuration.
fn config() -> Criterion {
    Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .configure_from_args()
}

/// Benchmarks `/v1/search` and `/v1/openwebui_search`.
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

    let mut group = c.benchmark_group("search");
    group.throughput(Throughput::Elements(1));

    group.bench_function("v1/search", |b| {
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

    group.bench_function("v1/openwebui_search", |b| {
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

    group.finish();
}

criterion_group! {
    name = benches;
    config = config();
    targets = search_benchmarks
}
criterion_main!(benches);
