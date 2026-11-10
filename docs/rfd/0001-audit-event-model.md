---
rfd: 1
title: Audit event model
authors:
  - neuradex maintainers
state: published
discussion: https://github.com/mkroman/neuradex/pull/70
labels:
  - audit
  - architecture
  - storage
created: 2026-11-10
updated: 2026-11-10
---

# RFD 1: Audit event model

## Summary

Record one typed, nested JSON **`AuditEvent`** per inbound request. The event carries a stable
envelope, service and HTTP facts, an `operation` tagged union, bounded header captures, and
content references. It deliberately stores **no query text, no full target URL, no response
bodies, and no search result text**. The audit crate (`neuradex-audit`) is the schema owner; the
service will emit one event per request through a synchronous sink wrapped in an async writer.

## Context and scope

neuradex is an internal Kagi staging service handling real data for named users. We need to
answer, for any request:

- what was requested, when, and with what result;
- which operation (fetch, peek, search, Open WebUI search, infrastructure) ran;
- which upstream host/path was involved and whether redirects occurred;
- whether content was captured and which stored object/frame it lives in;
- which trace to open;
- aggregate questions: error rate by route, latency, search volume, bytes captured.

The event must be safe to store for a year and useful without exposing the query or body. It must
also be testable without the HTTP service.

Out of scope: application metrics (OpenTelemetry metrics), Kagi-internal diagnostics, billing,
and read-audit/auth fields. Read audit is a future event type in RFD 0009; identity fields are
not in schema 1.0.0.

## Goals and non-goals

| Goal | How we know it is met |
| --- | --- |
| One event per request | The completion guard emits exactly one event, including aborts/panics |
| Typed operation fields | Search/fetch/peek/infrastructure fields are distinct, not a bag of empty columns |
| No query text or full URL | Only `query_sha256`/length/terms and `target_*`/`target_url_sha256` are stored |
| Bounded headers | HeaderCapture has fixed per-side/per-value/per-header limits |
| Content without embedding it | Content is an array of `{storage, key, blake3, sha256, size, content_type?}` |
| Stable schema | `SCHEMA_VERSION = "1.0.0"`; optional fields are additive |
| Testable without the service | `neuradex-audit` has no axum, Kagi, content, or object-storage dependency |

Non-goals:

- Exactly-once delivery. The service wraps the synchronous sink in an at-least-once queue.
- A general-purpose logging platform. Application logs stay in the logging pipeline.
- Storing response bodies or result snippets.
- Per-field encryption. Access control and retention are the primary controls.
- A read-audit or auth-subject schema in 1.0.0.

## Options

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| A. One flat event per request with all operation fields | Easy single-table mapping | Wide, mostly-empty schemas; operation-specific validation is weak; field-name collisions between search and fetch | Rejected |
| B. Nested `AuditEvent` with a typed `operation` union | Clear ownership; compile-time operation shapes; smaller JSON; one event still queryable via nested paths | Sinks need nested mappings or a flattening step | **Chosen** |
| C. Separate event types/tables per operation | Tight schemas | Cross-operation queries need unions; duplicate envelope/correlation logic; more mappings | Rejected |
| D. Structured logs only | No new pipeline | No retention control, no typed analytics, no content refs, log sampling loses the record | Rejected |
| E. One event per phase (start, upstream, response, pack) | Fine-grained timing | 3–5× rows; partial records; correlation joins | Rejected |

Option B wins because the operation union is the natural boundary: a search has query digests
and result counts, a fetch has target/redirect/body facts, and infrastructure has a kind. The
shared envelope stays identical.

## Determination

Adopt **Option B**: the `neuradex-audit` `AuditEvent`, serialized with `schema_version: "1.0.0"`
and `event: "neuradex.request"`.

Specifics:

- **One event per inbound request**, including 404/405 fallbacks and aborted requests. A
  completion guard emits the event on normal completion, abort, and panic. `/healthz` is
  currently an infrastructure operation; the service may choose not to emit it in production,
  but the schema can represent it.
- **Operation union**: `search`, `openwebui_search`, `fetch`, `peek`, `infrastructure`.
- **Search** stores `query_length`, `query_terms`, `query_sha256`, optional `limit`/`count`,
  `auth_mode`, queue/upstream timings, upstream/returned result counts, `zero_results`, an
  optional result-URL digest, and up to 10 result hosts. It never stores the query text or full
  result URLs.
- **Fetch/peek** stores target scheme/host/port/path, a SHA-256 of the full target URL,
  redirects, final host/status, content type/length, bytes read, TTFB/total ms, `truncated`, and
  `stopped_at_head`. It never stores the full URL or query string.
- **Infrastructure** stores a kind: `health`, `openapi`, `docs`, `fallback`, `other`.
- **Content references** are an array of `ContentReference` objects. An empty/absent array means
  no content; there is no separate `content_captured` boolean in 1.0.0.
- **Headers** are `HeaderCapture` objects with an allow-list/redact/sanitize/hash policy.
  Unknown headers keep their name and byte length but not their value.
- **No auth subject, read audit, or capture-error fields** in 1.0.0. Those are future work; see
  RFD 0009 and the open questions.

Reversibility: adding optional fields is backward-compatible. Changing a field's meaning or
removing one requires a new `schema_version` and a dual-write window. The event archive is
immutable, so old events are read with the old schema.

## Design

### Envelope

```text
event              "neuradex.request"
schema_version     "1.0.0"
event_id           evt-<pid>-<nanos>-<counter>
timestamp          RFC 3339 UTC
observed_at        RFC 3339 UTC, optional
service            {name, version, environment?}
trace_id           optional
span_id            optional
request_id         req-<pid>-<nanos>-<counter>
http               {method, route?, path, status?, request_body_bytes?, response_body_bytes?, duration_ms}
outcome            success | client_error | server_error | aborted | panic
error_type         optional low-cardinality string
operation          optional tagged union
content_references optional array
request_headers    optional HeaderCapture
response_headers   optional HeaderCapture
```

Timestamps are RFC 3339 with sub-second precision; `Timestamp` normalizes to UTC. `event_id` and
`request_id` are opaque strings with stable prefixes, generated from process ID, Unix
nanoseconds, and a process-local counter. They are not UUIDs; consumers must not parse them.

### Operation union

```text
{"type": "search", "query_length": 16, "query_terms": 3, "query_sha256": "…", ...}
{"type": "fetch", "target_scheme": "https", "target_host": "example.com", ...}
{"type": "infrastructure", "kind": "openapi"}
```

`operation.type` is the discriminator. Optional fields are omitted, not null. Sinks must use
nested mappings (Quickwit) or flatten the union (ClickHouse), not assume a fixed flat schema.

### Content references

```json
{
  "storage": "pack",
  "key": "packs/v1/date=2026-11-10/hour=12/pack-019a.nrp",
  "blake3": "<64 hex>",
  "sha256": "<64 hex>",
  "size": 5123,
  "content_type": "text/html"
}
```

`storage` is `pack` or `object`. `key` is opaque to the audit crate; for a pack it is the pack
object key. The frame offset/length are not in the event; the hourly request index maps
`request_id` to the frame. A future content-plane integration may add a capture-status field;
until then, absence means no content.

### Header capture

The policy is implemented in `crates/neuradex-audit/src/header_policy.rs`:

- Allow-list of safe headers; values captured verbatim.
- Sensitive headers (`authorization`, `cookie`, `set-cookie`, API keys, Open WebUI user
  email/name) redacted; name and byte length remain.
- IP/user/chat identifiers HMAC-SHA256 hashed when a key is configured, otherwise redacted.
- `location`/`referer` sanitized by removing query strings and fragments.
- Unknown headers redacted (name and byte length only), not dropped.
- Limits: 64 headers, 8 KiB per side, 512 bytes per value, 16 values per header.
- Non-UTF-8 values are base64-encoded; control characters are stripped.

The serialized shape is documented in [reference/event-fields.md](../reference/event-fields.md).
It is not a `{name: value}` map.

### Emission and sinks

`RequestRecord` accumulates facts and `finish()` builds the event. `CompletionGuard` ensures one
event on drop, abort, or panic. `AuditSink` is synchronous; the service wraps it in a bounded
async queue/background writer so the request path never blocks on a sink.

The same event JSON is written to:

- Quickwit (nested object mapping, 30 days),
- ClickHouse (flat projection, 365 days),
- the hourly B2 archive (NDJSON.zst, 365 days).

Quickwit stores arrays as `{"items": [...]}` because its `json` field is object-only; this is a
hot-index shaping detail, not an event field. ClickHouse derived columns (`content_bytes`, etc.)
are computed by Vector, not emitted by the crate.

### Schema evolution

- Optional fields may be added within `1.0.x`; customers omit them when unset.
- `SCHEMA_VERSION` is a string; a semantic change to an existing field requires a new version.
- Quickwit uses `mode: lenient`: an unmapped field is ignored, not rejected. Add a mapping and
  reindex when the field must be searchable.
- ClickHouse adds columns before enabling a field; Vector's `skip_unknown_fields` would
  otherwise drop it silently.
- The archive is immutable; do not rewrite old hours.

## Economic, capacity, and performance considerations

At 140,000 requests/hour (3.36M/day), an event of ~1.5–2 KiB (with bounded headers) is about
5–7 GB/day raw, ~1.3–1.8 GB/day at 4× zstd, or ~470–650 GB/year of archive. Quickwit's 30-day
hot copy is ~40–60 GB compressed plus index. ClickHouse at ~500 B/row and 5× compression is
~340 MB/day, ~120 GB/year at 365 days.

The event is serialized once and fanned out. The dominant cost is JSON/header capture and two
network writes, not hashing. Header capture is bounded; a worst-case 8 KiB per side is still
small relative to a 25 MiB body.

## Security considerations

- **No query text.** Only SHA-256, length, and term count; this removes the largest free-text
  privacy risk.
- **No full URL.** Only scheme/host/port/path plus a hash; query values and userinfo are gone.
- **No bodies or result snippets.** Content is referenced, not embedded.
- **Headers are the risk surface.** The allow-list, redaction, HMAC, and limits are the control.
  A sentinel-token test must assert no secret appears in the serialized event or logs.
- **Events contain hostnames, paths, and header names.** Treat the event store as restricted
  internal data, not a public log.
- **No auth subject in 1.0.0.** Named access is enforced at the ingress; the event alone cannot
  attribute a request to a user until RFD 0009 lands. This is a known gap.
- **No per-request deletion.** Retention is 365 days; packs are immutable. A legal deletion
  request is a pack-rewrite problem, not supported initially.
- **Read audit is future work.** Do not claim read accountability from schema 1.0.0.

## Operational considerations

- At-least-once delivery; duplicates are possible. `event_id` is unique, but the current schema
  has no idempotent sink key beyond it.
- A sink outage spools; if the spool fills, events are dropped only after a high-severity alert.
  Content is shed before events.
- Quickwit/ClickHouse are derived; B2 archive is the source of truth. Replay from the archive.
- The archive object key includes close time and writer, so retries never overwrite.
- Alerts: event lag > 5 min, spool depth > 80%, archive gap for a closed hour, duplicate rate >
  1%, ClickHouse insert failures > 0 for 5 min.
- The audit crate has no async runtime; the service must not block the request path on a sink.

## Open questions

| Question | Why it is open | Trigger / owner |
| --- | --- | --- |
| How do we represent "content capture failed" distinctly from "no content"? | The current schema only has `content_references`; absence is ambiguous | Content-plane integration; add an optional capture-status field or a content event |
| Do we need auth subject/user ID fields? | Named access needs attribution; 1.0.0 has only search `auth_mode` and header HMACs | RFD 0009 and ingress review |
| Do we need a read-audit event type? | Current `event` is always `neuradex.request`; `audit.read`/`content.read` are proposed | RFD 0009 implementation |
| Is `result_digest` enough, or do we need a bounded result list? | Digest supports comparison, not inspection; top hosts give aggregate context | Privacy review |
| Should `/healthz` be audited in production? | It is high-volume and unauthenticated; schema supports it, policy may exclude it | Operations review |
| Do we add a `client_ip` or request source? | Useful for abuse investigation; sensitive | Security review |

## References

- [RFD 0002: Content-addressed storage](0002-content-addressed-storage.md)
- [RFD 0003: Hashing](0003-hashing.md)
- [RFD 0005: Header policy](0005-header-policy.md)
- [RFD 0009: Access and read audit](0009-access-and-read-audit.md)
- [Event fields reference](../reference/event-fields.md)
- Code: `crates/neuradex-audit/src/event.rs`, `request_record.rs`, `header_policy.rs`, `guard.rs`
- Oxide RFD 1 / RFD 5
