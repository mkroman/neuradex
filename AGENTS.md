# AGENTS.md

## Overview

Small axum 0.8 HTTP service exposing LLM-agent tools as JSON under `/v1`: `fetch`, `peek` (document-head metadata only), and `search` (via Kagi). Single Rust crate, edition 2024, no workspace. Ships a Dockerfile + Helm chart (`chart/`) and a kind-based deployment-test workflow (`.github/workflows/deployment-test.yaml`); this file is the only instruction source.

## kagi dependency

`Cargo.toml` depends on `kagi = { git = "https://github.com/mkroman/zeta" }` — `kagi` is a workspace member of the `zeta` repo, resolved by name inside that repo (git+`path` combined is rejected by cargo). The lockfile pins the commit; `cargo update -p kagi` bumps it, and `--locked` builds (e.g. the Dockerfile) treat the lockfile as authoritative. Do not move shared helpers between the repos (dependency direction: neuradex → kagi, never the reverse).

## Verify

- `cargo fmt --check` — rustfmt defaults (no `rustfmt.toml`).
- `cargo clippy --all-targets` — must be warning-free. `all`, `pedantic`, and `nursery` are enabled as `warn` in `[lints]` and are treated as must-fix; `unsafe_code` is forbidden and `missing_docs` requires doc comments on all public items, including `# Errors` sections on `Result`-returning functions.
- `cargo test` — single test: `cargo test <name>`.
- Run order: fmt → clippy → test.

Tests are offline unit tests in inline `#[cfg(test)] mod tests`; nothing touches the network or external services.

## Running locally

- Requires `KAGI_SESSION_TOKEN` (search fails to start without it; non-empty). Optional: `LISTEN_ADDR` (default `127.0.0.1:8080`), `USER_AGENT`.
- `cargo run`.

## Non-obvious rules

- **Multi-value query params must go through `axum_extra::extract::Query`, not `axum::extract::Query`.** The latter uses `serde_urlencoded`, which 400s on repeated keys (`?include=redirects&include=headers`); axum-extra uses `serde_html_form`, which merges repeats into `Vec`. Optional lists use `Vec<T>` + `#[serde(default)]`, never `Option<Vec<T>>` (breaks for one occurrence).
- **Tests must exercise the real extraction path** — `Query::try_from_uri` on the actual `FetchQuery`/`SearchQuery` types — not a hand-rolled deserializer call. A mismatch here once shipped a bug where tests passed but production returned 400.
- **The HTML tokenizer (`html5ever`) is `!Send`.** `HeadParser` runs on `spawn_blocking`, bridged from the async reader via bounded `tokio::sync::mpsc` (async `send` / `blocking_recv`). Never use `std::sync::mpsc` or other blocking sends inside tokio tasks — that pins a worker thread per in-flight request.
- **Peek aborts the download early**: parsing stops at `</head>` or `<body>`, dropping the channel receiver, which cancels the reader task. Fetch reads the full body up to `FETCH_MAX_BYTES` (25 MiB); both report `truncated: true` only when bytes were actually cut (an exact-fit body is not truncated).
- Redirects are followed manually (wreq policy `none`) so each hop is counted and reported in metrics; the per-endpoint `include` allow-lists (`FETCH_INCLUDES` vs `PEEK_INCLUDES`) are enforced in the extractors, one `FetchParams`/`PeekParams`/`SearchParams` type per endpoint.

## Code layout

- `src/api/v1/extract.rs` — query parsing/validation; `src/api/v1/error.rs` — `ApiError`, the single rejection type rendering `{"error": {type, message}}`.
- `src/api/v1/redirect.rs` — manual redirect loop + `Fetched`; `src/api/v1/stream.rs` — body streaming/truncation/media-type gating.
- `src/metadata.rs` — streaming `HeadParser` (push-based tokenizer sink); `src/metrics.rs` — response metric types.

## Deployment

- `Dockerfile` — cargo-chef + `cargo auditable build --release --locked` on `rust:1.98-bookworm`, runtime `gcr.io/distroless/cc-debian12:nonroot` (includes the CA bundle kagi's `rustls-platform-verifier` needs). `LISTEN_ADDR=0.0.0.0:8080` is set in the image. `hack/install-cargo-tool.sh` installs the cargo tools from checksummed release binaries (identical to zeta's script; versions + manifest digests pinned in-file) instead of compiling them with `cargo install`.
- `chart/neuradex/` — Helm chart; secure-by-default (`existingSecret` for `KAGI_SESSION_TOKEN` by default, NetworkPolicy on, probes on `/healthz`).
- `.github/workflows/deployment-test.yaml` — `helm lint`/template variants plus a kind job: builds the image from the local source, loads it into the cluster, installs the chart with a dummy token, and asserts `/healthz` (204) and `/v1/fetch` (200) from an in-cluster curl pod.
