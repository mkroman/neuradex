# AGENTS.md

## Overview

Small axum 0.8 HTTP service exposing LLM-agent tools as JSON under `/v1`: `fetch`, `peek` (document-head metadata only), and `search` (via Kagi), plus `GET /healthz` (204). Single Rust crate, edition 2024, no workspace. Ships a Dockerfile + Helm chart (`chart/`) and a kind-based deployment-test workflow (`.github/workflows/deployment-test.yaml`); this file is the only instruction source except `support/opencode/README.md` (see "OpenCode tooling").

## kagi dependency

`Cargo.toml` depends on `kagi = { git = "https://github.com/mkroman/zeta" }` — `kagi` is a workspace member of the `zeta` repo, resolved by name inside that repo (git+`path` combined is rejected by cargo). The lockfile pins the commit; `cargo update -p kagi` bumps it, and `--locked` builds (e.g. the Dockerfile) treat the lockfile as authoritative. Do not move shared helpers between the repos (dependency direction: neuradex → kagi, never the reverse).

## Verify

- `cargo fmt --check` — rustfmt defaults (no `rustfmt.toml`).
- `cargo clippy --all-targets` — must be warning-free. `all`, `pedantic`, and `nursery` are enabled as `warn` in `[lints]` and are treated as must-fix; `unsafe_code` is forbidden and `missing_docs` requires doc comments on all public items, including `# Errors` sections on `Result`-returning functions.
- `cargo test` — single test: `cargo test <name>`.
- Run order: fmt → clippy → test.

Tests are offline unit tests in inline `#[cfg(test)] mod tests`; nothing touches the network or external services.

## Running locally

- Requires `KAGI_SESSION_TOKEN` (the binary exits at startup without a non-empty value). Optional: `LISTEN_ADDR` (default `127.0.0.1:8080`), `USER_AGENT`.
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
