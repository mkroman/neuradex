---
rfd: 2
title: Content-addressed storage
authors:
  - neuradex maintainers
state: published
discussion: https://github.com/mkroman/neuradex/pull/70
labels:
  - content
  - storage
  - architecture
created: 2026-11-10
updated: 2026-11-10
---

# RFD 2: Content-addressed storage

> **Status:** the packfile format and content writer are implemented and wired into the service.
> `CONTENT_STORE` selects `off` (default), a local directory, or an S3-compatible bucket. The
> hourly request index and event archive are target design, not current service writes.

## Summary

Store captured request/response bodies in immutable **packfiles** in a local directory or an
S3-compatible bucket. A pack targets 128 MiB of appended plaintext bytes. The service flushes
every 5 seconds, and each flush seals every open pack; the 10-minute max-age policy is a
backstop rather than the normal trigger. Frames start at offset 0 and are self-describing; each
pack ends with a sorted 64-byte footer index (plus an optional SHA-256 audit table) and a
128-byte trailer. The target design adds an **hourly request index** mapping `request_id` to the
pack, frame offset/length, and hashes; the current service resolves a pack reference by footer
lookup. There are no per-request manifests and no per-request objects.

Content identity is the BLAKE3-256 digest of the captured bytes. SHA-256 is stored for audit.
XXH3-64 checksums the compressed frame only.

## Context and scope

At 100,000 fetches/hour with an assumed 4 MiB average body, the service would produce about
9.15 TiB/day. A per-request object design would create 2.4 million objects/day and at least as
many PUTs, plus a manifest object per request if we wanted to find content later. That is
expensive in requests, metadata, and operational complexity, and it makes listing/backfill
pathological.

The content plane must:

- keep the request path non-blocking, even when B2 is slow or unavailable;
- make a request's content findable from its audit event alone (plus one hourly index lookup);
- avoid per-request S3 operations;
- detect corruption on read;
- support 365-day retention without immediate per-request deletion;
- leave room for a future large-body path.

Out of scope: content search, OCR, HTML re-parsing, and serving content back through the API
(the read path is an internal tool/API in RFD 0009, not a public endpoint).

## Goals and non-goals

| Goal | How we know it is met |
| --- | --- |
| No per-request object or manifest | A request produces zero S3 PUTs; a pack produces one |
| Bounded pack size and age | Every sealed pack is ≤ target + one frame and ≤ 10 minutes old at seal |
| Find content from `request_id` | Hourly index lookup returns pack ID and frame range |
| Verify integrity on read | XXH3, BLAKE3, and SHA-256 checks pass before bytes are returned |
| Survive B2 outages | Local spool absorbs; events mark content as missing, not silently lost |
| Cost-predictable | Pack count and bytes/day are modelled and measurable |
| Future large bodies | A body ≥ 30 MiB can be stored standalone without changing the event schema |

Non-goals:

- Global deduplication across all packs. Dedupe is within one open pack only.
- Per-request deletion or in-place mutation. Packs are immutable.
- A content catalog database. The hourly index and footer are the catalog.
- Storing search result snippets as content.

## Options

### Storage layout

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| A. One object per request + per-request manifest | Simple mental model; independent retention; easy delete | 2.4M+ objects/day; PUT cost and rate limits; listing and lifecycle pain; manifest is another object to lose; no dedupe | Rejected |
| B. One object per request, metadata in the event only | No manifest; event is enough | Still 2.4M objects/day; no packing/dedupe; large object count; multipart overhead for small bodies | Rejected |
| C. Packfiles with footer index + hourly request index | One PUT per 128 MiB; content-addressable frames; footer is a rebuildable index; range-friendly; duplicate rejection at the writer | More complex format; pack loss loses many requests; index must be written after upload; pack rewrite needed for deletion | **Chosen** |
| D. Database BLOB (Postgres/ClickHouse) | Transactional; easy query | Wrong tool for 9 TiB/day; expensive; bloats the analytics store; backup/restore pain | Rejected |
| E. Object store + external catalog DB | Flexible queries | Catalog becomes a second source of truth; consistency and failure handling; still needs objects | Rejected |

### Pack index location

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| Footer index in the pack | One object; index and data cannot diverge; range-readable | Footer must be written last; a corrupt tail loses the index | **Chosen** |
| Sidecar index object | Easy to read without touching the pack | Two objects to keep consistent; another failure point; per-pack metadata cost | Rejected |
| Index only in the hourly request index | No footer duplication | Cannot recover without the hourly index; no content-hash lookup within a pack | Rejected |

### Large bodies

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| Always pack | One code path | A 25 MiB frame is fine; a future >128 MiB body would violate the pack target | Chosen for now, with a guard |
| Standalone CAS object at ≥ 30 MiB | Keeps packs small; simple range reads | A second read path and object layout | **Specified**, effectively disabled because capture caps at 25 MiB |
| Reject large bodies | Simple | Loses content | Rejected |

## Determination

Adopt **Option C** for layout, **footer index** for pack indexing, and **standalone CAS** for a
body at or above the 30 MiB threshold.

Specifics:

- **No pack header.** Frames start at offset 0. The format has no pack ID or creation time;
  those are object-key/builder metadata.
- Frame header: **96 bytes**, magic `NDPF`, version, flags (bit 0 = zstd), `plaintext_len` u64,
  `compressed_len` u32, reserved u32, BLAKE3[32], SHA-256[32], XXH3-64 u64, then the zstd
  payload. There is no content type or truncation flag in the frame.
- Footer: **32-byte header** (magic `NDPX`, version, flags, entry count, reserved, XXH3-64
  checksum, reserved), then **64-byte entries** (BLAKE3[32], frame offset u64, compressed length
  u32, plaintext length u32, XXH3 u64, flags u16, reserved u16, reserved[4]), then an optional
  **32-byte SHA-256 audit table** in entry order. Entries are strictly sorted by BLAKE3.
- Trailer: **128 bytes**, magic `NDPKTRLR`, version, flags, reserved, footer offset u64, footer
  length u64, pack BLAKE3[32], pack SHA-256[32], footer checksum copy u64, and 24 reserved bytes
  for a future keyed-BLAKE3 extension. The pack hashes cover every byte before the trailer.
- The writer always zstd-compresses; there is no raw codec in v1. It rejects a duplicate BLAKE3
  hash in the same pack (`PackError::DuplicateHash`); the content layer must dedupe before
  calling `append`.
- The format does **not** enforce the pipeline's 128 MiB target, 10-minute age, or 25 MiB body
  cap. Those are content-layer policies. Format limits are the u32 length fields.
- Maximum captured body: **25 MiB** (the existing fetch cap). Peek head capture is capped at
  **2 MiB**.
- Large-body threshold: **30 MiB**. Because 30 MiB > 25 MiB, no body can reach it today. The
  code path and event fields exist so a future cap increase does not require a schema change.
- A pack is uploaded only after its footer and trailer are complete and locally fsynced. An
  empty pack is valid to the format but is not uploaded.
- The request index is written **after** the pack upload completes. An orphan pack is safe;
  an index pointing at a missing pack is not.
- Objects are immutable and keyed by date/hour and a unique pack ID. A retry uses a new key, so
  it never overwrites a previous object.

Reversibility: the event schema is independent of the pack format, so the pack format can be
versioned (`pack format v1`) and new packs can use v2 while old packs remain readable. Changing
the *identity* hash is much harder and needs RFD 0003.

## Design

### Object layout

The current service writes packs grouped by operation:

```text
<CONTENT_LOCAL_PATH or s3://CONTENT_S3_BUCKET/CONTENT_S3_PREFIX>/
  packs/{operation}/{yyyy-mm-ddTHH}/{pid}-{sequence}.pack
  objects/{blake3}                              # body at/above the 30 MiB threshold
```

The target design adds:

```text
  index/v1/date=YYYY-MM-DD/hour=HH/requests-<closed_at>-<writer>.ndjson.zst
  events/v1/date=YYYY-MM-DD/hour=HH/events-<closed_at>-<writer>.ndjson.zst
```

- A pack key is fixed when the pack opens; `operation` is sanitized to a safe lowercase name,
  and the hour is the pack creation hour.
- The current service does **not** write the hourly request index or event archive. It also does
  not write a per-request manifest.
- A large body is stored as `objects/{blake3}`, not as a frame.
- `neuradex-quickwit` is reserved for future Quickwit remote storage; the local stack uses a
  file-backed index root.

### Pack lifecycle

```mermaid
stateDiagram-v2
    [*] --> Open: first frame appended
    Open --> Open: append frame\n(size < 128 MiB,\nage < 10 min)
    Open --> Sealing: size target reached\nor max age reached\nor frame cap reached
    Sealing --> Sealed: write footer + trailer\nfsync local file
    Sealed --> Uploading: PUT to B2
    Uploading --> Uploaded: 2xx + HEAD size check
    Uploading --> Retry: 5xx / timeout / short write
    Retry --> Uploading: exponential backoff
    Retry --> Failed: retry budget exhausted
    Uploaded --> Indexed: hourly index object written
    Indexed --> [*]
    Failed --> [*]: no durable content reference
```

Notes:

- A pack is **open** only in the builder process. It is not visible to readers until the index
  is written.
- The builder keeps a local temp file (spool). It does not hold the whole 128 MiB in memory.
- On upload failure, the local file is retained for the retry budget; after that, the pack is
  failed and events referencing it have no durable content; the content plane reports the
  failure (capture-status fields are future schema work).
- An upload that succeeds but whose index write fails leaves an orphan pack. It is harmless and
  can be discovered by a reconciliation job; it must not be deleted blindly because a later
  index might reference it.

### Frame and footer

The byte-level format is in [reference/packfile-format.md](../reference/packfile-format.md).
The parts that matter here:

- Frames start at offset 0 and are self-describing: a 96-byte header carries BLAKE3, SHA-256,
  XXH3-64 of the compressed payload, plaintext/compressed lengths, and flags, followed by the
  zstd payload. There is no pack header, content type, or truncation flag in the frame.
- The footer is a sorted index: a 32-byte header, 64-byte entries keyed by BLAKE3, and an
  optional same-order SHA-256 audit table. It can be rebuilt by scanning frame headers.
- The trailer is 128 bytes, points at the footer, copies the footer checksum, and carries
  BLAKE3/SHA-256 over all bytes before the trailer.
- Frames are the source of truth. The footer is a rebuildable index; the request index is the
  request→frame lookup. If the request index and footer disagree, verify the frame header and
  rebuild the footer rather than trusting either blindly.

### Request index

The request index is an hourly, zstd-compressed NDJSON object. One line per request event with
captured content:

```json
{"request_id":"req-...","event_id":"evt-...","pack_key":"packs/fetch-response/2026-11-10T12/4242-0.pack","frame_offset":4096,"frame_length":1234,"blake3":"...","sha256":"...","size":5123,"content_type":"text/html","truncated":false}
```

Properties:

- Immutable. A retry writes a new object with a new `closed_at`/`writer` suffix.
- Sorted by `request_id` for predictable compression and optional binary search.
- Contains a header record with `schema_version`, `hour`, `count`, and `created_at`.
- Does not contain bodies or header values.
- Can be rebuilt from the event archive + pack footers if lost. The footer alone cannot tell you
  which request produced a frame; the event archive can.

`request_id → frame` is a one-to-many relationship in principle (a future retry could capture
the same request twice). The index stores one record per captured request; a duplicate is a
replay anomaly and is resolved by `event_id` at read time.

### Dedupe

- `PackWriter::append` rejects a duplicate BLAKE3 hash in the same pack
  (`PackError::DuplicateHash`). The content layer must check its in-memory `BLAKE3 → frame` map
  first and point the second request at the existing frame, or treat the rejection as a bug.
- Across packs, there is no global content index. The same page fetched tomorrow is written
  again. This is deliberate: a global index is another consistency problem.
- A future content index could map BLAKE3 → (pack, frame) and skip duplicate frames across
  packs. It would be an optimization, not a correctness requirement.

### Read path

```mermaid
sequenceDiagram
    participant R as Audit reader
    participant S as neuradex read API
    participant I as Hourly request index (B2)
    participant P as Pack (B2)
    R->>S: request_id
    S->>I: GET index object for the request's hour
    I-->>S: pack key, frame offset, frame length, hashes
    S->>P: GET pack (or range GET trailer/footer/frame in a future adapter)
    P-->>S: pack bytes
    S->>S: PackReader::new validates trailer, footer, pack hashes
    S->>S: lookup BLAKE3 -> footer entry
    S->>S: decode frame header, verify entry match
    S->>S: verify XXH3, decompress, verify BLAKE3 + SHA-256
    S-->>R: bytes + content metadata
```

- `neuradex-content` reads a pack with range GETs: the trailer, the footer, then the selected
  frame. `PackReader` in `neuradex-packfile` still takes a complete pack slice; the range adapter
  lives in the content crate and verifies the frame-level hashes.
- The footer may be cached in memory by pack key; it is immutable.
- Frames are self-describing. If the request index and footer disagree, verify the frame header;
  a footer mismatch means the index or footer must be rebuilt, not that the bytes changed.
- If the pack is missing (lifecycle expiry, B2 loss), the reader returns a typed "content
  expired or unavailable" result; the audit event still exists.

### Failure and backpressure

| Failure | Behaviour | What the event records |
| --- | --- | --- |
| Capture channel full | Drop content for this request; never block the response | No `content_references`; capture status is future |
| Local spool disk full | Same as above, plus a high-severity log | No reference; capture status is future |
| Client disconnects mid-body | Do not store a partial frame as canonical content | No reference; `outcome=aborted` |
| Upstream error before body | No content | No reference |
| Pack upload fails after retries | Sealed pack retained locally for inspection; no durable ref | No reference; content plane reports failure |
| Index write fails after upload | Orphan pack; readers cannot find it; reconciliation can rebuild | Reference is unresolvable until reconciled |
| Hash mismatch on read | Do not return bytes; record verification failure | `content.read` with `failed` |
| Clock skew across builders | Pack age may seal early/late; object key still unique | No event; metric |

The content path has lower priority than the event path. If both are under pressure, shed
content first.

## Economic, capacity, and performance considerations

At the assumed 4 MiB average body and 100,000 fetches/hour:

| Quantity | Value |
| --- | --- |
| Captured bytes/day | 9.15 TiB |
| Pack count/day at 128 MiB | ~75,000 |
| Pack count/hour | ~3,125 (one every ~1.2 s) |
| PUTs/day | ~75,000 (one per pack) |
| 365-day steady-state bytes | ~3.34 PiB |
| B2 storage at $6/TiB-month | ~$20k/month (see RFD 0008) |

The service flushes every 5 seconds, which seals every open pack; the 10-minute age policy is a
backstop. At 4 MiB average, 32 fetches fill a 128 MiB pack in about 1.2 seconds, so size (or the
next 5-second flush) triggers first. At low traffic, the 5-second flush creates smaller packs.

Network is the bottleneck: 9.15 TiB/day is about 111 MiB/s average, or ~0.93 Gbit/s, before
peak. At a 2× peak it is ~1.9 Gbit/s. Hashing and zstd are a small fraction of one core at this
rate on modern hardware, but the pack builder must be measured, not assumed.

If the average body is 512 KiB instead, the 365-day steady state is ~418 TiB and ~$2.5k/month.
The cost is linear in average body size; see RFD 0008 for the sensitivity table and the
measurement gate.

## Security considerations

- Packfiles contain real page bodies, which may include personal data, tokens in URLs, or
  third-party content. Treat `neuradex-content` as restricted data.
- B2 credentials are scoped to the bucket and prefix. The service needs `PutObject`,
  `GetObject`, `HeadObject`, and `ListBucket` for reconciliation; it does not need
  `DeleteObject` in normal operation. Lifecycle deletion is a bucket policy.
- Encryption: TLS to B2; B2 server-side encryption at rest. If a customer-managed key is
  required, use SSE-C with a key stored outside the service image.
- Pack IDs and hashes are not secrets, but the request index maps a known request ID to a frame.
  Access to the index must be as restricted as access to events.
- No per-request deletion: deleting one request means rewriting a pack, which changes its
  identity and every referencing event. Retention is the primary control. A legal deletion
  process is an open question, not a supported operation.
- Read access is audited. Do not expose B2 directly to analysts when an audited read API can
  mediate it.

## Operational considerations

- One pack builder per process is enough initially. Multiple builders are safe because pack IDs
  and index keys are unique, but each writes its own packs; dedupe does not cross builders.
- The builder must be crash-safe: a partial local pack is discarded on startup, never uploaded.
- Reconciliation: list packs under a day prefix, compare against the index objects, and mark
  orphan packs. Do not delete orphans automatically until the index retention window has
  passed.
- Lifecycle: expire `packs/`, `index/`, `events/`, and `cas/` prefixes at 365 days. Lifecycle
  is a safety net; it can run hours late. Do not rely on it for a precise deletion SLA.
- Metrics: open pack bytes/age, seal reason (size/age/frame cap), upload latency, upload
  failures, index write failures, frames per pack, compression ratio, dedupe ratio, read
  verification failures, range GET bytes.
- Runbook: [operations/runbook.md](../operations/runbook.md#content-is-missing).

## Open questions

| Question | Why it is open | Trigger / owner |
| --- | --- | --- |
| Should peek head bytes be captured? | Adds ≤2 MiB per peek; improves completeness; not modelled in capacity | Measure peek volume; default here is yes |
| Is a single PUT enough for a 128 MiB target plus one 25 MiB frame, or should we use multipart? | B2/S3 single PUT supports it, but retry and timeouts differ | Load-test in staging; if p99 PUT > 60 s, switch to multipart |
| Do we need a global content index for dedupe? | Could cut storage if pages repeat; adds a consistency problem | Measure duplicate BLAKE3 rate over 30 days |
| What is the pack loss blast radius we accept? | A lost pack loses up to 128 MiB of bodies; events remain | Decide with B2 durability/versioning review |
| How do we perform a legal per-request deletion? | Packs are immutable; rewrite is expensive | Legal/security review; document as unsupported until then |
| Should the request index be sorted binary rather than NDJSON? | Faster lookup, less compression-friendly | Measure index GET/parse time at 2.4M requests/day |

## References

- [RFD 0003: Hashing](0003-hashing.md)
- [RFD 0004: Storage and analytics](0004-storage-and-analytics.md)
- [RFD 0008: Capacity, cost, and retention](0008-capacity-cost-and-retention.md)
- [Packfile format](../reference/packfile-format.md)
- [Content plane](../architecture/content-plane.md)
- Code: `crates/neuradex-content`, `crates/neuradex-packfile`
