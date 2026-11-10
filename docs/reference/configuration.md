# Configuration

The service reads its configuration from the process environment **once at startup** in
`crates/neuradex/src/config.rs`. There is no config file, no hot reload, and no per-request
override. Restart the process to apply a change.

The service can start without content storage (the default) and without an audit log file. It
cannot start without `KAGI_SESSION_TOKEN`.

## HTTP, Kagi, logs, and traces

| Variable | Required | Default | Valid values | Meaning |
| --- | --- | --- | --- | --- |
| `KAGI_SESSION_TOKEN` | yes to serve | — | non-empty string | Kagi session token; the service refuses to start without it. Not needed for `export-docs`. |
| `KAGI_BASE_URL` | no | `https://kagi.com` | HTTP(S) URL | Kagi base URL override, mainly for tests, mocks, or a proxy. Empty or whitespace uses the default; an unusable URL fails startup. |
| `LISTEN_ADDR` | no | `127.0.0.1:8080` | socket address | HTTP listen address |
| `USER_AGENT` | no | `kagi::USER_AGENT` | valid header value | `User-Agent` sent with fetches and searches |
| `KAGI_MAX_SESSIONS` | no | `2` | integer 1–64 | Maximum simultaneous searches; out-of-range is a startup error |
| `KAGI_PRIVACY_PASS` | no | `true` | boolean | `true` uses Privacy Pass; `false` uses the session cookie |
| `KAGI_PRIVACY_PASS_FALLBACK` | no | `false` | boolean | Retry a failed Privacy Pass search with the session cookie; only effective with `KAGI_PRIVACY_PASS=true` |
| `KAGI_PRIVACY_PASS_STATE` | no | XDG state path | file path, `off`, `none`, or `-` | File holding unspent Privacy Pass tokens across restarts. Empty falls back to the default; `off`/`none`/`-` keep tokens in memory. |
| `RUST_LOG` | no | `info` | `tracing` filter | Local log filter, e.g. `neuradex=debug` |
| `LOG_FORMAT` | no | `text` | `text`, `json` | Local log formatter; `json` is selected case-insensitively after trimming |
| `OTEL_SERVICE_NAME` | no | `neuradex` | string | OTel `service.name` |
| `OTEL_RESOURCE_ATTRIBUTES` | no | — | comma-separated `k=v` | Extra resource attributes |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | no | unset | URL | OTLP/HTTP endpoint; unset disables export |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | no | unset | URL | Overrides the trace endpoint |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | no | unset | URL | Overrides the log endpoint |
| `OTEL_TRACES_EXPORTER` | no | unset | `none`, `otlp` | Set `none` to disable trace export |
| `OTEL_LOGS_EXPORTER` | no | unset | `none`, `otlp` | Set `none` to disable log export |
| `OTEL_SDK_DISABLED` | no | unset | boolean | `true` disables the OTel SDK entirely |

Notes:

- `KAGI_MAX_SESSIONS` is not silently clamped: values outside 1–64 fail startup.
- `KAGI_BASE_URL` is trimmed; empty or whitespace selects the default. The URL must be usable
  HTTP(S); the Kagi client validates it during startup. The session token is sent to this host,
  so use HTTPS outside a local mock or trusted proxy.
- `KAGI_PRIVACY_PASS` accepts `1/true/yes/on` and `0/false/no/off`, case-insensitive, trimmed.
- `KAGI_PRIVACY_PASS_FALLBACK` accepts the same boolean spellings and has no effect when
  `KAGI_PRIVACY_PASS=false`.
- `KAGI_PRIVACY_PASS_STATE` is trimmed; an empty value is treated as unset and falls back to the
  default path. `off`, `none`, and `-` disable persistence. Relative `XDG_STATE_HOME` values are
  ignored, as the XDG base directory specification requires.
- The local formatter and the audit JSONL sink are independent. `RUST_LOG`/`LOG_FORMAT` control
  tracing output; audit events are always JSON Lines.
- The service always creates spans and log records locally; OTLP export is separate and off
  when no endpoint is configured.

## Privacy Pass token state

With Privacy Pass enabled, unspent tokens are persisted so that restarting the service does not
issue a fresh batch every time. The default path is:

```text
${XDG_STATE_HOME:-$HOME/.local/state}/neuradex/privacy-pass/tokens.json
```

If neither `XDG_STATE_HOME` nor `HOME` is available (both are unset, empty, or relative), the
store stays in memory and the service logs that at startup. An operator or test can override the
file with `KAGI_PRIVACY_PASS_STATE`; the parent directory is created as needed.

Behavior:

- The file is JSON with a version and the token queue in consumption order:
  `{"version":1,"tokens":["<base64url token>", "..."]}`. Token values are credentials and are
  never logged.
- On Unix the file is `0600` and any directory created for it is `0700`. An existing directory is
  used as-is. Writes go to a temporary file in the same directory, are flushed with `fsync`, and
  are renamed over the state file.
- Tokens are loaded before any issuance. A missing file starts empty without a warning; a
  corrupt, unreadable, or wrong-version file logs a warning without contents, starts empty, and
  is replaced on the next successful write.
- The file is rewritten after every successful issuance, token consumption, and clear, so a
  restart cannot resurrect a spent token.
- Reads and writes are synchronous and happen while the token store lock is held. The state file
  is small and local, so the brief block is acceptable; no lock is held across an await.
- `KAGI_PRIVACY_PASS_STATE` has no effect when `KAGI_PRIVACY_PASS=false`. The configured path (or
  `memory`) is logged once at startup.

## Privacy Pass fallback

With `KAGI_PRIVACY_PASS_FALLBACK=true`, a Privacy Pass search that Kagi refuses is retried
with session-cookie authentication instead of failing:

- A quota rejection (`429`, roughly ten issuance rounds per account per month) degrades Privacy
  Pass until the start of the next calendar month, when Kagi's generation epoch resets.
- A subscription rejection (`403`) degrades it for the rest of the process lifetime; restart the
  service after fixing the plan.
- A rejected fresh batch or a malformed issuer response falls back for that one search only.

Degraded searches skip token issuance entirely, and a degraded service reports
`session_fallback` in its `neuradex.search.request` audit event. The fallback trades away
Privacy Pass's unlinkability — those searches are linked to the account — so it is off by
default.

## Audit

| Variable | Required | Default | Meaning |
| --- | --- | --- | --- |
| `AUDIT_LOG_PATH` | no | unset → stdout | Path to the JSONL audit log. Unset, empty/whitespace, or `-` writes to stdout. A path is opened with create+append at startup; the parent directory must already exist. |
| `AUDIT_HMAC_KEY` | no | unset | Raw key bytes used for HMAC-SHA256 of identifier/IP headers. Empty means no key; those headers are redacted. |

Behavior:

- Exactly one `AuditEvent` is emitted per request except `/healthz`, which is skipped. The
  request middleware emits it for API calls, infrastructure endpoints, and `404`/`405`
  fallbacks, including failures.
- `JsonlSink` serializes one compact JSON object per line and calls `flush()` after **every**
  event. The file is not buffered across requests.
- With `AUDIT_LOG_PATH` set, the file is opened once at startup in append mode and kept open.
  Restarting the service appends to the same file; there is no rotation and no truncation.
- With no path (or `-`), events go to stdout. They are still JSONL and still independent of
  `RUST_LOG`/`LOG_FORMAT`.
- `AUDIT_HMAC_KEY` is used as raw bytes, not base64-decoded. It HMACs the identifier/IP headers
  listed in [RFD 0005](../rfd/0005-header-policy.md); without it, those headers are redacted.
- A sink write failure is logged (`error.type=audit_sink`) and does not change the HTTP response.
- The service does **not** write to Quickwit, ClickHouse, or an object-store event archive. The
  local compose stack provides those for a separate Vector/replay path; they are not configured
  by the service.

## Content

| Variable | Required | Default | Meaning |
| --- | --- | --- | --- |
| `CONTENT_STORE` | no | `off` | `off`, `local`, or `s3` (case-insensitive, trimmed). Unset/`off` disables content storage. |
| `CONTENT_LOCAL_PATH` | when `local` | — | Local store root. The directory is created at startup; relative paths are relative to the working directory. |
| `CONTENT_S3_ENDPOINT` | when `s3` | — | S3-compatible endpoint, e.g. `https://s3.us-west-004.backblazeb2.com` |
| `CONTENT_S3_REGION` | when `s3` | — | Signing region, e.g. `us-west-004` |
| `CONTENT_S3_BUCKET` | when `s3` | — | Bucket name |
| `CONTENT_S3_PREFIX` | no | unset | Optional key prefix; surrounding whitespace and slashes are stripped |
| `CONTENT_S3_ACCESS_KEY_ID` | when `s3` | — | Access key identifier |
| `CONTENT_S3_SECRET_ACCESS_KEY` | when `s3` | — | Secret access key |
| `CONTENT_S3_FORCE_PATH_STYLE` | no | `true` | Path-style addressing; B2 and MinIO need it |
| `CONTENT_PACK_TARGET_BYTES` | no | `134217728` (128 MiB) | Target **plaintext** bytes per pack; positive integer |
| `CONTENT_MAX_PACK_AGE_SECS` | no | `600` (10 min) | Maximum open-pack age policy; positive integer |
| `CONTENT_MAX_BODY_BYTES` | no | `26214400` (25 MiB) | Maximum captured body size; positive integer. Also the audit middleware request-body limit, capped at 1 MiB for JSON bodies. |
| `CONTENT_RETENTION_DAYS` | no | `365` | Retention policy value; positive integer |
| `CONTENT_COMPRESSION_LEVEL` | no | `3` | zstd level for packs; must be in the zstd range accepted by the writer |

Required combinations:

- `CONTENT_STORE=local` requires `CONTENT_LOCAL_PATH`; a missing or empty value fails startup
  with `CONTENT_STORE=local requires CONTENT_LOCAL_PATH`.
- `CONTENT_STORE=s3` requires `CONTENT_S3_ENDPOINT`, `CONTENT_S3_REGION`, `CONTENT_S3_BUCKET`,
  `CONTENT_S3_ACCESS_KEY_ID`, and `CONTENT_S3_SECRET_ACCESS_KEY`. `CONTENT_S3_PREFIX` and
  `CONTENT_S3_FORCE_PATH_STYLE` are optional.
- The numeric content settings are parsed and validated **even when `CONTENT_STORE=off`**; an
  invalid `CONTENT_PACK_TARGET_BYTES`/`CONTENT_MAX_PACK_AGE_SECS`/`CONTENT_MAX_BODY_BYTES`/
  `CONTENT_RETENTION_DAYS`/`CONTENT_COMPRESSION_LEVEL` fails startup regardless of the backend.
- `CONTENT_STORE` with any other value fails startup with
  `invalid CONTENT_STORE: expected off, local, or s3`.

Behavior:

- Content is **off by default**. No content store is built and no `content_references` are
  added to audit events. The rest of the service behaves normally.
- The service stores request/response JSON for search endpoints and response bodies for fetch:
  - `fetch`: the returned response body (`fetch-response`).
  - `peek`: the head bytes actually read (`peek-response`).
  - `search`: the request JSON (`search-request`) and response JSON (`search-response`).
  - `openwebui_search`: the request JSON (`openwebui-search-request`) and response JSON
    (`openwebui-search-response`).
- A body over `CONTENT_MAX_BODY_BYTES` is rejected by the content writer; the request still
  succeeds and the failure is logged. JSON request bodies are additionally capped at 1 MiB by
  the extractor.
- Packs are grouped by operation and stored as
  `packs/{operation}/{yyyy-mm-ddTHH}/{pid}-{sequence}.pack`; large bodies are stored as
  `objects/{blake3}`. The pack target is measured in appended plaintext bytes.
- The service flushes content every **5 seconds** while serving, and performs one final
  best-effort flush on shutdown. `ContentWriter::flush` seals every open pack, so packs normally
  close at the 5-second tick rather than at `CONTENT_MAX_PACK_AGE_SECS`; the 10-minute policy is
  a backstop checked during appends.
- `CONTENT_RETENTION_DAYS` is stored in the content policy and logged at startup, but the
  service does not delete objects or configure a bucket lifecycle. Retention enforcement is an
  external B2/S3 lifecycle concern; see
  [deployment.md](../operations/deployment.md#retention) and `neuradex-admin content-lifecycle`.
- S3 credentials are explicit settings; the service does not read `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, or `AWS_ENDPOINT_URL`.
- Changing any content setting requires a restart. The store/client is built once at startup;
  the local directory is created/opened at startup, and the S3 client is created once.

## S3 examples

Backblaze B2:

```sh
export CONTENT_STORE=s3
export CONTENT_S3_ENDPOINT=https://s3.us-west-004.backblazeb2.com
export CONTENT_S3_REGION=us-west-004
export CONTENT_S3_BUCKET=neuradex-content
export CONTENT_S3_ACCESS_KEY_ID=...
export CONTENT_S3_SECRET_ACCESS_KEY=...
export CONTENT_S3_PREFIX=neuradex
export CONTENT_S3_FORCE_PATH_STYLE=true
```

MinIO locally:

```sh
export CONTENT_STORE=s3
export CONTENT_S3_ENDPOINT=http://127.0.0.1:9000
export CONTENT_S3_REGION=us-east-1
export CONTENT_S3_BUCKET=neuradex-content
export CONTENT_S3_ACCESS_KEY_ID=neuradex
export CONTENT_S3_SECRET_ACCESS_KEY=neuradex-local-secret
export CONTENT_S3_FORCE_PATH_STYLE=true
```

Plain HTTP is acceptable only for the local MinIO endpoint. Use HTTPS for B2 and any other
remote endpoint. The S3 access key and secret are secrets; do not log or commit them.

## CLI

| Command | Purpose |
| --- | --- |
| `cargo run` | Serve; reads the full configuration and requires `KAGI_SESSION_TOKEN` |
| `cargo run -- export-docs <out.html> [--spec <out.json>]` | Render the docs page without `AppState`; requires the `docs` feature and no token |
| `cargo run -- --help` | clap help, including the environment summary |

## Precedence and validation

- Environment variables override built-in defaults; there is no config file.
- Invalid `LISTEN_ADDR`, `KAGI_BASE_URL`, `KAGI_MAX_SESSIONS`, `KAGI_PRIVACY_PASS`,
  `KAGI_PRIVACY_PASS_FALLBACK`, a missing/empty `KAGI_SESSION_TOKEN`, invalid content values, or
  an invalid selected content store is a startup error with a human-readable message.
- Booleans accept `1/true/yes/on` and `0/false/no/off`, case-insensitive, trimmed.
- `AUDIT_LOG_PATH` is trimmed; empty and `-` mean stdout.
- The service logs the listen address, content mode/policy, Kagi search mode, and Privacy Pass
  state path at startup. It never logs the Kagi token, audit HMAC key, S3 secret, or token value.

## Secrets

| Secret | Where it belongs | Never |
| --- | --- | --- |
| `KAGI_SESSION_TOKEN` | Secret manager / Kubernetes Secret | In `compose.yaml`, docs, logs |
| Privacy Pass token state file | Local XDG state directory with `0600` permissions | In the repo, logs, or a shared directory |
| `AUDIT_HMAC_KEY` | Secret manager | In the audit log, image, or repo |
| `CONTENT_S3_ACCESS_KEY_ID` / `CONTENT_S3_SECRET_ACCESS_KEY` | Secret manager | In the image, repo, or audit event |
| ClickHouse password | Secret manager | In dashboards or query text |

Local compose credentials are public by design. They are not a template for production.

## Example local `.env`

```sh
# .env (local only)
KAGI_SESSION_TOKEN=staging-token
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
LOG_FORMAT=json

# Optional: override where unspent Privacy Pass tokens are kept.
# Default: ${XDG_STATE_HOME:-$HOME/.local/state}/neuradex/privacy-pass/tokens.json
# KAGI_PRIVACY_PASS_STATE=.local/neuradex/privacy-pass/tokens.json

# Optional: retry failed Privacy Pass searches with the session cookie. The retried searches
# are linked to the account, so this is a deliberate availability/privacy trade-off.
# KAGI_PRIVACY_PASS_FALLBACK=true

# Audit JSONL for the optional Vector profile.
AUDIT_LOG_PATH=.local/neuradex/events.jsonl

# Content: local disk; off by default.
CONTENT_STORE=local
CONTENT_LOCAL_PATH=.local/neuradex/content
```

For a fast local content test, lower `CONTENT_PACK_TARGET_BYTES` (for example `1048576`) and
`CONTENT_MAX_PACK_AGE_SECS` (for example `10`); the 5-second service flush still applies.

## Validation

```sh
# Service config errors without starting the server.
KAGI_SESSION_TOKEN=test cargo run -- --help

# Compose file.
docker compose config --quiet

# Vector config (requires the image).
docker compose --profile ingest run --rm --no-deps vector \
  validate --no-environment /etc/vector/vector.toml
```
