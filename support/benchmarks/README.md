# Benchmarks

The service has three Criterion benchmark targets and one concurrency harness. Run them on an
otherwise idle machine, pin the benchmark processes to a fixed CPU set, and compare runs made
with the same toolchain and CPU set.

## Criterion benchmarks

```sh
cargo bench -p neuradex --bench parsers
cargo bench -p neuradex --bench http-stack
cargo bench -p neuradex --bench search
```

`parsers` measures the head parser and the `scraper` tree builder over generated documents.
`http-stack` drives the production router in-process: the inbound hyper/TCP layer is excluded.
`search` drives the production router against the local `httpmock` Kagi server from
`test-support`; warm-up establishes the session, so steady-state iterations exclude session
setup and the live Kagi network.

Useful filters and options:

```sh
# Only the full-page parser cases, with a named baseline for later comparison.
cargo bench -p neuradex --bench parsers -- 'full_page' --save-baseline before

# Compare against the saved baseline (Criterion prints the change).
cargo bench -p neuradex --bench parsers -- 'full_page' --baseline before

# Fewer samples while iterating.
cargo bench -p neuradex --bench http-stack -- --sample-size 10 --measurement-time 1
```

`http-stack` reads two environment variables:

- `NEURADEX_BENCH_WORKERS` sets the benchmark runtime's worker count (default 2). The upstream
  runtime always uses 2.
- `NEURADEX_BENCH_TRACING` (`1`, `true`, `yes`, or `on`) installs a sink-backed
  `tracing_subscriber::fmt` subscriber with span-close events. This measures production
  span/audit formatting without stdout I/O.

## Concurrency harness

`crates/neuradex/examples/concurrency.rs` runs the production router over a real loopback socket
and drives it with a configurable number of concurrent requests. A child process of the same
example serves the upstream page; it can delay the response headers, trickle the document head,
or trickle the body. This exercises how long a request keeps a `spawn_blocking` thread parked in
the head parser.

```sh
cargo run --release -p neuradex --example concurrency -- \
    --scenario slow-head --endpoint peek --concurrency 512 --fast-concurrency 64 \
    --chunk-ms 250 --duration 10
```

Scenarios:

| Scenario | Upstream behavior |
|---|---|
| `fast` | Headers and body immediately |
| `slow-ttfb` | `--ttfb-ms` before the response headers, then the body immediately |
| `slow-head` | Trickle the head in `--chunk-bytes` writes every `--chunk-ms`; body immediately |
| `slow-body` | Head immediately; trickle the first `--slow-bytes` of the body |

Useful options: `--concurrency`, `--fast-concurrency` (adds a fast control group; the child
serves `/fast` without delays), `--workers` (service runtime workers), `--driver-workers`,
`--upstream-workers`, `--head-bytes`, `--body-bytes`, `--duration`. `--direct` bypasses the
service and drives the upstream child, which isolates harness/upstream effects from the service.

The harness prints one JSON object per run. `p99_ms` is the client-visible request latency,
`proc_p99_ms` subtracts the client's connect time, `svc_ttfb_p99_ms` is the service-reported time
to the upstream response headers, and `probe_p99_ms` is the queueing delay of an empty
`spawn_blocking` task submitted to the service runtime. The probe isolates blocking-pool
saturation from the request work. `fast_*` reports the fast control group separately.

Both loopback listeners are bound with a 4096 connection backlog through tokio's
`TcpSocket::listen`. Rust's
`std::net::TcpListener` (and therefore tokio's `TcpListener::bind`) uses a backlog of 128, so
concurrent bursts overflow the queue, drop SYNs, and show up as one-second retransmit delays that
look like service latency but are not.

### Interpreting the parser pool

The service parses heads on a process-wide `LocalSet` pool (`api/v1/parser_pool.rs`), not on
`spawn_blocking`: each parser waits for its next chunk as a `!Send` future, so a slow upstream
does not park an OS thread. `NEURADEX_PARSER_THREADS` overrides the pool size; the default is
`available_parallelism / 8`, clamped to 2 through 4.

Before the pool, a `peek` parser held one blocking thread from the first body chunk until the
head ended, and a `fetch` parser held one for the whole body. Tokio's blocking pool caps at 512
threads, so the 512th concurrent slow response filled it and new parsers queued behind the slow
ones. The slow requests themselves were not necessarily delayed — while a parser was queued, the
reader kept the channel full and the upstream's kernel buffers absorbed the early chunks. The
observable damage was head-of-line blocking for *other* requests. On a 4-second slow head with a
64-request fast control group, the pre-pool numbers were:

| Slow concurrency | Fast p50 | Fast p95 | Fast p99 | Blocking-pool probe p99 | Threads peak |
|---:|---:|---:|---:|---:|---:|
| 384 | 0.7 ms | 1.5 ms | 2.2 ms | 0.36 ms | 429 |
| 512 | 0.9 ms | 3.66 s | 3.71 s | 3.66 s | 520 |
| 640 | 27.5 ms | 3.98 s | 4.04 s | 3.90 s | 520 |

With the parser pool the same runs stay in the low milliseconds: at 512 and 640 slow heads the
fast p99 was 1.6 ms and 1.8 ms, the probe p99 was 0.44 ms and 0.36 ms, and `threads_peak` was 11
(2 parser threads plus the runtime threads) instead of 520. `probe_p99_ms` still measures an empty
`spawn_blocking` task; it now catches regressions that move parsing back onto the blocking pool.
The `fast_p95_ms`/`fast_p99_ms` columns are the primary signal.
