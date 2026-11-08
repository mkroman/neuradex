# AGENTS.md

axum 0.8 HTTP service exposing LLM-agent tools as JSON: `GET /v1/{fetch,peek,search}`, `POST /v1/openwebui_search` (Open WebUI `external` search shape: `{"query","count"}` in, bare `[{link,title,snippet}]` out), `GET /healthz` (204). Feature-gated: `/openapi.json` (`utoipa`) and the `/docs` page (`docs`; legacy `/swagger-ui[/]` 302 there).

## Workspace

- Virtual workspace (edition 2024, resolver 3). `default-members = ["crates/neuradex"]`, so **root `cargo test`/`clippy` only cover the service** — pass `--workspace` for all members. Every direct child of `crates/` must be a package (glob `members`).
- `crates/neuradex` — the service (binary + lib). Entry: `src/main.rs` (clap) → `src/http.rs::serve`; routes in `src/api/v1.rs`.
- `crates/neuradex-docs` — renders `/docs` from an `utoipa::openapi::OpenApi`; `render()` is the only entry point the service uses.
- `crates/kagi` — vendored copy of `kagi` from `github.com/mkroman/zeta` (its `repository` field still points there). Never re-point the dep at the zeta git repo. It carries a **local session-pool divergence** in `client.rs` (pinned by its tests) that a re-vendor must re-apply. A re-vendor also overwrites its `Cargo.toml`, so redo the workspace wiring: shared deps as `workspace = true`, `version`/`edition`/`license` from `[workspace.package]`, `[lints] workspace = true`, `publish = false`. Dependency direction: neuradex → kagi / neuradex-docs, never the reverse; don't move helpers between this repo and zeta.
- `crates/test-support` — shared HTTP test harnesses (`KagiHarness`, `TestApp`) built on `httpmock`; production crates depend on it only under `[dev-dependencies]`. It is feature-gated (`client` for kagi tests, `app` for service tests) so client tests do not build the whole service, and it depends on the crates under test (the allowed dev-dependency cycle). Keep harness code here rather than in `tests/common` modules.
- Lints live only in root `[workspace.lints]` (clippy `all`/`pedantic`/`nursery` as warn, `unsafe_code` forbidden, `missing_docs` incl. `# Errors` sections). Members use `[lints] workspace = true`; Cargo rejects per-member overrides.
- Shared deps (and the tokio feature set) are pinned in `[workspace.dependencies]`; members add features on top. New dependencies must be justified.
- utoipa's `macros` feature can't be dropped (utoipa 6.0.0 doesn't compile without it), even for neuradex-docs, which never uses the macros.

## Verify

Build needs `cmake`, `clang` and `libclang-dev` (wreq compiles BoringSSL). Full builds are slow, so iterate with targeted tests (`cargo test -p neuradex <name>`, `-p kagi`, `-p neuradex-docs`). Run fmt and clippy only once, at the end, in this order:

1. `cargo fmt`
2. `cargo clippy --all-targets --workspace -- -D warnings` (warnings are must-fix)
3. `cargo test --workspace`
4. Feature matrix (CI runs all three; use `cargo test`, not `check`, so router tests run in both router shapes). Keep `-p neuradex`: combining `--no-default-features` with `--workspace` also strips the other members' defaults.
   - `cargo test -p neuradex --no-default-features` (no utoipa at all)
   - `cargo test -p neuradex --no-default-features --features utoipa`

Also in CI: `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` (all-features pulls in kagi's native-tls/OpenSSL), `shellcheck hack/*.sh`, `cargo-deny ... check advisories` (only advisories gate). Tests are offline: unit tests are inline `#[cfg(test)]` modules; HTTP integration tests use the `test-support` harness against local `httpmock` servers. Kagi fixtures live in `crates/test-support/fixtures/kagi` and are read through `test_support::kagi::fixture`/`read_fixture`.

## Running locally

- `cargo run` serves. It needs a non-empty `KAGI_SESSION_TOKEN`. Optional: `LISTEN_ADDR` (default `127.0.0.1:8080`), `USER_AGENT`, `KAGI_MAX_SESSIONS` (default 2).
- `cargo run -- export-docs <out.html> [--spec <out.json>]` writes the docs page without building `AppState` (no token needed). It only works when built with `docs`.
- In debug builds, `/docs` re-reads `crates/neuradex-docs/theme/*` from disk on every request. Release builds embed and cache it. New theme files must be added to the ordered lists in `crates/neuradex-docs/src/assets.rs`.
- Unexpected 400s: `RUST_LOG=axum::rejection=trace`.

## Non-obvious rules

- **Routes and OpenAPI come from one place**: register `#[cfg_attr(feature = "utoipa", utoipa::path(...))]` handlers via `routes!` in `api()` (`src/api/v1.rs`). Never use `.route(...)` for them or hand-list paths. Exceptions:
  - The `#[cfg(not(feature = "utoipa"))]` `api()` must hand-register the same five paths. The unconditional router tests catch drift.
  - `/docs`, `/openapi.json`, `/swagger-ui` and the 404/405 fallbacks are hand-registered in `router()`.
- **Every error is `{"error": {type, message}}`**, including the router 404/405 fallbacks. 5xx bodies are generic; details go to tracing only. A new error path needs two tests: a router oneshot test in `src/api/v1.rs` and a variant mapping test in `src/api/v1/error.rs`. `ErrorBody` must stay in `components(schemas(...))` in `openapi.rs`, because the manual `IntoResponses` impl is invisible to utoipa. That impl (`error.rs`) hand-lists the documented statuses (400/415/500/502): a new status must be added there, and new handlers list `ApiError` in their `utoipa::path` responses, or the error is missing from `/openapi.json`.
- **Never extract `Query<T>`/`Json<T>` directly.** Use `ApiQuery<T>`/`ApiJson<T>` (`extract.rs`), which turn rejections into the JSON envelope. `ApiQuery` wraps `axum_extra::extract::Query` (serde_html_form) so repeated keys work. Optional lists are `Vec<T>` + `#[serde(default)]`, never `Option<Vec<T>>`.
- **The wire types are the single source of truth**: `FetchParams`/`SearchParams`/`OpenWebUiSearchRequest` handle deserialization, validation methods, and `IntoParams` docs. A new query param needs all three. Each endpoint owns its `include` allow-list (`FETCH_INCLUDES` vs `PEEK_INCLUDES`).
- Tests must go through the real extraction path (`Query::try_from_uri` on the real types), not a hand-rolled deserializer. That mismatch once shipped a production 400.
- `html5ever` is `!Send`: `HeadParser` runs on `spawn_blocking`, fed by a bounded `tokio::sync::mpsc` (async `send` / `blocking_recv`) in `stream.rs`. Never use `std::sync::mpsc` or blocking sends in tokio tasks.
- Peek stops at `</head>`/`<body>` (cap `HEAD_MAX_BYTES` 2 MiB). Fetch reads up to `FETCH_MAX_BYTES` (25 MiB). `truncated: true` only when bytes were actually cut.
- The wreq client uses `Policy::none()` because redirects are followed manually in `redirect.rs`, so each hop is counted in metrics.
- Reserve the `Validated*` prefix for extractors that validate. Scope middleware per route, not as a global layer with path predicates.
- `support/design/api-docs-design.md`: the single-file/no-runtime-fetch constraints are superseded (the page is rendered at runtime); its tokens and layout still apply.

## Deployment / CI

- Helm chart `chart/neuradex`: `helm lint`/`template` fail unless `--set kagiSessionToken=ci-dummy` or `--set existingSecret=<name>` is passed.
- `deployment-test.yaml` builds the image, installs the chart into kind, and pipes `hack/smoke-test.sh` into a curl pod. Update the script when you change `/healthz`, `/v1/fetch`, `/openapi.json` or `/docs`.
- The image binds `LISTEN_ADDR=0.0.0.0:8080`. Runtime is distroless (CA bundle required by kagi's rustls-platform-verifier). The Docker build copies only `Cargo.toml`, `Cargo.lock` and `crates/` (and `.dockerignore` excludes `support/`, `chart/` etc.), so anything else the build needs must be added to the `Dockerfile`.
- The chart's NetworkPolicy is on by default and only allows egress to DNS (53) and ports 80/443: fetches of URLs on other ports fail in-cluster but work locally.
- CI tool versions and checksums: `hack/tools.txt`; Docker build tools: `hack/docker-tools.txt`. Both are Renovate-managed; install with `hack/install-tools.sh`. In `tools.txt`, keep `repo`/`version`/`sha256` contiguous, because Renovate's regex depends on it.
- `ci-success` is the branch-protection check. `zizmor` audits workflows (third-party actions must be SHA-pinned).

## Git workflow

- Don't create branches, open PRs, push, or commit unless explicitly told to. Leave changes in the current checkout. Finishing verification is not a signal to ship.
- To watch PR checks, use `gh ... --wait`, not `sleep`.

## OpenCode tooling

`support/opencode/plugins/websearch.ts` makes this service's `/v1/search` the default provider for opencode's `websearch` (`NEURADEX_BASE_URL`, default `http://127.0.0.1:8080`). If websearch fails, the service probably isn't running. Setup (`npm install` in `support/opencode`, keep `@opencode/plugin` pinned to the opencode release): see `support/opencode/README.md`.
