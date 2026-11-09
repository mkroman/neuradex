//! Slow-upstream concurrency harness for `/v1/peek` and `/v1/fetch`.
//!
//! The production router is served over a real loopback TCP socket, and a child process of this
//! same example acts as the upstream. The upstream can delay its response headers, trickle the
//! document head, or trickle the body, which exercises how long a request keeps a
//! `spawn_blocking` thread parked in the head parser.
//!
//! ```text
//! cargo run --release --example concurrency -- \
//!     --scenario slow-head --concurrency 512 --fast-concurrency 64
//! ```
//!
//! The harness reports latency percentiles, request rate, peak RSS, peak OS thread count, and
//! the queueing delay of a probe task submitted to the service runtime's blocking pool. The
//! probe delay isolates blocking-pool saturation from the request work itself. `--fast-concurrency`
//! adds a fast control group so a saturated pool's head-of-line blocking is visible. `--direct`
//! bypasses the service and drives the upstream child directly.

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use neuradex::state::AppState;
use secrecy::SecretString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio::sync::Barrier;

/// The largest response the load client reads before it stops.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// How often the sampler reads `/proc/self/status`.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);

/// How often the blocking-pool probe submits an empty task.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// The per-request client timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The endpoint under load.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Endpoint {
    /// `/v1/peek`, which stops at the end of the document head.
    Peek,
    /// `/v1/fetch`, which reads the whole body.
    Fetch,
}

impl Endpoint {
    /// Returns the endpoint path.
    const fn path(self) -> &'static str {
        match self {
            Self::Peek => "peek",
            Self::Fetch => "fetch",
        }
    }
}

/// How the upstream delays its response.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Scenario {
    /// Serve headers and body as fast as the kernel accepts them.
    Fast,
    /// Delay the response headers, then serve the body fast.
    SlowTtfb,
    /// Trickle the document head, then serve the body fast.
    SlowHead,
    /// Serve the head fast, trickle the first `slow_bytes` of the body, then serve the rest.
    SlowBody,
}

impl Scenario {
    /// Returns the scenario name used in the report.
    const fn name(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::SlowTtfb => "slow-ttfb",
            Self::SlowHead => "slow-head",
            Self::SlowBody => "slow-body",
        }
    }
}

/// The harness command line.
#[derive(Clone, Debug, Parser)]
#[command(about = "Slow-upstream concurrency harness for the neuradex fetch endpoints")]
struct Cli {
    /// Serve the slow upstream instead of running the load test.
    #[arg(long, hide = true)]
    upstream: bool,
    /// The endpoint to drive.
    #[arg(long, value_enum, default_value_t = Endpoint::Peek)]
    endpoint: Endpoint,
    /// How the upstream delays its response.
    #[arg(long, value_enum, default_value_t = Scenario::SlowHead)]
    scenario: Scenario,
    /// Send requests to the upstream child directly, bypassing the service.
    #[arg(long)]
    direct: bool,
    /// Worker threads in the load driver runtime.
    #[arg(long, default_value_t = 2)]
    driver_workers: usize,
    /// Worker threads in the upstream child runtime.
    #[arg(long, default_value_t = 2)]
    upstream_workers: usize,
    /// Concurrent in-flight requests.
    #[arg(long, default_value_t = 128)]
    concurrency: usize,
    /// Additional fast requests sent alongside the slow ones.
    #[arg(long, default_value_t = 0)]
    fast_concurrency: usize,
    /// Measurement duration in seconds.
    #[arg(long, default_value_t = 5)]
    duration: u64,
    /// Worker threads in the service runtime.
    #[arg(long, default_value_t = 4)]
    workers: usize,
    /// Head size in bytes.
    #[arg(long, default_value_t = 64 * 1024)]
    head_bytes: usize,
    /// Body size in bytes.
    #[arg(long, default_value_t = 256 * 1024)]
    body_bytes: usize,
    /// Delay before the response headers, in milliseconds.
    #[arg(long, default_value_t = 400)]
    ttfb_ms: u64,
    /// Bytes per upstream write.
    #[arg(long, default_value_t = 4 * 1024)]
    chunk_bytes: usize,
    /// Delay between chunks in the slow region, in milliseconds.
    #[arg(long, default_value_t = 25)]
    chunk_ms: u64,
    /// Body bytes trickled in `slow-body` mode.
    #[arg(long, default_value_t = 64 * 1024)]
    slow_bytes: usize,
}

/// The generated upstream page and the offset where its head ends.
struct Page {
    /// The complete response body.
    bytes: Vec<u8>,
    /// The offset just past `</head>`.
    head_end: usize,
}

/// Builds an HTML page with `head_bytes` of head and `body_bytes` of body.
fn make_page(head_bytes: usize, body_bytes: usize) -> Page {
    const PREFIX: &[u8] = b"<!doctype html><html><head><title>Concurrency harness</title>";
    const HEAD_END: &[u8] = b"</head><body>";
    const TAIL: &[u8] = b"</body></html>";

    let mut bytes = Vec::with_capacity(head_bytes + body_bytes + PREFIX.len() + HEAD_END.len());
    bytes.extend_from_slice(PREFIX);

    while bytes.len() < head_bytes {
        bytes.push(b' ');
    }

    bytes.extend_from_slice(HEAD_END);
    let head_end = bytes.len();

    while bytes.len() < head_end + body_bytes {
        bytes.push(b'x');
    }

    bytes.extend_from_slice(TAIL);

    Page { bytes, head_end }
}

/// Reads one line from the upstream child and returns its bound address.
fn read_ready(child: &mut Child) -> SocketAddr {
    let stdout = child.stdout.take().expect("the upstream stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    let line = lines
        .next()
        .expect("the upstream prints a ready line")
        .expect("the upstream ready line is readable");

    line.strip_prefix("READY ")
        .expect("the upstream ready line starts with READY")
        .parse()
        .expect("the upstream ready line is a socket address")
}

/// The child process that serves the slow upstream.
struct Upstream {
    /// The child process.
    child: Child,
    /// The upstream base URL.
    url: String,
}

impl Upstream {
    /// Starts the upstream child with the same scenario options as the parent.
    fn start() -> Self {
        let executable = std::env::current_exe().expect("the current executable is known");
        let mut command = Command::new(executable);
        command
            .args(std::env::args_os().skip(1))
            .arg("--upstream")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().expect("the upstream child starts");
        let address = read_ready(&mut child);

        Self {
            child,
            url: format!("http://{address}/"),
        }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Binds a loopback listener with a large backlog.
///
/// `std::net::TcpListener`, and therefore `tokio::net::TcpListener::bind`, listens with a
/// backlog of 128. Bursts of concurrent connections overflow that queue, drop SYNs, and show up
/// as one-second retransmit delays. The harness needs a larger queue so it measures the service
/// instead of the kernel's default.
fn bind_listener() -> std::io::Result<std::net::TcpListener> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())?;
    socket.listen(4096)?;
    socket.set_nonblocking(true)?;

    Ok(socket.into())
}

/// Serves the configured page until the process is killed.
async fn serve_upstream(cli: Cli) {
    let page = Arc::new(make_page(cli.head_bytes, cli.body_bytes));
    let cli = Arc::new(cli);
    let listener = TcpListener::from_std(bind_listener().expect("bind the upstream listener"))
        .expect("register the upstream listener");
    let address = listener.local_addr().expect("read the upstream address");

    println!("READY {address}");
    std::io::stdout().flush().expect("flush the ready line");

    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let page = Arc::clone(&page);
        let cli = Arc::clone(&cli);

        tokio::spawn(async move {
            let _ = serve_connection(socket, &cli, &page).await;
        });
    }
}

/// Whether a chunk starting at `offset` belongs to the slow region.
const fn in_slow_region(cli: &Cli, page: &Page, offset: usize) -> bool {
    match cli.scenario {
        Scenario::Fast | Scenario::SlowTtfb => false,
        Scenario::SlowHead => offset < page.head_end,
        Scenario::SlowBody => {
            offset >= page.head_end && offset < page.head_end.saturating_add(cli.slow_bytes)
        }
    }
}

/// Serves one request with the configured delay shape.
async fn serve_connection(mut socket: TcpStream, cli: &Cli, page: &Page) -> std::io::Result<()> {
    let mut request = [0_u8; 1024];
    let read = socket.read(&mut request).await?;
    let fast = request[..read].starts_with(b"GET /fast ");

    if cli.scenario == Scenario::SlowTtfb && !fast {
        tokio::time::sleep(Duration::from_millis(cli.ttfb_ms)).await;
    }

    let headers = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        page.bytes.len()
    );
    socket.write_all(headers.as_bytes()).await?;

    let chunk_bytes = cli.chunk_bytes.max(1);
    let mut offset = 0;

    while offset < page.bytes.len() {
        let end = offset.saturating_add(chunk_bytes).min(page.bytes.len());
        socket.write_all(&page.bytes[offset..end]).await?;
        let slow = !fast && in_slow_region(cli, page, offset);
        offset = end;

        if slow {
            tokio::time::sleep(Duration::from_millis(cli.chunk_ms)).await;
        }
    }

    socket.shutdown().await
}

/// One `/proc/self/status` sample.
#[derive(Clone, Copy)]
struct Sample {
    /// Resident set size in KiB.
    rss_kib: u64,
    /// OS thread count.
    threads: u64,
}

/// Parses `VmRSS` and `Threads` from `/proc/self/status`.
fn read_status() -> Sample {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut sample = Sample {
        rss_kib: 0,
        threads: 0,
    };

    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            sample.rss_kib = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("Threads:") {
            sample.threads = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        }
    }

    sample
}

/// Samples RSS and thread count until stopped.
fn start_sampler() -> (Arc<AtomicBool>, std::thread::JoinHandle<Vec<Sample>>) {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        let mut samples = Vec::new();

        while !flag.load(Ordering::Relaxed) {
            samples.push(read_status());
            std::thread::sleep(SAMPLE_INTERVAL);
        }

        samples.push(read_status());

        samples
    });

    (stop, handle)
}

/// One request's timing breakdown.
struct Outcome {
    /// The response status code.
    status: u16,
    /// Time spent connecting, in nanoseconds.
    connect: u64,
    /// Time from the request start to the first response byte, in nanoseconds.
    first_byte: u64,
    /// The service-reported total time, for peek responses.
    service_total_ms: Option<u64>,
    /// The service-reported time to first byte, for peek responses.
    service_ttfb_ms: Option<u64>,
}

/// Extracts an integer JSON field from a response body.
fn json_number(body: &[u8], key: &str) -> Option<u64> {
    let text = std::str::from_utf8(body).ok()?;
    let rest = &text[text.find(key)?.saturating_add(key.len())..];
    let digits: String = rest
        .chars()
        .skip_while(|character| !character.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();

    digits.parse().ok()
}

/// The measurements from one worker.
struct WorkerResult {
    /// Whether this worker sent the fast control requests.
    fast: bool,
    /// Request latencies in nanoseconds.
    latencies: Vec<u64>,
    /// Connect latencies in nanoseconds.
    connects: Vec<u64>,
    /// First-byte latencies in nanoseconds.
    first_bytes: Vec<u64>,
    /// Request times excluding the connect, in nanoseconds.
    processing: Vec<u64>,
    /// Service-reported total times in milliseconds.
    service_totals: Vec<u64>,
    /// Service-reported time-to-first-byte values in milliseconds.
    service_ttfbs: Vec<u64>,
    /// Requests that failed or returned a non-200 status.
    failures: u64,
}

/// Parses the numeric status code out of an HTTP response prefix.
fn parse_status(prefix: &[u8]) -> u16 {
    let text = String::from_utf8_lossy(prefix);
    text.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

/// Sends one request and reads the complete response.
async fn one_request(
    request: &str,
    address: SocketAddr,
    capture: bool,
) -> std::io::Result<Outcome> {
    let started = Instant::now();
    let mut stream = TcpStream::connect(address).await?;
    let connect = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    stream.set_nodelay(true)?;
    stream.write_all(request.as_bytes()).await?;

    let mut status = [0_u8; 16];
    let mut filled = 0;
    let mut buffer = [0_u8; 16 * 1024];
    let mut total: usize = 0;
    let mut first_byte = None;
    let mut body = Vec::new();

    loop {
        let read = stream.read(&mut buffer).await?;

        if read == 0 {
            break;
        }

        if first_byte.is_none() {
            first_byte = Some(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }

        if filled < status.len() {
            let take = read.min(status.len() - filled);
            status[filled..filled + take].copy_from_slice(&buffer[..take]);
            filled += take;
        }

        if capture && body.len() < 128 * 1024 {
            let take = read.min(128 * 1024 - body.len());
            body.extend_from_slice(&buffer[..take]);
        }

        total = total.saturating_add(read);

        if total >= MAX_RESPONSE_BYTES {
            break;
        }
    }

    Ok(Outcome {
        status: parse_status(&status[..filled]),
        connect,
        first_byte: first_byte.unwrap_or(connect),
        service_total_ms: capture
            .then(|| json_number(&body, "\"total_ms\":"))
            .flatten(),
        service_ttfb_ms: capture
            .then(|| json_number(&body, "\"ttfb_ms\":"))
            .flatten(),
    })
}

/// Returns the next value from a small xorshift generator.
const fn next_random(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}

/// The shared state and per-group settings for one worker.
#[derive(Clone)]
struct Workload {
    /// The request every worker in the group sends.
    request: Arc<str>,
    /// The service address.
    address: SocketAddr,
    /// The time the workers stop.
    deadline: Instant,
    /// The start barrier, shared by every worker and the driver.
    barrier: Arc<Barrier>,
    /// The number of requests currently started but not finished.
    in_flight: Arc<AtomicUsize>,
    /// The largest observed in-flight count.
    max_in_flight: Arc<AtomicUsize>,
    /// Whether this group is the fast control group.
    fast: bool,
}

/// Drives requests until the deadline and records each latency.
async fn worker(id: usize, workload: Workload) -> WorkerResult {
    let Workload {
        request,
        address,
        deadline,
        barrier,
        in_flight,
        max_in_flight,
        fast,
    } = workload;
    let mut result = WorkerResult {
        fast,
        latencies: Vec::new(),
        connects: Vec::new(),
        first_bytes: Vec::new(),
        processing: Vec::new(),
        service_totals: Vec::new(),
        service_ttfbs: Vec::new(),
        failures: 0,
    };
    let mut random = u64::try_from(id)
        .unwrap_or(0)
        .wrapping_add(0x9e37_79b9_7f4a_7c15);

    barrier.wait().await;

    // Stagger the first request so the workers do not all reconnect in lockstep. A synchronized
    // reconnect burst overflows the listen queue and shows up as a one-second TCP retransmit
    // tail that has nothing to do with the service under test.
    tokio::time::sleep(Duration::from_millis(next_random(&mut random) % 100)).await;

    loop {
        let started = Instant::now();
        let concurrent = in_flight.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        max_in_flight.fetch_max(concurrent, Ordering::Relaxed);
        let capture = request.starts_with("GET /v1/");
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            Box::pin(one_request(&request, address, capture)),
        )
        .await
        {
            Ok(Ok(outcome)) if outcome.status == 200 => {
                result
                    .latencies
                    .push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
                let total = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
                result.connects.push(outcome.connect);
                result.first_bytes.push(outcome.first_byte);
                result
                    .processing
                    .push(total.saturating_sub(outcome.connect));

                if let Some(service_total) = outcome.service_total_ms {
                    result.service_totals.push(service_total);
                }

                if let Some(service_ttfb) = outcome.service_ttfb_ms {
                    result.service_ttfbs.push(service_ttfb);
                }
            }
            Ok(_) | Err(_) => result.failures = result.failures.saturating_add(1),
        }

        in_flight.fetch_sub(1, Ordering::Relaxed);

        if Instant::now() >= deadline {
            break;
        }

        tokio::time::sleep(Duration::from_millis(next_random(&mut random) % 25)).await;
    }

    result
}

/// Returns the `percent` percentile of sorted values.
fn percentile(sorted: &[u64], percent: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }

    let index = usize::try_from(percent)
        .unwrap_or(100)
        .saturating_mul(sorted.len())
        .div_ceil(100)
        .saturating_sub(1);

    sorted[index.min(sorted.len() - 1)]
}

/// Converts nanoseconds to milliseconds.
fn millis(nanos: u64) -> f64 {
    Duration::from_nanos(nanos).as_secs_f64() * 1_000.0
}

/// The measurements collected during one run.
struct Measurement {
    /// Completed requests.
    requests: usize,
    /// Failed requests.
    failures: u64,
    /// Wall time from the start signal to the last completed request.
    elapsed: Duration,
    /// Request latencies in nanoseconds.
    latencies: Vec<u64>,
    /// Connect latencies in nanoseconds.
    connects: Vec<u64>,
    /// First-byte latencies in nanoseconds.
    first_bytes: Vec<u64>,
    /// Request times excluding the connect, in nanoseconds.
    processing: Vec<u64>,
    /// Fast control request latencies in nanoseconds.
    fast_latencies: Vec<u64>,
    /// Fast control requests that failed.
    fast_failures: u64,
    /// Service-reported total times in milliseconds.
    service_totals: Vec<u64>,
    /// Service-reported time-to-first-byte values in milliseconds.
    service_ttfbs: Vec<u64>,
    /// The largest observed number of in-flight client requests.
    max_in_flight: usize,
    /// Blocking-pool probe delays in nanoseconds.
    probe: Vec<u64>,
}

/// Prints the harness report as one JSON object.
#[expect(
    clippy::cast_precision_loss,
    reason = "the report is a human-facing summary, not a value used in measurements"
)]
fn report(cli: &Cli, measurement: &Measurement, baseline: &Sample, peak: &Sample) {
    let mut sorted = measurement.latencies.clone();
    sorted.sort_unstable();
    let mut connect_sorted = measurement.connects.clone();
    connect_sorted.sort_unstable();
    let mut first_byte_sorted = measurement.first_bytes.clone();
    first_byte_sorted.sort_unstable();
    let mut processing_sorted = measurement.processing.clone();
    processing_sorted.sort_unstable();
    let mut fast_sorted = measurement.fast_latencies.clone();
    fast_sorted.sort_unstable();
    let mut service_total_sorted = measurement.service_totals.clone();
    service_total_sorted.sort_unstable();
    let mut service_ttfb_sorted = measurement.service_ttfbs.clone();
    service_ttfb_sorted.sort_unstable();
    let mut probe_sorted = measurement.probe.clone();
    probe_sorted.sort_unstable();

    let seconds = measurement.elapsed.as_secs_f64();
    let rps = measurement.requests as f64 / seconds.max(f64::MIN_POSITIVE);
    let mib = |kib: u64| kib as f64 / 1024.0;

    println!(
        "{{\"scenario\":\"{}\",\"endpoint\":\"{}\",\"direct\":{},\"concurrency\":{},\"duration_s\":{},\
         \"requests\":{},\"failures\":{},\"rps\":{:.1},\
         \"p50_ms\":{:.1},\"p95_ms\":{:.1},\"p99_ms\":{:.1},\"max_ms\":{:.1},\
         \"connect_p99_ms\":{:.1},\"first_byte_p99_ms\":{:.1},\
         \"proc_p50_ms\":{:.1},\"proc_p95_ms\":{:.1},\"proc_p99_ms\":{:.1},\
         \"svc_total_p95_ms\":{},\"svc_total_p99_ms\":{},\
         \"svc_ttfb_p95_ms\":{},\"svc_ttfb_p99_ms\":{},\"max_in_flight\":{},\
         \"fast_requests\":{},\"fast_failures\":{},\
         \"fast_p50_ms\":{:.1},\"fast_p95_ms\":{:.1},\"fast_p99_ms\":{:.1},\
         \"rss_baseline_mib\":{:.1},\"rss_peak_mib\":{:.1},\
         \"threads_baseline\":{},\"threads_peak\":{},\"blocking_threads_peak\":{},\
         \"probe_p50_ms\":{:.2},\"probe_p99_ms\":{:.2},\"probe_max_ms\":{:.2}}}",
        cli.scenario.name(),
        cli.endpoint.path(),
        cli.direct,
        cli.concurrency,
        cli.duration,
        measurement.requests,
        measurement.failures,
        rps,
        millis(percentile(&sorted, 50)),
        millis(percentile(&sorted, 95)),
        millis(percentile(&sorted, 99)),
        millis(sorted.last().copied().unwrap_or(0)),
        millis(percentile(&connect_sorted, 99)),
        millis(percentile(&first_byte_sorted, 99)),
        millis(percentile(&processing_sorted, 50)),
        millis(percentile(&processing_sorted, 95)),
        millis(percentile(&processing_sorted, 99)),
        percentile(&service_total_sorted, 95),
        percentile(&service_total_sorted, 99),
        percentile(&service_ttfb_sorted, 95),
        percentile(&service_ttfb_sorted, 99),
        measurement.max_in_flight,
        measurement.fast_latencies.len(),
        measurement.fast_failures,
        millis(percentile(&fast_sorted, 50)),
        millis(percentile(&fast_sorted, 95)),
        millis(percentile(&fast_sorted, 99)),
        mib(baseline.rss_kib),
        mib(peak.rss_kib.max(baseline.rss_kib)),
        baseline.threads,
        peak.threads.max(baseline.threads),
        peak.threads.saturating_sub(baseline.threads),
        millis(percentile(&probe_sorted, 50)),
        millis(percentile(&probe_sorted, 99)),
        millis(probe_sorted.last().copied().unwrap_or(0)),
    );
}

/// Runs one load-test scenario.
fn run(cli: &Cli) {
    let upstream = Upstream::start();
    let service_runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(cli.workers)
            .thread_name("neuradex-load-service")
            .build()
            .expect("build the service runtime"),
    );
    let listener = service_runtime.block_on(async {
        TcpListener::from_std(bind_listener().expect("bind the service listener"))
            .expect("register the service listener")
    });
    let address = listener.local_addr().expect("read the service address");

    let state = AppState::try_new(
        "neuradex-concurrency-harness",
        SecretString::from("harness-token"),
        Duration::from_secs(30),
        1,
        false,
    )
    .expect("build the application state");
    let server = service_runtime.spawn(async move {
        axum::serve(listener, neuradex::api::v1::router(state))
            .await
            .expect("serve the application");
    });

    let driver = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(cli.driver_workers)
        .thread_name("neuradex-load-driver")
        .build()
        .expect("build the driver runtime");

    let (request, target) = if cli.direct {
        let target = upstream
            .url
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .parse()
            .expect("the upstream address is valid");
        let request: Arc<str> =
            Arc::from("GET / HTTP/1.1\r\nhost: upstream\r\nconnection: close\r\n\r\n".to_owned());

        (request, target)
    } else {
        let path = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("url", &upstream.url)
            .finish();
        let request: Arc<str> = Arc::from(format!(
            "GET /v1/{}?{path} HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n",
            cli.endpoint.path()
        ));

        (request, address)
    };

    let fast_request = (cli.fast_concurrency > 0 && !cli.direct).then(|| {
        let fast_url = format!("{}fast", upstream.url);
        let path = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("url", &fast_url)
            .finish();
        let request: Arc<str> = Arc::from(format!(
            "GET /v1/{}?{path} HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n",
            cli.endpoint.path()
        ));

        request
    });

    driver.block_on(drive(
        cli,
        &service_runtime,
        &request,
        fast_request.as_deref(),
        target,
    ));

    server.abort();
    drop(upstream);
}

/// Runs the timed load and reports the measurements.
async fn drive(
    cli: &Cli,
    service_runtime: &Arc<Runtime>,
    request: &str,
    fast_request: Option<&str>,
    address: SocketAddr,
) {
    let warmup = Box::pin(one_request(
        request,
        address,
        cli.endpoint == Endpoint::Peek,
    ))
    .await
    .expect("the warm-up request completes");
    assert_eq!(warmup.status, 200, "the warm-up request succeeds");

    let baseline = read_status();
    let (stop, sampler) = start_sampler();
    let barrier = Arc::new(Barrier::new(
        cli.concurrency
            .saturating_add(cli.fast_concurrency)
            .saturating_add(1),
    ));
    let deadline = Instant::now() + Duration::from_secs(cli.duration);
    let mut tasks = tokio::task::JoinSet::new();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let workload = Workload {
        request: Arc::from(request),
        address,
        deadline,
        barrier: Arc::clone(&barrier),
        in_flight,
        max_in_flight: Arc::clone(&max_in_flight),
        fast: false,
    };

    spawn_workers(&mut tasks, &workload, cli.concurrency);

    if let Some(fast_request) = fast_request {
        let fast_workload = Workload {
            request: Arc::from(fast_request),
            fast: true,
            ..workload
        };

        spawn_workers(&mut tasks, &fast_workload, cli.fast_concurrency);
    }

    let (probe_stop_tx, mut probe_stop_rx) = tokio::sync::watch::channel(false);
    let probe_runtime = Arc::clone(service_runtime);
    let probe = tokio::spawn(async move {
        let mut delays = Vec::new();
        let mut ticker = tokio::time::interval(PROBE_INTERVAL);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let started = Instant::now();
                    let handle = probe_runtime.spawn_blocking(|| ());
                    let _ = handle.await;
                    delays.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
                }
                _ = probe_stop_rx.changed() => break,
            }
        }

        delays
    });

    let started = Instant::now();
    barrier.wait().await;

    let mut totals = Aggregate::default();

    while let Some(result) = tasks.join_next().await {
        totals.merge(&result.expect("the load worker completes"));
    }

    let elapsed = started.elapsed();
    let _ = probe_stop_tx.send(true);
    let probe = probe.await.expect("the probe task completes");
    stop.store(true, Ordering::Relaxed);
    let readings = sampler.join().expect("the sampler completes");
    let mut peak = baseline;

    for sample in &readings {
        peak.rss_kib = peak.rss_kib.max(sample.rss_kib);
        peak.threads = peak.threads.max(sample.threads);
    }

    let measurement = Measurement {
        requests: totals.latencies.len(),
        failures: totals.failures,
        elapsed,
        latencies: totals.latencies,
        connects: totals.connects,
        first_bytes: totals.first_bytes,
        processing: totals.processing,
        fast_latencies: totals.fast_latencies,
        fast_failures: totals.fast_failures,
        service_totals: totals.service_totals,
        service_ttfbs: totals.service_ttfbs,
        max_in_flight: max_in_flight.load(Ordering::Relaxed),
        probe,
    };

    report(cli, &measurement, &baseline, &peak);
}

/// Spawns `count` workers for one workload group.
fn spawn_workers(
    tasks: &mut tokio::task::JoinSet<WorkerResult>,
    workload: &Workload,
    count: usize,
) {
    for id in 0..count {
        tasks.spawn(worker(id, workload.clone()));
    }
}

/// The merged results of every worker.
#[derive(Default)]
struct Aggregate {
    /// Request latencies in nanoseconds.
    latencies: Vec<u64>,
    /// Connect latencies in nanoseconds.
    connects: Vec<u64>,
    /// First-byte latencies in nanoseconds.
    first_bytes: Vec<u64>,
    /// Request times excluding the connect, in nanoseconds.
    processing: Vec<u64>,
    /// Fast control request latencies in nanoseconds.
    fast_latencies: Vec<u64>,
    /// Fast control requests that failed.
    fast_failures: u64,
    /// Service-reported total times in milliseconds.
    service_totals: Vec<u64>,
    /// Service-reported time-to-first-byte values in milliseconds.
    service_ttfbs: Vec<u64>,
    /// Requests that failed.
    failures: u64,
}

impl Aggregate {
    /// Merges one worker's results.
    fn merge(&mut self, result: &WorkerResult) {
        if result.fast {
            self.fast_latencies.extend_from_slice(&result.latencies);
            self.fast_failures = self.fast_failures.saturating_add(result.failures);

            return;
        }

        self.latencies.extend_from_slice(&result.latencies);
        self.connects.extend_from_slice(&result.connects);
        self.first_bytes.extend_from_slice(&result.first_bytes);
        self.processing.extend_from_slice(&result.processing);
        self.service_totals
            .extend_from_slice(&result.service_totals);
        self.service_ttfbs.extend_from_slice(&result.service_ttfbs);
        self.failures = self.failures.saturating_add(result.failures);
    }
}

/// Runs either the load test or the upstream child.
fn main() {
    let cli = Cli::parse();

    if cli.upstream {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(cli.upstream_workers)
            .thread_name("neuradex-load-upstream")
            .build()
            .expect("build the upstream runtime");
        runtime.block_on(serve_upstream(cli));
    } else {
        run(&cli);
    }
}
