# Quickwit index

**Status:** the local index is created by `quickwit-init` from
`hack/quickwit/neuradex-events.yaml`. The service does not write to Quickwit directly; the
optional Vector profile tails the audit JSONL and ingests it. The mapping follows the serialized
`AuditEvent` (schema `1.0.0`), including its nested `service`, `http`, and `operation` objects.

## Index identity

| Property | Value |
| --- | --- |
| Index ID | `neuradex-events` |
| Config version | `0.8` |
| Node config version | `0.7` (Quickwit 0.8.x) |
| Timestamp field | `timestamp` |
| Retention | 30 days, evaluated daily |
| Doc mapping mode | `lenient` |
| Store source | `true` |
| Commit timeout | 10 seconds |
| Local index root | `file:///quickwit/qwdata/indexes` |
| Local metastore | `file:///quickwit/qwdata/indexes` |

In the local stack Quickwit is an optional **hot** view fed by Vector. It is not the source of
truth; the service's audit JSONL is the current record, and a B2 event archive is target design.
A missing index is a search incident, not service data loss.

## Mapping

The event is nested. Quickwit maps the `service`, `http`, and `operation` objects with `object`
fields; the header captures and content-reference array are stored as JSON.

| Path | Type | Fast | Notes |
| --- | --- | --- | --- |
| `event` | text raw | yes | Always `neuradex.request` |
| `schema_version` | text raw | no | `1.0.0` |
| `event_id` | text raw | no | Unique |
| `timestamp` | datetime | yes | RFC 3339, milliseconds |
| `observed_at` | datetime | yes | Optional |
| `service.name` | text raw | yes | `neuradex` |
| `service.version` | text raw | no | Package version |
| `service.environment` | text raw | yes | Optional |
| `trace_id` | text raw | no | Optional |
| `span_id` | text raw | no | Optional |
| `request_id` | text raw | no | Request correlation |
| `http.method` | text raw | yes | HTTP method |
| `http.route` | text raw | yes | Matched route; absent for fallback |
| `http.path` | text default | no | Request path |
| `http.status` | u64 | yes | Optional |
| `http.request_body_bytes` | u64 | no | Optional |
| `http.response_body_bytes` | u64 | no | Optional |
| `http.duration_ms` | u64 | yes | Duration |
| `outcome` | text raw | yes | success/client_error/server_error/aborted/panic |
| `error_type` | text raw | yes | Optional |
| `operation.type` | text raw | yes | search/openwebui_search/fetch/peek/infrastructure |
| `operation.query_length` | u64 | no | Search |
| `operation.query_terms` | u64 | no | Search |
| `operation.query_sha256` | text raw | no | Search; no query text |
| `operation.limit` | u64 | no | `/v1/search` |
| `operation.count` | u64 | no | Open WebUI |
| `operation.auth_mode` | text raw | yes | privacy_pass/session |
| `operation.queue_ms` | u64 | yes | Search queue |
| `operation.upstream_ms` | u64 | yes | Upstream search |
| `operation.upstream_result_count` | u64 | no | Kagi result count |
| `operation.returned_result_count` | u64 | no | Client result count |
| `operation.zero_results` | bool | yes | Empty result set |
| `operation.result_digest` | text raw | no | SHA-256 of result URLs |
| `operation.top_hosts` | json | no | Wrapped as `{"items":[…]}` for Quickwit |
| `operation.target_scheme` | text raw | no | Fetch/peek |
| `operation.target_host` | text raw | yes | Fetch/peek |
| `operation.target_port` | u64 | no | Fetch/peek |
| `operation.target_path` | text default | no | Fetch/peek; default search field |
| `operation.target_url_sha256` | text raw | no | Full-URL hash |
| `operation.redirects` | u64 | no | Redirect count |
| `operation.final_host` | text raw | yes | Fetch/peek |
| `operation.upstream_status` | u64 | yes | Fetch/peek |
| `operation.content_type` | text raw | yes | Fetch/peek |
| `operation.content_length` | u64 | no | Fetch/peek |
| `operation.bytes_read` | u64 | no | Fetch/peek |
| `operation.ttfb_ms` | u64 | yes | Fetch/peek |
| `operation.total_ms` | u64 | yes | Fetch/peek |
| `operation.truncated` | bool | yes | Body cap hit |
| `operation.stopped_at_head` | bool | yes | Peek stopped at head |
| `operation.kind` | text raw | yes | Infrastructure kind |
| `content_references` | json | no | Wrapped as `{"items":[…]}` for Quickwit |
| `request_headers` | json | no | HeaderCapture object |
| `response_headers` | json | no | HeaderCapture object |

### Array shaping

Quickwit's `json` field accepts a JSON object, not a top-level array. The event has two array
values: `content_references` and `operation.top_hosts`. Vector's `shape_quickwit` transform
wraps them as:

```json
{"items": [{"storage": "pack", "key": "packs/…", "blake3": "…", "sha256": "…", "size": 5123}]}
```

The archive and ClickHouse keep the original arrays. Query the wrapper with JSON accessors or use
ClickHouse for array analytics. Do not send an unwrapped array to Quickwit's `json` field; the
document is rejected with `expected object`.

### Lenient mode

`mode: lenient` ignores unmapped fields instead of rejecting the document. This lets a service
deploy add a field before the mapping is updated. The trade-off: the new field is not searchable
until the mapping changes and the index is recreated/reindexed.

## Ingest

```sh
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/ingest?commit=auto' \
  -H 'content-type: application/x-ndjson' \
  --data-binary @event.ndjson
```

- Vector sends the output of `shape_quickwit` as NDJSON.
- `commit=auto` commits according to `commit_timeout_secs` (10 s) or the split policy.
- A document with a wrong field type is rejected; check the Quickwit log.
- Keep batches below the ingest content-length limit (10.5 MB locally). Vector uses
  `batch.max_events = 1000`.

## Search

```sh
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/search' \
  -H 'content-type: application/json' \
  -d '{"query":"http.route:/v1/fetch AND http.status:>=500","max_hits":10}' | jq .
```

Query tips:

- Nested fields use dot paths: `http.route`, `operation.type`, `operation.target_host`.
- Raw fields (`request_id`, `operation.query_sha256`, `content_references`) are not tokenized;
  quote values with punctuation: `request_id:"req-2"`.
- `http.status:>=500` works with the numeric fast field.
- There is no query text in the event; search `operation.query_sha256` for an exact query hash or
  `operation.target_path` for a fetch path.
- `max_hits: 0` returns counts without documents.
- Time filtering uses `timestamp` or the API's start/end timestamps.
- `content_references`/`request_headers`/`response_headers` are stored but not indexed; retrieve
  them from `_source`, not a search query.

## Retention

```yaml
retention:
  period: 30 days
  schedule: daily
```

The janitor deletes splits older than 30 days on the daily schedule. This is independent of the
service's JSONL audit log and ClickHouse's local 365-day TTL. Do not use Quickwit for a request
older than 30 days; use the JSONL log, ClickHouse, or the target B2 archive.

## Local operation

```sh
docker compose exec -T quickwit quickwit index list
curl -sS 'http://127.0.0.1:7280/api/v1/indexes' | jq '.[0].index_config.doc_mapping'
docker compose exec -T quickwit quickwit index delete --index neuradex-events --yes
docker compose up -d --force-recreate quickwit-init
```

`quickwit-init` checks `quickwit index list` and creates the index only if it is missing. It is
idempotent. If the mapping changed, delete the index first; Quickwit mappings are not mutable in
place.

### Reindex / replay

1. Create a new index with a new ID and the new config.
2. Replay the B2 event archive into it (apply `shape_quickwit` if arrays are present).
3. Verify counts and spot-check nested queries.
4. Point the read path at the new index.
5. Delete the old index after verification.

## Local vs production

| Aspect | Local | Production |
| --- | --- | --- |
| Node | Single all-in-one | Cluster/HA TBD |
| Metastore | File | S3/Postgres TBD |
| Index root | File volume | S3 (`neuradex-quickwit`) TBD |
| Auth | None, localhost | Internal auth/TLS |
| Retention | 30 days | 30 days |

## Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `unknown variant millis` | `fast_precision` typo | Use `milliseconds` |
| `expected object, got "..."` | Header sent as a string, or array not wrapped | Send an object; use `shape_quickwit` for arrays |
| `array of values but a single value expected` | `content_references`/`top_hosts` not wrapped | Apply `shape_quickwit` |
| 0 hits after ingest | Commit not finished | Wait ≥ 10 s; check logs |
| `index already exists` | Init race/manual create | Let init skip; delete only if mapping changed |
| Search slow | Large time range/high cardinality | Narrow the range; use fast fields |
| Index missing | Volume reset | `docker compose up -d --force-recreate quickwit-init` |
