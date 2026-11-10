# Configuration

Configuration is environment-first. Secrets are never committed. This page separates what the
service reads **today** from the audit/content settings that are **planned** for the pipeline.

## Service environment (implemented)

Read in `crates/neuradex/src/config.rs` at startup.

| Variable | Required | Default | Valid values | Meaning |
| --- | --- | --- | --- | --- |
| `KAGI_SESSION_TOKEN` | yes | — | non-empty string | Kagi session token; the service refuses to start without it |
| `LISTEN_ADDR` | no | `127.0.0.1:8080` | socket address | HTTP listen address |
| `USER_AGENT` | no | `kagi::USER_AGENT` | valid header value | `User-Agent` sent with fetches and searches |
| `KAGI_MAX_SESSIONS` | no | `2` | integer 1–64 | Maximum simultaneous searches; also bounds the service search gate |
| `KAGI_PRIVACY_PASS` | no | `true` | boolean (`1/true/yes/on`, `0/false/no/off`) | `true` uses Privacy Pass; `false` uses the session cookie |
| `RUST_LOG` | no | `info` | `tracing` filter | Log filter, e.g. `neuradex=debug` |
| `LOG_FORMAT` | no | `text` | `text`, `json` | Log formatter; Helm sets `json` |
| `OTEL_SERVICE_NAME` | no | `neuradex` | string | OTel `service.name` |
| `OTEL_RESOURCE_ATTRIBUTES` | no | — | comma-separated `k=v` | Extra resource attributes |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | no | unset | URL | OTLP/HTTP endpoint; unset disables export |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | no | unset | URL | Overrides the trace endpoint |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | no | unset | URL | Overrides the log endpoint |
| `OTEL_TRACES_EXPORTER` | no | unset | `none`, `otlp` | Set `none` to disable trace export |
| `OTEL_LOGS_EXPORTER` | no | unset | `none`, `otlp` | Set `none` to disable log export |
| `OTEL_SDK_DISABLED` | no | unset | boolean | `true` disables the OTel SDK entirely |

Notes:

- `KAGI_MAX_SESSIONS` is clamped to 1–64; out-of-range values are a startup error, not a clamp.
- The service always creates spans and log records locally; export is independent. A missing
  endpoint keeps local runs quiet.
- `LISTEN_ADDR=0.0.0.0:8080` is the container/Helm default; locally the default is loopback.

### CLI

| Command | Purpose |
| --- | --- |
| `cargo run` | Serve |
| `cargo run -- export-docs <out.html> [--spec <out.json>]` | Render the docs page without `AppState`; requires the `docs` feature |
| `cargo run -- --help` | clap help |

## Planned audit and content settings

These names are the design's proposed configuration. **They are not implemented in this
branch.** Do not set them expecting an effect until the wiring lands; the names may be adjusted
in the implementation PR.

| Variable | Proposed default | Meaning |
| --- | --- | --- |
| `NEURADEX_AUDIT_ENABLED` | `true` | Emit audit events |
| `NEURADEX_AUDIT_EVENT_LOG` | unset | Local NDJSON spool path (e.g. `.local/neuradex/events.jsonl`) |
| `NEURADEX_AUDIT_SPOOL_DIR` | `.local/neuradex/audit-spool` | Bounded event spool |
| `NEURADEX_AUDIT_STORE_QUERY_TEXT` | `false` | Capture `query_text`; restricted to audit readers |
| `NEURADEX_AUDIT_HMAC_KEY` | — | Base64 32+ byte key for sensitive header HMACs |
| `NEURADEX_AUDIT_QUICKWIT_URL` | unset | Quickwit ingest endpoint |
| `NEURADEX_AUDIT_CLICKHOUSE_URL` | unset | ClickHouse HTTP endpoint |
| `NEURADEX_AUDIT_CLICKHOUSE_DATABASE` | `neuradex` | ClickHouse database |
| `NEURADEX_AUDIT_CLICKHOUSE_TABLE` | `events` | ClickHouse table |
| `NEURADEX_CONTENT_ENABLED` | `true` | Capture content |
| `NEURADEX_CONTENT_BUCKET` | `neuradex-content` | B2 bucket |
| `NEURADEX_CONTENT_PACK_TARGET_BYTES` | `134217728` | 128 MiB encoded pack target |
| `NEURADEX_CONTENT_PACK_MAX_AGE_SECS` | `600` | 10 minutes |
| `NEURADEX_CONTENT_SPOOL_DIR` | `.local/neuradex/spool` | Local pack/content spool |
| `NEURADEX_CONTENT_MAX_BODY_BYTES` | `26214400` | 25 MiB capture cap |
| `NEURADEX_CONTENT_LARGE_BODY_BYTES` | `31457280` | 30 MiB standalone CAS threshold (effectively disabled) |
| `NEURADEX_CONTENT_READ_MAX_BYTES` | `26214400` | Read API response cap |

The defaults reflect the decisions in [RFD 0002](../rfd/0002-content-addressed-storage.md) and
[RFD 0003](../rfd/0003-hashing.md). Changing `NEURADEX_CONTENT_MAX_BODY_BYTES` above 30 MiB
activates the standalone CAS path; test it before doing so.

## Object storage (B2 / S3)

The content crate uses the official `aws-sdk-s3` via the `s3` feature. It reads the standard
AWS environment variables:

| Variable | Example | Meaning |
| --- | --- | --- |
| `AWS_ACCESS_KEY_ID` | `...` | B2 application key ID |
| `AWS_SECRET_ACCESS_KEY` | `...` | B2 application key |
| `AWS_REGION` | `us-west-004` | B2 region |
| `AWS_ENDPOINT_URL` | `https://s3.us-west-004.backblazeb2.com` | B2 S3 endpoint |
| `AWS_ALLOW_HTTP` | `false` | neuradex-specific escape hatch for local plain HTTP |

For MinIO locally:

```sh
export AWS_ACCESS_KEY_ID=neuradex
export AWS_SECRET_ACCESS_KEY=neuradex-local-secret
export AWS_REGION=us-east-1
export AWS_ENDPOINT_URL=http://127.0.0.1:9000
export AWS_ALLOW_HTTP=true
```

Use path-style addressing for MinIO; the content store constructor sets it when the endpoint is
not an AWS/B2 hostname (planned). Never set `AWS_ALLOW_HTTP=true` in production.

## Compose environment

| Service | Variable | Local value |
| --- | --- | --- |
| `minio` | `MINIO_ROOT_USER` | `neuradex` |
| `minio` | `MINIO_ROOT_PASSWORD` | `neuradex-local-secret` |
| `minio-init` | `MINIO_ROOT_USER` / `MINIO_ROOT_PASSWORD` | same |
| `clickhouse` | `CLICKHOUSE_DB` | `neuradex` |
| `clickhouse` | `CLICKHOUSE_USER` | `neuradex` |
| `clickhouse` | `CLICKHOUSE_PASSWORD` | `neuradex-local-secret` |
| `clickhouse` | `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT` | `1` |
| `quickwit` | `QW_DISABLE_TELEMETRY` | `true` |
| `quickwit-init` | `QW_CLUSTER_ENDPOINT` | `http://quickwit:7280` |
| `vector` | — | reads `hack/vector/neuradex-events.toml` |

See [compose-services.md](compose-services.md) for ports and volumes.

## Config files

| File | Format | Used by | Purpose |
| --- | --- | --- | --- |
| `compose.yaml` | YAML | Docker Compose | Local stack |
| `hack/quickwit/quickwit.yaml` | YAML, version 0.7 | Quickwit node | Node ID, ports, data dir, file metastore |
| `hack/quickwit/neuradex-events.yaml` | YAML, version 0.8 | `quickwit-init` | Index mapping, commit timeout, 30-day retention |
| `hack/clickhouse/init.sql` | SQL | ClickHouse init | Database, `events`, daily views |
| `hack/vector/neuradex-events.toml` | TOML | Vector | File source, parse, Quickwit/ClickHouse sinks |

Quickwit mappings are not mutable in place. A field change requires a new index (or delete +
recreate) and a replay; see [quickwit-index.md](quickwit-index.md).

## Precedence and validation

- Environment variables override built-in defaults; there is no config file for the service.
- Invalid `LISTEN_ADDR`, `KAGI_MAX_SESSIONS`, `KAGI_PRIVACY_PASS`, or a missing/empty
  `KAGI_SESSION_TOKEN` is a startup error with a human-readable message.
- Booleans accept `1/true/yes/on` and `0/false/no/off`, case-insensitive, trimmed.
- The service logs the listen address and port at startup but never logs the token.

Validate a local configuration without starting the full stack:

```sh
# Service config errors (no network needed).
KAGI_SESSION_TOKEN=test cargo run -- --help

# Compose file.
docker compose config --quiet

# Vector config (requires the image).
docker compose --profile ingest run --rm --no-deps vector \
  validate --no-environment /etc/vector/vector.toml
```

## Secrets

| Secret | Where it belongs | Never |
| --- | --- | --- |
| `KAGI_SESSION_TOKEN` | Secret manager / Kubernetes Secret | In `compose.yaml`, docs, logs |
| B2 application key | Secret manager | In the image, repo, or event |
| Header HMAC key | Secret manager | In B2 credentials or event payloads |
| ClickHouse password | Secret manager | In dashboards or query text |
| Ingress signing key | Ingress secret manager | In the service image |

Local compose credentials are public by design. They are not a template for production.

## Example local `.env`

Docker Compose reads `.env` automatically. Do not commit it (`.local/` and typical `.env`
patterns are ignored; check `.gitignore` before adding one).

```sh
# .env (local only)
KAGI_SESSION_TOKEN=staging-token
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
LOG_FORMAT=json
RUST_LOG=info
```

The compose services use their own environment in `compose.yaml`; this file is for `cargo run`
and ad-hoc commands.
