# Local development

This page is the working loop for the audit/analytics/content stack. The service runs on the
host; the stores run in Docker Compose. The stack is local-only and unauthenticated — use
synthetic pages and events, never real user data.

## Prerequisites

| Tool | Why | Check |
| --- | --- | --- |
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
export KAGI_SESSION_TOKEN=staging-token
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export LOG_FORMAT=json
cargo run
```

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

> The audit/content pipeline is not wired into the service in this branch. The next section
> feeds the same event shape directly into the stack so the mappings can be developed and
> tested before the code lands.

## Send a synthetic event

The optional Vector profile tails `.local/neuradex/events.jsonl`. Create the directory and one
event:

```sh
mkdir -p .local/neuradex
cat > .local/neuradex/events.jsonl <<'JSON'
{"event":"neuradex.request","schema_version":"1.0.0","event_id":"evt-local-1","timestamp":"2026-11-10T12:36:56.789Z","service":{"name":"neuradex","version":"0.1.0"},"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","span_id":"00f067aa0ba902b7","request_id":"req-local-1","http":{"method":"GET","route":"/v1/fetch","path":"/v1/fetch","status":200,"duration_ms":42},"outcome":"success","operation":{"type":"fetch","target_scheme":"https","target_host":"example.com","target_path":"/","target_url_sha256":"7d5e1f180f774d3f6d57faa2ae34610fdbfb752d0c0e48d840e3f2d633a27f78","redirects":0,"final_host":"example.com","upstream_status":200,"content_type":"text/html","content_length":5123,"bytes_read":5123,"ttfb_ms":10,"total_ms":42,"truncated":false,"stopped_at_head":false},"content_references":[{"storage":"pack","key":"packs/v1/date=2026-11-10/hour=12/pack-local-1.nrp","blake3":"9f2c000000000000000000000000000000000000000000000000000000000000","sha256":"3a7d000000000000000000000000000000000000000000000000000000000000","size":5123,"content_type":"text/html"}]}
JSON
```

Start Vector:

```sh
docker compose --profile ingest up -d vector
docker compose logs -f vector
```

Verify both sinks:

```sh
# Quickwit: wait a few seconds for the 10 s commit.
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/search' \
  -H 'content-type: application/json' \
  -d '{"query":"*","max_hits":5}' | jq '{num_hits, ids: [.hits[]._source.request_id]}'

# ClickHouse: the row and the daily materialized view.
curl -sS 'http://127.0.0.1:8123/?user=neuradex&password=neuradex-local-secret' \
  --data-binary "SELECT event, request_id, operation_type, http_route, content_bytes FROM neuradex.events WHERE request_id='req-local-1' FORMAT PrettyCompact"

curl -sS 'http://127.0.0.1:8123/?user=neuradex&password=neuradex-local-secret' \
  --data-binary "SELECT * FROM neuradex.events_daily ORDER BY day DESC LIMIT 5 FORMAT PrettyCompact"
```

Quickwit indexes the nested `service`/`http`/`operation` objects and stores arrays as
`{"items":[…]}` because its `json` field is object-only. ClickHouse gets a flat projection with
JSON strings for `content_references` and the header captures. Both come from the same event.

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

For a local S3 client:

```sh
export AWS_ACCESS_KEY_ID=neuradex
export AWS_SECRET_ACCESS_KEY=neuradex-local-secret
export AWS_REGION=us-east-1
export AWS_ENDPOINT_URL=http://127.0.0.1:9000
export AWS_ALLOW_HTTP=true
```

Do not reuse these values outside the local stack. See
[reference/configuration.md](../reference/configuration.md).

## Reset

```sh
# Stop containers, keep volumes.
docker compose down

# Stop and delete all local Quickwit/MinIO/ClickHouse data.
docker compose down -v

# Delete the local event spool (contains metadata; never commit it).
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
| ClickHouse `request_headers` is empty | Vector transform not applied | Ensure the `encode_clickhouse` transform is in the chain |
| MinIO port bind fails | Another MinIO owns 9000/9001 | Use the override above |
| ClickHouse init SQL did not run | Data directory was not empty | `docker compose down -v` or apply DDL manually |
| Vector not shipping | Profile not enabled, or spool file missing | `docker compose --profile ingest up -d vector`; check logs |
| Jaeger has no traces | OTLP endpoint not set, or service not restarted | Export `OTEL_EXPORTER_OTLP_ENDPOINT` and restart `cargo run` |

## Next steps

- [Testing](testing.md) — what to run before opening a PR.
- [Compose services reference](../reference/compose-services.md) — images, ports, volumes.
- [Runbook](../operations/runbook.md) — what to do when the stack misbehaves.
