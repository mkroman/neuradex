# Glossary

Terms are grouped by the plane they belong to. A term marked **planned** has a design but no
runtime implementation in this branch.

## Process

| Term | Meaning |
| --- | --- |
| **RFD** | Request for Discussion. The design-record format used in this repository instead of ADRs. An RFD records context, options, benefits, drawbacks, and a determination. |
| **Determination** | The recorded decision at the end of an RFD. It states the chosen direction, the reasons, and what remains uncertain. |
| **State** | One of `prediscussion`, `ideation`, `discussion`, `published`, `committed`, `abandoned`, adapted from Oxide RFD 1. |
| **Committed** | An RFD whose design is fully implemented. This branch's RFDs are `published` unless noted. |
| **supersedes / superseded by** | A pointer between RFDs when a later decision replaces an earlier one. |
| **Open question** | A decision deliberately left unresolved. It is not an invitation to silently pick a different design in code. |

## Request plane

| Term | Meaning |
| --- | --- |
| **neuradex** | The axum HTTP service exposing fetch, peek, search, and Open WebUI search. |
| **fetch** | `GET /v1/fetch`: fetches a page, extracts document-head metadata, and returns the body as UTF-8 text, capped at 25 MiB. |
| **peek** | `GET /v1/peek`: fetches only the document head, capped at 2 MiB, and returns metadata without the body. |
| **search** | `POST /v1/search`: Kagi search with `query`, `limit`, and `timeout`. |
| **Open WebUI search** | `POST /v1/openwebui_search`: the `external` search shape, returning a bare `[{link,title,snippet}]` array. |
| **request_id** | An opaque identifier generated once per inbound request. It is the primary correlation key for events, content, and read audit. UUIDv7 is the provisional encoding; see RFD 0001. |
| **trace_id / span_id** | OpenTelemetry identifiers. They are recorded in the audit event even when trace export is disabled. |
| **outcome** | A low-cardinality classification of the request: `success`, `client_error`, `upstream_error`, `timeout`, `internal_error`, `canceled`, `partial`, or `failed`. |
| **named access** | The rule that every audit/content reader is an authenticated, identified principal. There is no anonymous read path. |
| **read audit** | **Future work:** an audit event emitted when someone reads audit events or content, recording who, what, why, and how many bytes. Not in schema 1.0.0. |

## Audit plane

| Term | Meaning |
| --- | --- |
| **audit event** | A structured, nested JSON `AuditEvent` for one request. It contains metadata, never bodies, query text, or full target URLs. |
| **event archive** | The hourly, immutable NDJSON (zstd-compressed) object in B2 that contains the full audit events for that hour. It is the cold source of truth. |
| **hot index** | Quickwit's 30-day searchable copy of events. Losing it loses recent search convenience, not the archival record. |
| **analytics store** | ClickHouse's 365-day store of typed event columns and daily aggregates. It serves dashboards and capacity queries. |
| **event schema** | The versioned field set in [reference/event-fields.md](reference/event-fields.md). `schema_version` is `"1.0.0"`. |
| **header policy** | The rules for which upstream request/response headers are captured, redacted, HMACed, or dropped. See RFD 0005. |
| **sensitive header** | A header whose value must never be stored verbatim, such as `authorization`, `cookie`, `set-cookie`, or `x-api-key`. |
| **HMAC-SHA256** | The keyed digest used to record that a sensitive header was present without storing its value. It is not a content hash and cannot be reversed. |
| **event spool** | The bounded local queue/disk buffer that absorbs sink outages. In local development it is `.local/neuradex/events.jsonl`; in production it is a managed local spool. |
| **at-least-once** | The event delivery guarantee: duplicates are possible and are deduplicated by `request_id`/`event_id`; silent loss is not acceptable. |
| **dedupe** | Collapsing duplicate event rows or duplicate content frames. Event duplicates are tolerated; `PackWriter` rejects a duplicate BLAKE3 hash, so the content layer must dedupe before appending. |

## Content plane

| Term | Meaning |
| --- | --- |
| **content** | The decompressed bytes neuradex read from an upstream fetch or peek, captured up to the endpoint's cap. |
| **CAS** | Content-addressed storage. The address is the BLAKE3-256 digest of the content bytes. |
| **BLAKE3-256** | The primary content identity. BLAKE3 is fast, parallel, and used as the CAS key. |
| **SHA-256 audit hash** | An independent SHA-256 digest of the same content bytes, stored for audit and interoperability with standard tooling. |
| **XXH3-64** | A fast non-cryptographic checksum of a *compressed frame*. It detects corruption; it is never an identity and never a key. |
| **packfile** | An immutable object containing self-describing frames (from offset 0), a sorted footer index, and a 128-byte trailer. Target 128 MiB. |
| **frame** | One content record: a 96-byte header (magic `NDPF`, lengths, BLAKE3, SHA-256, XXH3) followed by a zstd payload. |
| **footer index** | The sorted 64-byte-entry index mapping BLAKE3 hashes to frame offsets/lengths, with an optional same-order SHA-256 table. It is a rebuildable index; frames are self-describing. |
| **trailer** | The fixed 128-byte tail pointing at the footer, carrying pack BLAKE3/SHA-256 and a copy of the footer checksum. |
| **pack_id** | A 128-bit opaque object-key identifier, unique per pack. It is not encoded in the pack; events reference the pack object key. |
| **sealed pack** | A pack whose frame set is final and whose footer/trailer have been written. Only sealed packs are uploaded. |
| **request index** | An hourly, immutable object mapping `request_id` to pack ID, frame offset/length, and content hashes. It is not a per-request manifest. |
| **manifest** | A per-request object listing content. Deliberately *not* used; the footer plus hourly index replace it. |
| **large-body threshold** | 30 MiB. A body at or above it is intended to be stored as a standalone CAS object. Because the capture cap is 25 MiB, the threshold is effectively disabled today. |
| **capture error** | A marker on an event saying why content is absent or partial, e.g. `client_disconnect`, `spool_full`, `upstream_error`. |
| **truncated** | The captured bytes were cut at the endpoint cap. The hash identifies the captured prefix, not the full upstream body. |
| **content ref** | A `content_references[]` entry (`storage`, `key`, `blake3`, `sha256`, `size`, optional `content_type`) that locates stored content. The frame offset/length are in the hourly request index. |
| **dedupe window** | The in-memory hash map used by one pack builder to avoid writing the same body twice into the same pack. There is no global content index today. |

## Storage and analytics

| Term | Meaning |
| --- | --- |
| **B2** | Backblaze B2, accessed through its S3-compatible API using the official `aws-sdk-s3` Rust SDK. |
| **S3 API** | The object API used for B2. The service uses `aws-sdk-s3`, not `object_store`. |
| **bucket** | A B2 bucket. `neuradex-content` holds packs, indexes, and archives; `neuradex-quickwit` is reserved for Quickwit remote storage. |
| **prefix** | A key namespace inside a bucket, e.g. `packs/v1/date=2026-11-10/hour=01/`. |
| **Quickwit** | The full-text search engine for hot audit events. Version 0.8.2; 30-day retention. |
| **index** | A Quickwit index with a fixed document mapping. `neuradex-events` is the local index. |
| **split** | A Quickwit immutable segment. Splits are published by the indexer after `commit_timeout_secs`. |
| **ClickHouse** | The columnar analytics database. Database `neuradex`, table `events`, daily materialized views. |
| **MergeTree** | The ClickHouse table engine used for `events`: partitioned by date, ordered by `(timestamp, request_id)`, TTL 365 days. |
| **materialized view** | A ClickHouse view that writes aggregates into a target table on insert. Two daily views exist: `events_daily` and `content_daily`. |
| **hot / cold** | Hot = Quickwit, 30 days, low-latency search. Cold = B2 archive, 365 days, source of truth. ClickHouse is the 365-day analytical middle. |
| **Jaeger** | Local OTLP trace UI and collector. Version 2.22.0 in the compose stack. |
| **OTLP** | OpenTelemetry Protocol. The service exports traces/logs over OTLP/HTTP when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. |
| **Vector** | Optional local log shipper. It tails the event spool and writes NDJSON to Quickwit and JSONEachRow to ClickHouse. |
| **lifecycle rule** | A B2 bucket rule that expires objects by prefix/age. It is a safety net for the 365-day policy, not a per-request delete mechanism. |

## Capacity and operations

| Term | Meaning |
| --- | --- |
| **pack target** | 128 MiB of encoded pack bytes. The builder seals when the next frame would exceed it. |
| **pack max age** | 10 minutes from the first frame. It bounds content staleness at low traffic; at the modelled 4 MiB average, size triggers first (about every 1.2 seconds). |
| **steady state** | The storage occupied after one full retention period at a constant ingest rate. |
| **RPO** | Recovery point objective. For events, the spool bound; for content, the last successfully uploaded pack. |
| **RTO** | Recovery time objective. Target for restoring search/analytics after a component loss. |
| **read amplification** | Bytes read to serve one lookup. A future range-read adapter can keep it to the footer plus one frame; the current `PackReader` takes the full pack slice. |
| **cardinality** | The number of distinct values in a field. `request_id` is high-cardinality; `event`, `route`, and `status_class` are low. |
| **TTL** | Time to live. ClickHouse drops partitions older than 365 days; Quickwit deletes splits older than 30 days; B2 lifecycle expires objects at 365 days. |
| **degradation mode** | A documented behaviour under failure, e.g. "events continue, content capture drops and is marked". |
| **break-glass** | A time-bound, logged elevation path for incident access, requiring a ticket and a second approver. |
