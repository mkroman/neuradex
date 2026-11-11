# Local development

This page is the working loop for the audit/analytics/content stack. The service runs on the
host; the stores run in Docker Compose. The stack is local-only and unauthenticated — use
synthetic pages and events, never real user data.

## Prerequisites

| Tool | Why | Check |
| --- | --- | --- |
| [mise](https://mise.jdx.dev/) | Pinned CI/operator tools and `mise run check` | `mise --version` |
| Rust (stable) | Build/run the service | `cargo --version` |
| Docker + Compose v2 | Run Quickwit, MinIO, ClickHouse, Jaeger | `docker compose version` |
| `cmake`, `clang`, `libclang-dev` | `wreq` compiles BoringSSL | see [AGENTS.md](../../AGENTS.md) |
| `KAGI_SESSION_TOKEN` | The service refuses to start without it | `test -n "$KAGI_SESSION_TOKEN"` |

Ports used on the host:

| Port | Service |
| --- | --- |
| 7280 | Quickwit REST/UI |
| 7281 | Quickwit gRPC |
| 8123 | ClickHouse HTTP |
| 9000 | MinIO S3 API |
| 9001 | MinIO console |
| 16686 | Jaeger UI |
| 19000 | ClickHouse native |
| 4317 | OTLP/gRPC |
| 4318 | OTLP/HTTP |

Every binding is `127.0.0.1`. If 9000/9001 are already taken (for example by another local
MinIO), use a temporary override:

```yaml
# /tmp/neuradex-ports.yaml
services:
  minio:
    ports: !override
      - "127.0.0.1:19001:9000"
      - "127.0.0.1:19002:9001"
```

```sh
docker compose -f compose.yaml -f /tmp/neuradex-ports.yaml up -d
```

The rest of this page assumes the default ports.

## Start the stack

```sh
docker compose up -d
docker compose ps
```

Expected: `quickwit`, `minio`, `clickhouse`, `jaeger` healthy; `quickwit-init` and
`minio-init` exited 0.

If something is not healthy:

```sh
docker compose logs --no-color quickwit quickwit-init
docker compose logs --no-color minio minio-init
docker compose logs --no-color clickhouse
```

Quickwit's UI is at <http://127.0.0.1:7280>; the index `neuradex-events` should exist. MinIO's
console is at <http://127.0.0.1:9001> (`neuradex` / `neuradex-local-secret`) and should show
`neuradex-content` and `neuradex-quickwit`.

## Run the service

```sh
mkdir -p .local/neuradex
export KAGI_SESSION_TOKEN=staging-token
export AUDIT_LOG_PATH=.local/neuradex/events.jsonl
export CONTENT_STORE=local
export CONTENT_LOCAL_PATH=.local/neuradex/content
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export LOG_FORMAT=json
# Optional: keep unspent Privacy Pass tokens under the checkout instead of the XDG state dir.
# The default is ${XDG_STATE_HOME:-$HOME/.local/state}/neuradex/privacy-pass/tokens.json.
# export KAGI_PRIVACY_PASS_STATE=.local/neuradex/privacy-pass/tokens.json
cargo run
```

Content is **off by default**; the two `CONTENT_*` variables above enable the local store.
Without `AUDIT_LOG_PATH`, audit events go to stdout. The audit file is opened in append mode at
startup, and content is flushed every 5 seconds; restart the service to change configuration.

Privacy Pass tokens are persisted by default, so frequent local restarts reuse the unspent
tokens instead of requesting a fresh batch from `/pp/gettokens` each time. Set
`KAGI_PRIVACY_PASS_STATE` to override the path, or to `off` to keep tokens in memory only. The
file contains bearer credentials; it is created with `0600` permissions in a `0700` directory on
Unix and must not be committed. `.local/` is already ignored by git.

Then exercise the API:

```sh
curl -sS 'http://127.0.0.1:8080/healthz' -o /dev/null -w '%{http_code}\n'   # 204
curl -sS 'http://127.0.0.1:8080/v1/fetch?url=https://example.com&include=headers' | jq '{url, metrics, title: .metadata.title}'
curl -sS -XPOST 'http://127.0.0.1:8080/v1/search' \
  -H 'content-type: application/json' \
  -d '{"query":"rust programming","limit":3,"timeout":10}' | jq '{query, metrics, results: [.results[].title]}'
```

The service exports spans/logs when `OTEL_EXPORTER_OTLP_ENDPOINT` is set; open Jaeger at
<http://127.0.0.1:16686> and look for service `neuradex`.

Audit events are written to `.local/neuradex/events.jsonl`; content is written under
`.local/neuradex/content/` when the fetch returns a body. Inspect the JSONL directly:

```sh
tail -n 1 .local/neuradex/events.jsonl | jq '{event, request_id, http, operation, content_references}'
find .local/neuradex/content -type f | sort
```

## Send events to the optional stack

The optional Vector profile tails `.local/neuradex/events.jsonl` and writes to Quickwit and
ClickHouse. Start it after the service has produced at least one event:

```sh
docker compose --profile ingest up -d vector
docker compose logs -f vector
```

Generate a fetch and a search so both event shapes appear:

```sh
curl -sS 'http://127.0.0.1:8080/v1/fetch?url=https://example.com' >/dev/null
curl -sS -XPOST 'http://127.0.0.1:8080/v1/search' \
  -H 'content-type: application/json' \
  -d '{"query":"rust programming","limit":3,"timeout":10}' >/dev/null
sleep 12
```

Verify both sinks:

```sh
# Quickwit: the index commits every 10 s.
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/search' \
  -H 'content-type: application/json' \
  -d '{"query":"*","max_hits":5}' | jq '{num_hits, ids: [.hits[]._source.request_id]}'

# ClickHouse: the row and the daily materialized view.
curl -sS 'http://127.0.0.1:8123/?user=neuradex&password=neuradex-local-secret' \
  --data-binary "SELECT event, request_id, operation_type, http_route, content_reference_count, content_bytes FROM neuradex.events ORDER BY timestamp DESC LIMIT 5 FORMAT PrettyCompact"

curl -sS 'http://127.0.0.1:8123/?user=neuradex&password=neuradex-local-secret' \
  --data-binary "SELECT * FROM neuradex.events_daily ORDER BY day DESC LIMIT 5 FORMAT PrettyCompact"
```

Quickwit indexes the nested `service`/`http`/`operation` objects and stores arrays as
`{"items":[…]}` because its `json` field is object-only. ClickHouse gets a flat projection with
JSON strings for `content_references` and the header captures. Both come from the same event.

The service itself does not write to Quickwit, ClickHouse, or an event archive; content goes to the configured local/S3 store (MinIO locally, B2 when configured). Vector is the local bridge.
The event archive and request index described in the RFDs are not wired into the service yet.

## Inspect the stores

### Quickwit

```sh
# List indexes.
curl -sS 'http://127.0.0.1:7280/api/v1/indexes' | jq '.[] | {index_id: .index_config.index_id, index_uid}'

# Search by a raw field (quote values with punctuation).
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/search' \
  -H 'content-type: application/json' \
  -d '{"query":"operation.type:fetch AND http.status:200","max_hits":10}' | jq '{num_hits, hits: [.hits[]._source | {request_id, operation: .operation.type, route: .http.route}]}'

# Count errors.
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/search' \
  -H 'content-type: application/json' \
  -d '{"query":"http.status:>=500","max_hits":0}' | jq '.num_hits'
```

### ClickHouse

```sh
docker compose exec -T clickhouse clickhouse-client \
  --user neuradex --password neuradex-local-secret \
  --query "SELECT event, count() FROM neuradex.events GROUP BY event"
```

Or over HTTP:

```sh
curl -sS 'http://127.0.0.1:8123/?user=neuradex&password=neuradex-local-secret' \
  --data-binary "SELECT toDate(timestamp) AS day, count() FROM neuradex.events GROUP BY day FORMAT PrettyCompact"
```

### MinIO

```sh
docker compose exec -T minio mc alias set local http://127.0.0.1:9000 neuradex neuradex-local-secret
docker compose exec -T minio mc ls local/
docker compose exec -T minio mc ls --recursive local/neuradex-content/
```

### Jaeger

Generate a span with a fetch request, then open <http://127.0.0.1:16686> and select service
`neuradex`. Jaeger is in-memory; a restart drops traces.

## Configuration

The compose stack's credentials are intentionally local:

| Variable | Value |
| --- | --- |
| `MINIO_ROOT_USER` | `neuradex` |
| `MINIO_ROOT_PASSWORD` | `neuradex-local-secret` |
| `CLICKHOUSE_DB` | `neuradex` |
| `CLICKHOUSE_USER` | `neuradex` |
| `CLICKHOUSE_PASSWORD` | `neuradex-local-secret` |

To exercise the S3 content backend against MinIO instead of the local store, set these and
restart `cargo run`:

```sh
export CONTENT_STORE=s3
export CONTENT_S3_ENDPOINT=http://127.0.0.1:9000
export CONTENT_S3_REGION=us-east-1
export CONTENT_S3_BUCKET=neuradex-content
export CONTENT_S3_ACCESS_KEY_ID=neuradex
export CONTENT_S3_SECRET_ACCESS_KEY=neuradex-local-secret
export CONTENT_S3_FORCE_PATH_STYLE=true
```

Do not reuse these values outside the local stack. See
[reference/configuration.md](../reference/configuration.md).

Useful local overrides for a quick content test:

```sh
export CONTENT_PACK_TARGET_BYTES=1048576
export CONTENT_MAX_PACK_AGE_SECS=10
```

The service still flushes every 5 seconds, so a small pack closes at the next tick.

## Reset

```sh
# Stop containers, keep volumes.
docker compose down

# Stop and delete all local Quickwit/MinIO/ClickHouse data.
docker compose down -v

# Delete the local audit log and content store (contains metadata/real bytes; never commit it).
rm -rf .local/neuradex
```

ClickHouse init SQL runs only on an empty data directory. After changing
`hack/clickhouse/init.sql`, reset the volume or apply the `ALTER` manually. Quickwit mappings
are not mutable in place; delete the index and re-run `quickwit-init`.

## Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| `quickwit-init` exits 1 with `unknown variant millis` | Old index config used `millis` | Use `milliseconds` (current file) |
| `quickwit-init` exits 1 with `index already exists` | Index exists but check failed | Inspect `quickwit index list`; delete/recreate if the mapping changed |
| Quickwit search returns 0 after ingest | Commit is asynchronous | Wait ≥ 10 s; check `quickwit` logs for parse errors |
| Quickwit rejects `request_headers` | Headers were a JSON string, not an object | Send objects in the event; Quickwit `json` fields require objects |
| ClickHouse `request_headers` is empty | Vector flatten transform not applied | Ensure the `flatten` transform is in the chain |
| MinIO port bind fails | Another MinIO owns 9000/9001 | Use the override above |
| ClickHouse init SQL did not run | Data directory was not empty | `docker compose down -v` or apply DDL manually |
| Vector not shipping | Profile not enabled, or audit log file missing | `docker compose --profile ingest up -d vector`; check the `AUDIT_LOG_PATH` file and logs |
| Jaeger has no traces | OTLP endpoint not set, or service not restarted | Export `OTEL_EXPORTER_OTLP_ENDPOINT` and restart `cargo run` |
| No audit events in the JSONL | `AUDIT_LOG_PATH` unset (stdout) or path parent missing | Set `AUDIT_LOG_PATH` and ensure its parent exists; restart the service |
| No content references | `CONTENT_STORE` is still `off` | Set `CONTENT_STORE=local` and `CONTENT_LOCAL_PATH`; restart |

## Next steps

- [Testing](testing.md) — what to run before opening a PR.
- [Compose services reference](../reference/compose-services.md) — images, ports, volumes.
- [Runbook](../operations/runbook.md) — what to do when the stack misbehaves.
