---
rfd: 1
title: Audit event model
authors:
  - neuradex maintainers
state: published
discussion: TBD
labels:
  - audit
  - architecture
  - storage
created: 2026-11-10
updated: 2026-11-10
---

# RFD 1: Audit event model

## Summary

Record one wide, versioned JSON event per inbound request and per audit-relevant operational
action. Events carry correlation IDs, request metadata, outcome, authenticated principal,
an optional search query, sanitized headers, and content references. They never carry response
bodies, search result text, or unredacted secrets. The same event is written to Quickwit (hot
search), ClickHouse (analytics), and the hourly B2 event archive (cold source of truth).

## Context and scope

neuradex is an internal Kagi staging service handling real data for named users. We need to
answer, for any request:

- what was requested, when, by whom, and with what result;
- which upstream host and URL were involved;
- whether the body was captured, truncated, or missing, and where its frame lives;
- which trace to open, and which pack to inspect.

We also need aggregate questions: error rate by route, latency by upstream, bytes captured per
day, search volume, and who read the audit data. The event model must serve both without
requiring a per-request object store manifest.

Out of scope: application performance metrics (use OpenTelemetry metrics when they exist),
Kagi-internal diagnostics, and billing. Query text is in scope but restricted; see RFD 0009.

## Goals and non-goals

| Goal | How we know it is met |
| --- | --- |
| One event per request, carrying all correlation IDs | A request can be traced from HTTP through pack frame without a join on an external system |
| Stable, documented schema | [reference/event-fields.md](../reference/event-fields.md) matches the serialized example and the sink mappings |
| No bodies or secrets in events | Review of the header policy and a redaction test suite |
| Queryable both ways | The event is ingestible as NDJSON into Quickwit and JSONEachRow into ClickHouse without transformation |
| Safe schema evolution | New fields are additive; old events remain readable; `schema_version` is present |
| Bounded event size | Serialized event ≤ 64 KiB, header strings ≤ 32 KiB each, values capped |

Non-goals:

- Exactly-once delivery. The pipeline is at-least-once and deduplicates by `event_id` when
  needed.
- A general-purpose logging platform. Application logs stay in the logging pipeline.
- Storing response bodies or result snippets. Those belong to the content plane or not at all.
- Per-field encryption. Access control and retention are the primary controls.

## Options

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| A. One wide event per request, flat fields | Simple sink mappings; one row per request; easy to query in both Quickwit and ClickHouse; no joins for the common case | Some fields are empty for some event types; schema is wide | **Chosen** |
| B. Separate tables/indices per endpoint (`fetch_events`, `search_events`, …) | Tight schemas; no empty fields | Cross-route queries need unions; duplicated correlation and access logic; more mappings to maintain | Rejected |
| C. Nested `content` / `auth` objects in JSON | Readable document | Quickwit and ClickHouse flattening differs; query syntax varies; harder to keep two sinks identical | Rejected |
| D. Only structured logs, no dedicated event store | No new pipeline | No retention control, no typed analytics, no read audit, log sampling loses the record | Rejected |
| E. One event per *phase* (start, upstream, response, pack) | Fine-grained timing | 3–5× rows; correlation joins; partial records on crash; the request is not atomic in the audit | Rejected |

Option A wins because the dominant questions are per-request and cross-route, and because the
two sinks must agree byte-for-byte on field names. Empty optional fields are cheaper than joins.

## Determination

Adopt **Option A**: a single flat event schema, serialized as JSON, with `schema_version: 1`.

- The event is the durable record. If content is missing, the event says why.
- `request_id` is the primary correlation key; `event_id` is unique per event so operational
  events can share the schema.
- Search result *text* is never stored. Search events store the query (restricted), query hash,
  result count, and result URLs/titles only if the privacy review allows titles; currently the
  event stores only `result_count` and `target_url` is the first result URL, not the full set.
- Headers are JSON objects in the event, with a documented redaction shape. ClickHouse stores
  them as JSON-encoded strings; Quickwit stores them as `json` fields. See RFD 0005.
- The schema is published in [reference/event-fields.md](../reference/event-fields.md). A code
  change that adds, removes, or changes a field must update that page and this RFD's open
  questions if the change is not purely additive.

Reversibility is high for additive fields and low for changing a field's meaning. Adding a
field is a minor version bump in practice; changing `content_hash_blake3` or `outcome` semantics
requires a new `schema_version` and a migration note.

## Design

### Event types

| `event` | Emitted for | HTTP status present | Content refs |
| --- | --- | --- | --- |
| `fetch` | `GET /v1/fetch` | yes | yes, when capture succeeds |
| `peek` | `GET /v1/peek` | yes | yes for the head bytes, when capture succeeds |
| `search` | `POST /v1/search` | yes | no |
| `openwebui_search` | `POST /v1/openwebui_search` | yes | no |
| `audit.read` | A read of events or content | no (0) | no |
| `content.pack_sealed` | A pack's footer/trailer is complete | no (0) | pack-level |
| `content.pack_uploaded` | A sealed pack is durable in B2 | no (0) | pack-level |
| `content.pack_failed` | A pack upload or verification failed | no (0) | pack-level |
| `content.read` | A content frame was read and verified (success or failure) | no (0) | frame-level |
Every inbound request except `GET /healthz` produces an event, including 404/405 fallbacks and
client errors. Health probes are excluded because they are high-volume, unauthenticated, and
carry no user data.

### Field groups

The complete table is in [reference/event-fields.md](../reference/event-fields.md). The groups
are:

| Group | Fields | Notes |
| --- | --- | --- |
| Envelope | `schema_version`, `event`, `event_id`, `timestamp` | `event_id` is unique; `timestamp` is completion time in UTC |
| Correlation | `request_id`, `trace_id`, `span_id` | `request_id` is required for request events; trace IDs are always available because the service creates spans even when export is off |
| Request | `route`, `method`, `status`, `duration_ms`, `outcome`, `error_type`, `service_version` | `status` is the HTTP status, or 0 for non-HTTP events |
| Principal | `auth_subject`, `auth_method`, `user_id` | Populated by the trusted ingress; never from an untrusted client header |
| Search | `query_text`, `query_hash`, `result_count` | `query_text` is optional and restricted; `query_hash` is always safe to aggregate |
| Target | `target_url`, `upstream_host`, `redirects_followed` | URL is sanitized: userinfo, fragment, and query values removed |
| Content | `content_*` | See RFD 0002 and RFD 0003 |
| Headers | `request_headers`, `response_headers`, `*_headers_truncated` | JSON objects; ClickHouse stores JSON-encoded strings; redaction shape in RFD 0005 |
| Read audit | `read_action`, `read_target`, `read_reason`, `read_bytes` | Only for `audit.read` and `content.read` |

### Example

```json
{
  "schema_version": 1,
  "event": "fetch",
  "event_id": "019a7f3c-6a2e-7c31-9b5a-2f4c1d0e8a11",
  "timestamp": "2026-11-10T12:34:56.789Z",
  "request_id": "019a7f3c-6a2e-7c31-9b5a-2f4c1d0e8a10",
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
  "span_id": "00f067aa0ba902b7",
  "route": "/v1/fetch",
  "method": "GET",
  "status": 200,
  "duration_ms": 120,
  "outcome": "success",
  "error_type": "",
  "service_version": "0.1.0",
  "auth_subject": "user:alice@example.com",
  "auth_method": "mtls",
  "user_id": "alice",
  "query_text": "",
  "query_hash": "",
  "result_count": 0,
  "target_url": "https://example.com/",
  "upstream_host": "example.com",
  "redirects_followed": 0,
  "content_captured": true,
  "content_kind": "fetch_body",
  "content_hash_blake3": "9f2c…64 hex chars…",
  "content_hash_sha256": "3a7d…64 hex chars…",
  "content_bytes": 5123,
  "content_type": "text/html",
  "content_truncated": false,
  "content_pack_id": "019a7f3c-7b10-7a22-8c33-1d2e3f405162",
  "content_frame_offset": 4096,
  "content_frame_length": 1234,
  "content_capture_error": "",
  "request_headers": {"accept": ["*/*"], "user-agent": ["neuradex/0.1"]},
  "response_headers": {"content-type": ["text/html; charset=utf-8"], "content-length": ["5123"]},
  "request_headers_truncated": false,
  "response_headers_truncated": false,
  "read_action": "",
  "read_target": "",
  "read_reason": "",
  "read_bytes": 0
}
```

### Semantics that must not drift

- `timestamp` is when the event is emitted (request completion), not when the request started.
  Use `duration_ms` to reconstruct the start. This makes hourly archive assignment stable.
- `status` is the final status returned to the client. A redirect chain is one event and
  `redirects_followed` counts the hops; intermediate statuses are not separate events.
- `outcome` is derived once, from the final status and the failure class:
  - `success`: 2xx/3xx, or a completed non-HTTP operation.
  - `client_error`: 4xx.
  - `upstream_error`: 5xx caused by the upstream or Kagi.
  - `timeout`: deadline exceeded, including search queue timeout.
  - `internal_error`: 5xx caused by neuradex.
  - `canceled`: client disconnected before completion.
  - `partial`: the response completed but content capture was incomplete.
  - `failed`: non-HTTP operational event failed.
- `error_type` is low-cardinality and stable (`invalid_param`, `unsupported_media_type`,
  `upstream_timeout`, `dns_error`, `tls_error`, `content_spool_full`, …). The human-readable
  error message stays in logs, never in the event.
- `target_url` is the requested URL for fetch/peek and the first result URL for search. It is
  sanitized with the same function as telemetry: no userinfo, no fragment, query values
  replaced with `REDACTED`.
- `query_text` is empty unless the `store_query_text` policy is enabled for the principal.
  `query_hash` is SHA-256 of the exact query string and is always populated for search.
- `result_count` is the number returned to the client after `limit`/`count` truncation.
- `content_bytes` is the number of canonical content bytes hashed, not the compressed frame
  size and not `bytes_read` (which may include bytes beyond a truncation cap).
- `content_captured: false` plus `content_capture_error` is a valid, expected state. It is not
  an error response to the client.

### Schema evolution

- Additive optional fields are allowed within `schema_version: 1`. Quickwit uses
  `mode: lenient`, so an unmapped field is ignored rather than rejecting the document; add the
  mapping and reindex if the field must be searchable. ClickHouse tolerates missing JSON keys
  through Vector's `skip_unknown_fields` and column defaults; add the column before enabling it.
- Removing a field or changing its type/meaning requires `schema_version: 2` and a dual-write
  period. Readers must ignore unknown fields and treat missing optional fields as empty.
- Enumerations (`event`, `outcome`, `error_type`, `auth_method`, `content_kind`, `read_action`)
  are open in storage and validated in code. New values do not require a schema version bump,
  but must be added to [reference/event-fields.md](../reference/event-fields.md).
- The event archive is immutable. A schema fix is applied to new hours; old hours are read with
  the old mapping. Do not rewrite archives in place.

## Economic, capacity, and performance considerations

At 40,000 searches/hour plus 100,000 fetches/hour, the event rate is 140,000/hour or
3.36 million/day. At an average 1.5 KiB serialized (including headers), that is about
5.2 GB/day raw and about 1.3 GB/day at 4× zstd compression, or roughly 470 GB/year of archive.
Quickwit's 30-day hot copy is roughly 40 GB compressed plus index overhead. ClickHouse stores
the same rows with typed columns; at ~500 B/row raw and 5× compression it is roughly
340 MB/day, or 120 GB/year at 365 days.

The per-request cost is dominated by JSON serialization and two network writes, not by hashing
or compression. The pipeline must be asynchronous and bounded; see RFD 0002. The event is
serialized once and fanned out, so Quickwit and ClickHouse see identical bytes apart from the
sink-specific framing.

## Security considerations

- Events contain URLs, user identity, query hashes, and potentially query text. Treat the event
  store as **restricted internal data**, not as a public log.
- Bodies, result snippets, authorization values, cookies, and API keys never enter an event.
- Query text is the most sensitive free-text field. It is off by default in production and
  restricted to named audit readers when enabled. Aggregations use `query_hash`.
- Header values are redacted or HMACed per RFD 0005. The HMAC key is separate from the B2
  credentials and is rotated on a documented schedule.
- Read access is itself audited (`audit.read`), and read audit events are retained at least as
  long as the data they describe. See RFD 0009.
- There is no per-request deletion. Retention is 365 days; a legal deletion request requires a
  pack rewrite, which is not supported initially. This is recorded as a limitation, not hidden.

## Operational considerations

- Event emission is at-least-once. A duplicate is identified by `event_id`; a missing event is a
  page-worthy incident.
- The local spool absorbs sink outages. If the spool fills, events are dropped only after
  logging a high-severity counter; content capture is dropped first because events are more
  important.
- The event archive is written hourly with a grace window. Events that arrive after the close
  go into the next hour's object with their original `timestamp`; readers must not assume
  `timestamp` falls inside the object's hour.
- The archive object key contains the close time and writer ID, so a retry never overwrites a
  previous object.
- Backfill/replay reads the archive, not Quickwit: write a replay job that re-emits NDJSON to
  the hot index and ClickHouse with idempotent keys.
- Alerts: event lag > 5 minutes, spool depth > 80%, archive missing for a closed hour, duplicate
  rate > 1%, ClickHouse insert failures > 0 for 5 minutes.

## Open questions

| Question | Why it is open | Trigger / owner |
| --- | --- | --- |
| Is `request_id` UUIDv7, ULID, or a 128-bit random value? | All are opaque and sortable enough; the choice affects log ergonomics and index locality | Decide with the first audit crate implementation; storage treats it as a string |
| Do we capture peek head bytes as content, or metadata only? | Peek is capped at 2 MiB and may be high-volume; capturing it improves audit completeness but changes capacity | Measure peek volume for one week; default in this design is capture |
| Do we store the full result URL list for search? | Useful for investigation; increases event size and exposes result targets | Privacy review; currently only the first URL |
| Do we need a per-event `client_ip`? | Useful for abuse investigation; sensitive and not required for named access | Security review; currently omitted |
| Should `audit.read` live in the same table/index? | Simpler today; a separate immutable store may be required for read-audit retention | Revisit when read endpoints exist |

## References

- [RFD 0002: Content-addressed storage](0002-content-addressed-storage.md)
- [RFD 0005: Header policy](0005-header-policy.md)
- [RFD 0009: Access and read audit](0009-access-and-read-audit.md)
- [Event fields reference](../reference/event-fields.md)
- [Oxide RFD 1](https://rfd.shared.oxide.computer/rfd/0001), [RFD 5](https://rfd.shared.oxide.computer/rfd/0005)
- Code: `crates/neuradex-audit`, `crates/neuradex/src/api/v1.rs`
