# Quickwit index

**Status:** the local index is created by `quickwit-init` from
`hack/quickwit/neuradex-events.yaml`. Production topology is not specified.

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

Quickwit is the **hot** store. It is not the 365-day source of truth; the B2 archive is. A
missing index is a search incident, not data loss.

## Field mapping

All event fields are mapped. Types:

- `datetime` with `rfc3339` input/output, `fast: true`, `fast_precision: milliseconds`.
- `u64` for counters/status/durations.
- `bool` for flags.
- `text` with `tokenizer: raw` for identifiers/enums; `fast: true` for low-cardinality fields.
- `text` with `tokenizer: default` for `query_text` and `target_url` (searchable prose/URLs).
- `json` for headers, `indexed: false`, `stored: true`.

| Field | Type | Fast | Stored | Notes |
| --- | --- | --- | --- | --- |
| `timestamp` | datetime | yes | yes | RFC 3339, ms precision |
| `schema_version` | u64 | yes | yes | 1 |
| `event` | text raw | yes | yes | Event type |
| `event_id` | text raw | no | yes | Unique |
| `request_id` | text raw | no | yes | Primary lookup |
| `trace_id` | text raw | no | yes | 32 hex |
| `span_id` | text raw | no | yes | 16 hex |
| `route` | text raw | yes | yes | Route template |
| `method` | text raw | yes | yes | HTTP method |
| `status` | u64 | yes | yes | HTTP status or 0 |
| `duration_ms` | u64 | yes | yes | ms |
| `outcome` | text raw | yes | yes | Low cardinality |
| `error_type` | text raw | yes | yes | Low cardinality |
| `service_version` | text raw | no | yes | Version |
| `auth_subject` | text raw | yes | yes | Principal |
| `auth_method` | text raw | yes | yes | mtls/oidc/none/local |
| `user_id` | text raw | yes | yes | Stable user ID |
| `query_text` | text default | no | yes | Restricted |
| `query_hash` | text raw | no | yes | SHA-256 hex |
| `result_count` | u64 | yes | yes | Results |
| `target_url` | text default | no | yes | Sanitized URL |
| `upstream_host` | text raw | yes | yes | Host |
| `redirects_followed` | u64 | no | yes | Hops |
| `content_captured` | bool | yes | yes | Capture flag |
| `content_kind` | text raw | no | yes | fetch_body/peek_head |
| `content_hash_blake3` | text raw | no | yes | Identity |
| `content_hash_sha256` | text raw | no | yes | Audit hash |
| `content_bytes` | u64 | yes | yes | Canonical bytes |
| `content_type` | text raw | no | yes | Media type |
| `content_truncated` | bool | no | yes | Cap hit |
| `content_pack_id` | text raw | no | yes | Pack ID |
| `content_frame_offset` | u64 | no | yes | Frame offset |
| `content_frame_length` | u64 | no | yes | Frame length |
| `content_capture_error` | text raw | no | yes | Why missing |
| `request_headers` | json | no | yes | Object, not string |
| `response_headers` | json | no | yes | Object, not string |
| `request_headers_truncated` | bool | no | yes | Limit hit |
| `response_headers_truncated` | bool | no | yes | Limit hit |
| `read_action` | text raw | yes | yes | Read audit |
| `read_target` | text raw | no | yes | Target |
| `read_reason` | text raw | no | yes | Ticket |
| `read_bytes` | u64 | no | yes | Bytes |

### Header objects are mandatory

Quickwit's `json` field expects a JSON **object**. If an event sends
`"request_headers": "{\"accept\":[\"*/*\"]}"` (a string), the document is rejected with
`doc mapper parse error: expected object`. The canonical event carries objects; ClickHouse
receives JSON text via Vector's `encode_clickhouse` transform. This was verified against
Quickwit 0.8.2.

### Why lenient mode

`mode: lenient` ignores unmapped fields instead of rejecting the document. This lets a service
deploy add a field before the index mapping is updated. The trade-off: the new field is not
searchable until the mapping changes and the index is recreated/reindexed. Use this for
forward-compatible fields; do not rely on it for fields that must be queryable immediately.

## Ingest

Quickwit's ingest API accepts NDJSON:

```sh
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/ingest?commit=auto' \
  -H 'content-type: application/x-ndjson' \
  --data-binary @event.ndjson
```

- `commit=auto` commits according to `commit_timeout_secs` (10 s) or the split policy.
- A document with a wrong field type is rejected; check the Quickwit log.
- Vector's HTTP sink sends NDJSON with `framing.method = "newline_delimited"`.
- Batch size: keep below the ingest content-length limit (10.5 MB locally); Vector's
  `batch.max_events = 1000` is a safe default for ~2 KiB events.

## Search

```sh
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/search' \
  -H 'content-type: application/json' \
  -d '{"query":"event:fetch AND status:200","max_hits":10}' | jq .
```

Query tips:

- Raw fields (`request_id`, `trace_id`, `content_pack_id`) are not tokenized; quote values that
  contain punctuation: `request_id:"req-2"`.
- `status:>=500` works with the numeric fast field.
- `query_text` is tokenized; use it only for restricted searches.
- `target_url` is tokenized; exact URL search may need quotes.
- The default search fields are `query_text`, `target_url`, `upstream_host`.
- `max_hits: 0` returns counts without documents.
- Time filtering uses the `timestamp` field or the API's start/end timestamps.

## Retention

```yaml
retention:
  period: 30 days
  schedule: daily
```

The janitor deletes splits older than 30 days on the daily schedule. This is independent of the
B2 365-day retention and ClickHouse's 365-day TTL. Do not use Quickwit to answer a question about
a request older than 30 days; use the archive or ClickHouse.

## Local operation

```sh
# List indexes.
docker compose exec -T quickwit quickwit index list

# Describe the mapping.
curl -sS 'http://127.0.0.1:7280/api/v1/indexes/neuradex-events' | jq '.index_config.doc_mapping'

# Delete and recreate after a mapping change (destroys hot data).
docker compose exec -T quickwit quickwit index delete --index neuradex-events --yes
docker compose up -d --force-recreate quickwit-init
```

The `quickwit-init` service checks `quickwit index list` and creates the index only if it is
missing. It is idempotent. If the mapping changed, the init script will *not* update the existing
index; delete it first.

### Reindex / replay

Quickwit mappings are not mutable in place. To change a mapping:

1. Create a new index with a new ID (e.g. `neuradex-events-v2`) and the new config.
2. Replay the B2 event archive into the new index.
3. Verify counts and spot-check queries.
4. Point the read path at the new index.
5. Delete the old index after the retention window or when satisfied.

Do not delete the old index before the new one is verified.

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
| `expected object, got "..."` | Header sent as a string | Send an object in the event |
| 0 hits after ingest | Commit not finished | Wait ≥ 10 s; check logs |
| `index already exists` | Init race/manual create | Let init skip; delete only if mapping changed |
| Search slow | Large time range/high cardinality | Narrow the time range; use fast fields |
| Index missing | Volume reset | `docker compose up -d --force-recreate quickwit-init` |
