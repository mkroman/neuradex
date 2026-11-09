//! Criterion benchmarks for the application HTTP stack.
//!
//! Every iteration drives the production router from [`neuradex::api::v1::router`] through
//! [`tower::ServiceExt::oneshot`], so the measured work is the router, the extractors, the
//! handlers, the `wreq` fetch, the stream parser, and JSON serialization. These are
//! application-stack measurements: the inbound hyper/TCP layer is intentionally excluded, since
//! no socket sits in front of the service and the router is called in-process rather than
//! through `test-support` or an HTTP mock.
//!
//! The local axum upstream runs on its own multi-threaded Tokio runtime, separate from the
//! runtime that drives the benchmarked future. The upstream runtime keeps serving the page
//! while the benchmark runtime is blocked in `block_on`, so upstream work is not charged to the
//! measured iteration and the two sides cannot stall each other's worker pool. Both runtimes
//! stay alive until the group finishes, and the upstream tasks are aborted before either
//! runtime is dropped. Set `NEURADEX_BENCH_WORKERS` to override the benchmark runtime's worker
//! count (default 2); the upstream runtime always uses 2 workers.
//!
//! The `head-only` case serves a 64 KiB head with no body and acts as the control for the
//! `large` case, which pairs the same head with a 1 MiB body.
//!
//! Set `NEURADEX_BENCH_TRACING` to a truthy value (`1`, `true`, `yes`, or `on`) to install a
//! global `tracing_subscriber::fmt` subscriber that writes to [`std::io::sink`] and records span
//! close events. This measures production span/audit formatting overhead without stdout I/O
//! noise; it is off by default so normal benchmark runs are unchanged.

mod pages;

use std::hint::black_box;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::header;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, BenchmarkId, Criterion, Throughput};
use criterion::{criterion_group, criterion_main};
use neuradex::api::v1;
use neuradex::state::AppState;
use secrecy::SecretString;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tower::ServiceExt;

/// The head size of the small benchmark page.
const SMALL_HEAD_BYTES: usize = 4 * 1024;

/// The body size of the small benchmark page.
const SMALL_BODY_BYTES: usize = 64 * 1024;

/// The head size of the large benchmark page.
const LARGE_HEAD_BYTES: usize = 64 * 1024;

/// The body size of the large benchmark page.
const LARGE_BODY_BYTES: usize = 1024 * 1024;

/// The body limit passed to [`to_bytes`] when reading a benchmark response.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The default number of worker threads in the benchmark runtime.
const DEFAULT_BENCH_WORKERS: usize = 2;

/// The number of worker threads in the upstream runtime.
const UPSTREAM_WORKERS: usize = 2;

/// Returns the configured benchmark runtime worker count.
///
/// `NEURADEX_BENCH_WORKERS` overrides the default of [`DEFAULT_BENCH_WORKERS`]. Invalid or zero
/// values fall back to the default.
fn bench_workers() -> usize {
    std::env::var("NEURADEX_BENCH_WORKERS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|&workers| workers > 0)
        .unwrap_or(DEFAULT_BENCH_WORKERS)
}

/// Returns whether `NEURADEX_BENCH_TRACING` requests the tracing overhead mode.
fn tracing_enabled() -> bool {
    std::env::var("NEURADEX_BENCH_TRACING").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Installs the optional sink-backed tracing subscriber.
///
/// The subscriber is only installed when [`tracing_enabled`] returns true. An already-installed
/// global subscriber is left untouched, so a harness that configures its own subscriber still
/// works.
fn init_tracing() {
    if !tracing_enabled() {
        return;
    }

    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_ansi(false)
        .try_init();
}

/// A local upstream server that serves one prebuilt page.
struct Upstream {
    /// The base URL that the benchmark requests fetch.
    url: String,
    /// The spawned server task, aborted once the benchmark group finishes.
    task: JoinHandle<()>,
}

impl Upstream {
    /// Starts an axum server for `page` on an ephemeral loopback port.
    ///
    /// `runtime` must be the dedicated upstream runtime, not the benchmark runtime, so the
    /// server keeps making progress while the benchmarked future is driven. The page is copied
    /// into a [`Bytes`] once, outside the timed closure, so every request only clones a cheap
    /// handle.
    fn spawn(runtime: &Runtime, page: &pages::Page) -> Self {
        let body = Bytes::from(page.html.clone());
        let listener = runtime
            .block_on(TcpListener::bind("127.0.0.1:0"))
            .expect("bind the upstream server");
        let address = listener.local_addr().expect("read the upstream address");
        let app = Router::new().route(
            "/",
            get(move || {
                let body = body.clone();
                async move { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body) }
            }),
        );
        let task = runtime.spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("run the upstream server");
        });

        Self {
            url: format!("http://{address}/"),
            task,
        }
    }
}

/// One benchmark case: a generated page and the upstream server that serves it.
struct Case {
    /// The Criterion parameter name for the page size.
    label: &'static str,
    /// The generated page.
    page: pages::Page,
    /// The local upstream serving the page.
    upstream: Upstream,
}

/// Builds a benchmark case and starts its upstream server.
fn make_case(
    upstream_runtime: &Runtime,
    label: &'static str,
    head_bytes: usize,
    body_bytes: usize,
) -> Case {
    let page = pages::make_page(head_bytes, body_bytes);
    debug_assert!(
        page.head_len <= page.len(),
        "the generated head fits inside the page"
    );
    let upstream = Upstream::spawn(upstream_runtime, &page);

    Case {
        label,
        page,
        upstream,
    }
}

/// Builds the request path for `endpoint` with `url` as its `url` query parameter.
fn request_path(endpoint: &str, url: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("url", url)
        .finish();

    format!("/v1/{endpoint}?{query}")
}

/// Benchmarks both endpoints against one case.
fn bench_case(
    group: &mut BenchmarkGroup<'_, WallTime>,
    runtime: &Runtime,
    router: &Router,
    case: &Case,
) {
    for endpoint in ["peek", "fetch"] {
        // Peek aborts after the head; fetch returns the whole document. Report the bytes each
        // endpoint is expected to process rather than charging peek for the body.
        let bytes = if endpoint == "peek" {
            case.page.head_len
        } else {
            case.page.len()
        };

        group.throughput(Throughput::ElementsAndBytes {
            elements: 1,
            bytes: u64::try_from(bytes).expect("the page length fits in u64"),
        });

        let path = request_path(endpoint, &case.upstream.url);

        group.bench_with_input(BenchmarkId::new(endpoint, case.label), &path, |b, path| {
            b.to_async(runtime).iter(|| {
                let router = router.clone();
                let request = Request::builder()
                    .uri(path.as_str())
                    .body(Body::empty())
                    .expect("build a valid benchmark request");
                async move {
                    let response = router.oneshot(request).await.expect("the router responds");
                    assert_eq!(
                        response.status(),
                        StatusCode::OK,
                        "the benchmark request succeeds"
                    );
                    let body = to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
                        .await
                        .expect("read the response body");
                    black_box(body);
                }
            });
        });
    }
}

/// Runs the HTTP-stack benchmark cases.
fn http_benchmarks(c: &mut Criterion) {
    init_tracing();

    let upstream_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(UPSTREAM_WORKERS)
        .thread_name("neuradex-bench-upstream")
        .build()
        .expect("build the upstream runtime");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(bench_workers())
        .thread_name("neuradex-bench")
        .build()
        .expect("build the benchmark runtime");

    let state = AppState::try_new(
        "neuradex-benchmark",
        SecretString::from("benchmark-token"),
        Duration::from_secs(30),
        1,
        false,
    )
    .expect("build the application state");
    let router = v1::router(state);

    let cases = [
        make_case(
            &upstream_runtime,
            "small",
            SMALL_HEAD_BYTES,
            SMALL_BODY_BYTES,
        ),
        make_case(&upstream_runtime, "head-only", LARGE_HEAD_BYTES, 0),
        make_case(
            &upstream_runtime,
            "large",
            LARGE_HEAD_BYTES,
            LARGE_BODY_BYTES,
        ),
    ];

    {
        let mut group = c.benchmark_group("http-stack");

        for case in &cases {
            bench_case(&mut group, &runtime, &router, case);
        }

        group.finish();
    }

    for case in &cases {
        case.upstream.task.abort();
    }
}

criterion_group! {
    name = benches;
    config = pages::criterion_config();
    targets = http_benchmarks
}
criterion_main!(benches);
