# AGENTS.md

## Overview

Small axum 0.8 HTTP service exposing LLM-agent tools as JSON under `/v1`: `fetch`, `peek` (document-head metadata only), and `search` (via Kagi), plus `GET /healthz` (204). Cargo workspace (edition 2024): the service is the root package (`neuradex`) and the Kagi client/parser is vendored at `crates/kagi` (copied from the `zeta` repo). Ships a Dockerfile + Helm chart (`chart/`) and a kind-based deployment-test workflow (`.github/workflows/deployment-test.yaml`); this file is the only instruction source except `support/opencode/README.md` (see "OpenCode tooling").

## kagi crate

`crates/kagi` is a workspace copy of the `kagi` crate from `https://github.com/mkroman/zeta` (taken from zeta's HEAD at vendor time; the crate's `repository` field still points there). The root package depends on it via `kagi = { path = "crates/kagi", ... }`. To update it, re-copy the crate from a zeta checkout and re-apply the vendoring deltas (see below). Never re-point the dependency back at git+`https://github.com/mkroman/zeta` in this workspace.

Vendoring deltas vs upstream: shared deps (`htmlize`, `regex`, `reqwest`, `scraper`, `secrecy`, `serde`, `serde_json`, `thiserror`, `tokio`, `tracing`) use `workspace = true` and are pinned in the root `[workspace.dependencies]`; the root workspace table owns the tokio feature set (add `features` on top of it in member manifests). The crate is marked `publish = false` (it ships only as a build-time dependency, not to crates.io). The crate also carries a **local divergence from upstream**: a session pool in `client.rs` (per-request checkout, one-at-a-time establishment gated by `creation`, per-session refresh cycles, and bounded exponential backoff after failed fetches) — a future re-vendor must re-apply it; the pool's behavior is pinned by the client tests. Do not move shared helpers between the repos (dependency direction: neuradex → kagi, never the reverse).

## Verify

- `cargo fmt --check` — rustfmt defaults (no `rustfmt.toml`).
- `cargo clippy --all-targets --workspace` — must be warning-free across all members. `all`, `pedantic`, and `nursery` are enabled as `warn` in `[lints]` and are treated as must-fix; `unsafe_code` is forbidden and `missing_docs` requires doc comments on all public items, including `# Errors` sections on `Result`-returning functions. The vendored `crates/kagi` carries the same lint set.
- `cargo test --workspace` — runs the root package plus `crates/kagi` (a root-package workspace defaults to the root package only without `--workspace`; `crates/kagi` has 8 unit tests + 1 doc test that plain `cargo test` would skip). Single test: `cargo test -p neuradex <name>`.
- Run order: fmt → clippy → test.

Tests are offline unit tests in inline `#[cfg(test)] mod tests`; nothing touches the network or external services.

## Running locally

- Requires `KAGI_SESSION_TOKEN` (the binary exits at startup without a non-empty value). Optional: `LISTEN_ADDR` (default `127.0.0.1:8080`), `USER_AGENT`, `KAGI_MAX_SESSIONS` (default `kagi::DEFAULT_MAX_SESSIONS`, 2 — the pool's simultaneous-session cap).
- `cargo run`.
- Local builds need `cmake` + `libclang` because wreq compiles BoringSSL (and bindgen needs libclang) — the Dockerfile installs both for this reason.

## Non-obvious rules

- **Multi-value query params must go through `axum_extra::extract::Query`, not `axum::extract::Query`.** The latter uses `serde_urlencoded`, which 400s on repeated keys (`?include=redirects&include=headers`); axum-extra uses `serde_html_form`, which merges repeats into `Vec`. Optional lists use `Vec<T>` + `#[serde(default)]`, never `Option<Vec<T>>` (breaks for one occurrence).
- **Tests must exercise the real extraction path** — `Query::try_from_uri` on the actual `FetchQuery`/`SearchQuery` types — not a hand-rolled deserializer call. A mismatch here once shipped a bug where tests passed but production returned 400.
- **The HTML tokenizer (`html5ever`) is `!Send`.** `HeadParser` runs on `spawn_blocking`, bridged from the async reader via bounded `tokio::sync::mpsc` (async `send` / `blocking_recv`). Never use `std::sync::mpsc` or other blocking sends inside tokio tasks — that pins a worker thread per in-flight request.
- **Peek aborts the download early**: parsing stops at `</head>` or `<body>`, dropping the channel receiver, which cancels the reader task (hard cap `HEAD_MAX_BYTES`, 2 MiB). Fetch reads the full body up to `FETCH_MAX_BYTES` (25 MiB); both report `truncated: true` only when bytes were actually cut (an exact-fit body is not truncated).
- Redirects are followed manually (wreq policy `none`) so each hop is counted and reported in metrics; the per-endpoint `include` allow-lists (`FETCH_INCLUDES` vs `PEEK_INCLUDES`) are enforced in the extractors, one `FetchParams`/`PeekParams`/`SearchParams` type per endpoint.

## Code layout

- `src/main.rs` → `src/http.rs` — binary entry point and `serve` (bind + graceful shutdown on ctrl-c/SIGTERM); `src/api/v1/{fetch,peek,search}.rs` — the three endpoint handlers, wired in `src/api/v1.rs` (`router`, `AppState`, wreq client with `Policy::none()`).
- `src/api/v1/extract.rs` — query parsing/validation; `src/api/v1/error.rs` — `ApiError`, the single rejection type rendering `{"error": {type, message}}`.
- `src/api/v1/redirect.rs` — manual redirect loop + `Fetched`; `src/api/v1/stream.rs` — body streaming/truncation/media-type gating, and the async reader → `spawn_blocking` bridge (bounded mpsc) the `!Send`-tokenizer rule refers to.
- `src/metadata.rs` — streaming `HeadParser` (push-based tokenizer sink); `src/metrics.rs` — response metric types.

## Deployment

- `Dockerfile` — cargo-chef + `cargo auditable build --release --locked` on `rust:1.98-bookworm`, runtime `gcr.io/distroless/cc-debian12:nonroot` (includes the CA bundle kagi's `rustls-platform-verifier` needs). `LISTEN_ADDR=0.0.0.0:8080` is set in the image. `hack/install-cargo-tool.sh` installs the cargo tools from checksummed release binaries (identical to zeta's script; versions + manifest digests pinned in-file) instead of compiling them with `cargo install`.
- `chart/neuradex/` — Helm chart; secure-by-default (`existingSecret` for `KAGI_SESSION_TOKEN` by default, NetworkPolicy on, probes on `/healthz`). `templates/secret.yaml` `fail`s at render time unless `existingSecret` **or** `kagiSessionToken` is set — bare `helm lint chart/neuradex`/`helm template` fails; use `--set kagiSessionToken=ci-dummy` (what CI does) or `--set existingSecret=<name>`.
- `.github/workflows/deployment-test.yaml` — `helm lint`/template variants plus a kind job: builds the image from the local source, loads it into the cluster, installs the chart with a dummy token, and asserts `/healthz` (204) and `/v1/fetch` (200) from an in-cluster curl pod.

## OpenCode tooling

`support/opencode/plugins/websearch.ts` is an opencode v2 plugin (`@opencode/plugin`, version-pinned to the app release) that registers this service's `/v1/search` as a `websearch` provider and selects it as the default — opencode's built-in `websearch` tool then searches through this service (`NEURADEX_BASE_URL`, default `http://127.0.0.1:8080`); if websearch errors, it is this service not running, not opencode itself. The `@opencode/plugin` package must be installed where the plugin resolves imports (the global config dir for the symlink setup, since local files get no auto-install) — see `support/opencode/README.md`.
