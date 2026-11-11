# AGENTS.md

axum 0.8 HTTP service exposing LLM-agent tools as JSON: `GET /v1/{fetch,peek}`, `POST /v1/search` (`{"query","limit","timeout"}` in, `SearchResponse` out), `POST /v1/openwebui_search` (Open WebUI `external` search shape: `{"query","count"}` in, bare `[{link,title,snippet}]` out), `GET /healthz` (204). `/openapi.json` is always served; the `/docs` page is feature-gated (`docs`; legacy `/swagger-ui[/]` 302 there).

## Workspace

- Virtual workspace (edition 2024, resolver 3). `default-members = ["crates/neuradex"]`, so **root `cargo test`/`clippy` only cover the service** — pass `--workspace` for all members. Every direct child of `crates/` must be a package (glob `members`).
- `crates/neuradex` — the service (binary + lib). Entry: `src/main.rs` (clap) → `src/http.rs::serve`; runtime configuration is `src/config.rs`, shared state is `src/state.rs`, routes are in `src/api/v1.rs`.
- `crates/neuradex-docs` — renders `/docs` from the serialized OpenAPI JSON (`serde_json::Value`); `render()` is the only entry point the service uses. It has no `utoipa` dependency.
- `crates/neuradex-audit` — audit event schema, header policy, and sinks. No service, axum, or kagi dependency; the service maps its types into audit events.
- `crates/neuradex-packfile` — the pure packfile byte format (frames, footer, trailer, hashes). No async or object-storage dependency.
- `crates/neuradex-content` — content orchestration: pack writer, local/S3 stores, request index, range retrieval. The `local` feature is the default; `s3` adds `aws-sdk-s3`. The service owns the wiring, not this crate.
- `crates/neuradex-admin` — administrative binaries. `content-lifecycle` generates, checks, and applies the B2 lifecycle policy; keep operator-only AWS code here, not in the service.
- `crates/kagi` — vendored copy of `kagi` from `github.com/mkroman/zeta` (its `repository` field still points there). Never re-point the dep at the zeta git repo. It carries **local divergences** in `client.rs` (the session pool and the Privacy Pass session fallback) and `privacy_pass.rs` / `privacy_pass/` (Kagi Privacy Pass, its quota/subscription degradation, and durable token state), all pinned by its tests; a re-vendor must re-apply them. A re-vendor also overwrites its `Cargo.toml`, so redo the workspace wiring: shared deps as `workspace = true`, `version`/`edition`/`license` from `[workspace.package]`, `[lints] workspace = true`, `publish = false`. Dependency direction: neuradex → kagi / neuradex-docs, never the reverse; don't move helpers between this repo and zeta. Keep `client.rs` as the vendored file — do not split its session pool into new modules, or a re-vendor will lose the split.
- `crates/test-support` — shared HTTP test harnesses (`KagiHarness`, `PrivacyPassHarness`, `TestApp`) built on `httpmock`; production crates depend on it only under `[dev-dependencies]`. It is feature-gated (`client` for kagi tests, `app` for service tests) so client tests do not build the whole service, and it depends on the crates under test (the allowed dev-dependency cycle). Keep harness code here rather than in `tests/common` modules. Use it only from `tests/`, never from `src/#[cfg(test)]` modules of a crate it depends on: the dev-dependency cycle can otherwise link two copies of that crate.
- Lints live only in root `[workspace.lints]` (clippy `all`/`pedantic`/`nursery` as warn, `unsafe_code` forbidden, `missing_docs` incl. `# Errors` sections). Members use `[lints] workspace = true`; Cargo rejects per-member overrides.
- Shared deps (and the tokio feature set) are pinned in `[workspace.dependencies]`; members add features on top. New dependencies must be justified.
- `neuradex` and `test-support` enable `kagi`'s four compression features on purpose: reqwest only advertises `Accept-Encoding` — and decodes compressed responses — when they are on. Do not trim them when changing TLS features; the Kagi harness pins the header.
- utoipa's `macros` feature can't be dropped (utoipa 6.0.0 doesn't compile without it); only the service uses the derives.

## Documentation and RFDs

`docs/` is part of the deliverable: a change that alters a documented contract updates the matching page in the same PR. Start at `docs/README.md` — it is the index, status table, and "if you need to… read" map; follow its links instead of guessing paths.

- `docs/architecture/` — current behavior: request lifecycle, event model, content plane, storage topology, failure model. Read the relevant page before changing a subsystem.
- `docs/reference/` — contracts that must match the code exactly: configuration, event fields, packfile bytes, ClickHouse schema, Quickwit mapping, compose services.
- `docs/operations/` — runbook, observability, capacity and cost. `docs/development/` — local stack, testing, CI, and style.
- `docs/rfd/` — decisions and their history, not current state. Read `docs/rfd/README.md` for the process, the six Oxide states, and the index. New RFDs start from `docs/rfd/TEMPLATE.md` as `docs/rfd/NNNN-slug.md` with the next free number, and use `authors: [neuradex maintainers]`, never personal names.
- RFD states: `discussion` is not a commitment, `published` means the direction is agreed, `committed` means it is implemented. Do not rewrite a merged RFD to reverse a decision — supersede it with a new RFD and add `supersedes`/`superseded-by` links to both.
- The `/docs` HTTP page is generated from the OpenAPI document; edit the handler docs and `openapi.rs`, not HTML.
- `docs/README.md` → "Keeping docs accurate" maps each kind of change to the page that must be updated.

## Verify

Build needs `cmake`, `clang` and `libclang-dev` (wreq compiles BoringSSL). Full builds are slow, so iterate with targeted tests (`cargo test -p neuradex <name>`, `-p kagi`, `-p neuradex-docs`). Install the pinned tools with `mise install --locked`; run the gates once at the end with `mise run check` (fmt, clippy, workspace tests, lean tests, rustdoc, cargo deny advisories, shellcheck, in that order). The lean leg keeps `-p neuradex`: combining `--no-default-features` with `--workspace` strips the other members' defaults. Rustdoc uses `--all-features`, so it pulls in kagi's native-tls/OpenSSL; cargo deny only gates advisories.

Tests are offline: cheap unit tests are inline `#[cfg(test)]` modules; tests that need `test-support` live in `tests/` and drive local `httpmock` servers. Kagi fixtures live in `crates/test-support/fixtures/kagi`; integration tests read them through `test_support::kagi::fixture`/`read_fixture`, and parser unit tests read the same files directly to avoid the dev-dependency cycle.

## Running locally

- `cargo run` serves. It needs a non-empty `KAGI_SESSION_TOKEN`. Optional: `LISTEN_ADDR` (default `127.0.0.1:8080`), `USER_AGENT`, `KAGI_MAX_SESSIONS` (default 2; bounds simultaneous searches in both authentication modes, maximum 64), `KAGI_PRIVACY_PASS` (default `true`; set `false` for session-cookie search authentication), `KAGI_PRIVACY_PASS_FALLBACK` (default `false`; retries a search with session-cookie authentication when Privacy Pass fails — a 429 quota rejection degrades until the next month, a 403 subscription rejection until restart; the retried searches are linked to the account), `KAGI_PRIVACY_PASS_STATE` (file for unspent Privacy Pass tokens; default `${XDG_STATE_HOME:-$HOME/.local/state}/neuradex/privacy-pass/tokens.json`, `off` keeps them in memory), `RUST_LOG` (default `info`), and `LOG_FORMAT` (`text`, the local default, or `json`; the Helm chart sets `json`). OpenTelemetry follows the standard `OTEL_*` variables: `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_EXPORTER_OTLP_ENDPOINT` (OTLP/HTTP; unset disables export), `OTEL_TRACES_EXPORTER`/`OTEL_LOGS_EXPORTER` (`none` disables a signal), and `OTEL_SDK_DISABLED`.
- Audit and content: `AUDIT_LOG_PATH` (JSONL; unset or `-` is stdout), `AUDIT_HMAC_KEY` (raw key for hashed identifier/IP headers; without it they are redacted), `CONTENT_STORE` (`off` default, `local`, or `s3`), `CONTENT_LOCAL_PATH`, the `CONTENT_S3_*` settings, and `CONTENT_PACK_TARGET_BYTES`/`CONTENT_MAX_PACK_AGE_SECS`/`CONTENT_MAX_BODY_BYTES`/`CONTENT_RETENTION_DAYS`/`CONTENT_COMPRESSION_LEVEL`.
- The local stack is `compose.yaml` (Quickwit, MinIO, ClickHouse, Jaeger; optional Vector with `docker compose --profile ingest up -d vector`). `docs/development/local-development.md` is the full sequence; the service is not a compose service. MinIO host ports conflict with another local MinIO unless overridden; `docs/reference/compose-services.md` has the `!override` example.
- `cargo run -- export-docs <out.html> [--spec <out.json>]` writes the docs page without building `AppState` (no token needed). It only works when built with `docs`.
- In debug builds, `/docs` re-reads `crates/neuradex-docs/theme/*` from disk on every request. Release builds embed and cache it. New theme files must be added to the ordered lists in `crates/neuradex-docs/src/assets.rs`.
- Unexpected 400s: `RUST_LOG=axum::rejection=trace`.

## Non-obvious rules

- **Routes and OpenAPI come from one place**: register `#[utoipa::path(...)]` handlers via `routes!` in `api()` (`src/api/v1.rs`). Never use `.route(...)` for them or hand-list paths. Exceptions:
  - `/docs`, `/openapi.json`, `/swagger-ui` and the 404/405 fallbacks are hand-registered in `router()`.
- **Every error is `{"error": {type, message}}`**, including the router 404/405 fallbacks. 5xx bodies are generic; details go to tracing only. `ApiError::Upstream`/`Internal` carry boxed sources: keep the source, log it (never render it in the body), and keep the client message generic. A new error path needs two tests: a router oneshot test in `src/api/v1.rs` and a variant mapping test in `src/api/v1/error.rs`. `ErrorBody` must stay in `components(schemas(...))` in `openapi.rs`, because the manual `IntoResponses` impl is invisible to utoipa. That impl (`error.rs`) hand-lists the documented statuses (400/415/500/502): a new status must be added there, and new handlers list `ApiError` in their `utoipa::path` responses, or the error is missing from `/openapi.json`.
- **Never extract `Query<T>`/`Json<T>` directly.** Use `ApiQuery<T>`/`ApiJson<T>` (`extract.rs`), which turn rejections into the JSON envelope. `ApiQuery` wraps `axum_extra::extract::Query` (serde_html_form) so repeated keys work. Optional lists are `Vec<T>` + `#[serde(default)]`, never `Option<Vec<T>>`.
- **The wire types are the single source of truth**: `PageParams` (shared by fetch and peek)/`SearchRequest`/`OpenWebUiSearchRequest` handle deserialization, validation methods, and OpenAPI docs (`IntoParams` for query types, `ToSchema` + `request_body` for JSON bodies). A new field needs all three. Each endpoint owns its `include` allow-list (`FETCH_INCLUDES` vs `PEEK_INCLUDES`).
- Tests must go through the real extraction path (`Query::try_from_uri` on the real types), not a hand-rolled deserializer. That mismatch once shipped a production 400.
- `html5ever` is `!Send`: `HeadParser` runs on `spawn_blocking`, fed by a bounded `tokio::sync::mpsc` (async `send` / `blocking_recv`) in `stream.rs`. Never use `std::sync::mpsc` or blocking sends in tokio tasks. Abort the reader when the parser finishes, even on success: dropping the receiver does not interrupt a reader parked in `bytes_stream().next()`, so peek would otherwise wait for a stalled body.
- `wreq::Error` renderings include the request URI. Call `without_uri()` before boxing one in `ApiError`; 5xx logs render the source chain, while the response body stays generic.
- Peek stops at `</head>`/`<body>` (cap `HEAD_MAX_BYTES` 2 MiB); the parser also stops early, but fetch keeps collecting the body up to `FETCH_MAX_BYTES` (25 MiB). The returned body is capped at `FETCH_MAX_BYTES`, while `bytes_read` counts the decompressed bytes pulled from upstream and may exceed the cap by the final chunk. `truncated: true` only when bytes were actually cut.
- The wreq client uses `Policy::none()` because redirects are followed manually in `redirect.rs`, so each hop is counted in metrics.
- Reserve the `Validated*` prefix for extractors that validate. Scope middleware per route, not as a global layer with path predicates.
- `support/design/api-docs-design.md`: the single-file/no-runtime-fetch constraints are superseded (the page is rendered at runtime); its tokens and layout still apply.

## Deployment / CI

- Helm chart `chart/neuradex`: `helm lint`/`template` fail unless `--set kagiSessionToken=ci-dummy` or `--set existingSecret=<name>` is passed. Audit/content are configured through `audit.*`/`content.*`; see `docs/operations/deployment.md`. Retention is external: `cargo run -p neuradex-admin -- content-lifecycle` generates/checks/applies the B2 lifecycle policy.
- `deployment-test.yaml` builds the image, installs the chart into kind, and pipes `hack/smoke-test.sh` into a curl pod. Update the script when you change `/healthz`, `/v1/fetch`, `/openapi.json` or `/docs`.
- The image binds `LISTEN_ADDR=0.0.0.0:8080`. Runtime is distroless (CA bundle required by kagi's rustls-platform-verifier). The Docker build copies only `Cargo.toml`, `Cargo.lock` and `crates/` (and `.dockerignore` excludes `support/`, `chart/` etc.), so anything else the build needs must be added to the `Dockerfile`.
- The chart's NetworkPolicy is on by default and only allows egress to DNS (53) and ports 80/443: fetches of URLs on other ports fail in-cluster but work locally. Other egress (e.g. an OTLP collector on 4318) needs an `extraNetworkPolicyEgress` rule.
- CI and operator tools: versions and mise backends in `mise.toml`, checksums in `mise.lock`; install with `mise install --locked`. The mise tasks (`fmt`, `clippy`, `test`, `test-lean`, `doc`, `deny`, `shellcheck`; `check` runs all seven) are the local command surface (`mise run <task>`), and CI puts the pinned tools on `PATH` with `jdx/mise-action`. Short names in `[tools]` map to GitHub backends via `[tool_alias]`, so workflow `install_args` stay short; Renovate updates the short-name versions, and `mise lock` applies the alias when it refreshes the lockfile. `use_versions_host = false` keeps `mise lock` checksums sourced from GitHub release metadata/checksum assets; Helm's archive comes from `get.helm.sh` and its published `.sha256sum`. Docker build tools remain in `hack/docker-tools.txt` and are installed by `hack/install-tools.sh` during the image build. Renovate manages `mise.toml`/`mise.lock` and `hack/docker-tools.txt`, with a 7-day minimum release age.
- `ci-success` is the branch-protection check. `zizmor` audits workflows (third-party actions must be SHA-pinned).

## Git workflow

- Don't create branches, open PRs, push, or commit unless explicitly told to. Leave changes in the current checkout. Finishing verification is not a signal to ship.
- Use the GitHub MCP tools for pull-request operations: `github.create_pull_request` to open a PR, `github.pull_request_read` (`method: "get"`, `"get_status"`, `"get_check_runs"`) to inspect it and its checks, `github.update_pull_request` to change it, and `github.list_pull_requests`/`github.search_pull_requests` to find PRs.
- To wait for PR checks, use `gh pr checks <number> --watch` (or `gh pr checks <number> --watch --fail-fast`). The GitHub MCP has no blocking wait primitive, and agents should not hand-roll polling loops with `sleep`.

## OpenCode tooling

`support/opencode/plugins/websearch.ts` makes this service's `/v1/search` the default provider for opencode's `websearch` (`NEURADEX_BASE_URL`, default `http://127.0.0.1:8080`). If websearch fails, the service probably isn't running. Setup (`npm install` in `support/opencode`, keep `@opencode/plugin` pinned to the opencode release): see `support/opencode/README.md`.
