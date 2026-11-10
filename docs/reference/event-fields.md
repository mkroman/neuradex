# Event fields

This is the schema emitted by `neuradex-audit` (`SCHEMA_VERSION = "1.0.0"`). It is **not** flat:
`service`, `http`, `operation`, `content_references`, and the header captures are nested. The
source of truth is `crates/neuradex-audit/src/event.rs`; this page mirrors its serde output.

**Status:** the audit crate is implemented (PR #67). The service does not emit events yet; the
read-audit and auth fields in [RFD 0009](../rfd/0009-access-and-read-audit.md) are future work
and are not part of this schema.

## Top level

| Field | JSON type | Required | Description |
| --- | --- | --- | --- |
| `event` | string | yes | Always `neuradex.request` |
| `schema_version` | string | yes | `"1.0.0"` |
| `event_id` | string | yes | `evt-<pid>-<nanos>-<counter>`, unique per event |
| `timestamp` | string (RFC 3339 UTC) | yes | Event creation time |
| `observed_at` | string (RFC 3339 UTC) | no | Upstream observation time, when distinct |
| `service` | object | yes | `{name, version, environment?}` |
| `trace_id` | string | no | W3C trace ID, when traced |
| `span_id` | string | no | Span ID, when traced |
| `request_id` | string | yes | `req-<pid>-<nanos>-<counter>`, shared by one request's events |
| `http` | object | yes | HTTP facts (see below) |
| `outcome` | string | yes | `success`, `client_error`, `server_error`, `aborted`, `panic` |
| `error_type` | string | no | Machine-readable failure class |
| `operation` | object | no | Tagged union; `type` selects the fields |
| `content_references` | array | no | Stored content references (omitted when empty) |
| `request_headers` | object | no | Header capture for the upstream request |
| `response_headers` | object | no | Header capture for the upstream response |

Optional fields are **omitted** when unset; they are not `null`. Consumers must treat a missing
field as absent.

## `service`

| Field | JSON type | Required | Description |
| --- | --- | --- | --- |
| `name` | string | yes | Service name, default `neuradex` |
| `version` | string | yes | Cargo package version |
| `environment` | string | no | Deployment environment, when configured |

## `http`

| Field | JSON type | Required | Description |
| --- | --- | --- | --- |
| `method` | string | yes | HTTP method |
| `route` | string | no | Matched route template; omitted for a fallback |
| `path` | string | yes | Request path (no query string) |
| `status` | number | no | Final HTTP status; omitted if no response was produced |
| `request_body_bytes` | number | no | Request body size, when known |
| `response_body_bytes` | number | no | Response body size, when known |
| `duration_ms` | number | yes | Request duration in whole milliseconds |

The path is the raw request path. A fallback request has `http.path` but no `http.route`.

## `operation`

`operation.type` is one of `search`, `openwebui_search`, `fetch`, `peek`, `infrastructure`.
The fields below are union fields: only the applicable ones are present.

### Search (`search`, `openwebui_search`)

| Field | JSON type | Required | Description |
| --- | --- | --- | --- |
| `type` | string | yes | `search` or `openwebui_search` |
| `query_length` | number | yes | Query length in Unicode scalar values |
| `query_terms` | number | yes | Whitespace-separated term count |
| `query_sha256` | string | yes | SHA-256 hex of the query |
| `limit` | number | no | `/v1/search` requested limit |
| `count` | number | no | `/v1/openwebui_search` requested count |
| `auth_mode` | string | no | `privacy_pass` or `session` |
| `queue_ms` | number | no | Time waiting for a search slot |
| `upstream_ms` | number | no | Time in the upstream search |
| `upstream_result_count` | number | no | Results returned by Kagi |
| `returned_result_count` | number | no | Results returned to the client |
| `zero_results` | boolean | yes | Whether the returned set was empty |
| `result_digest` | string | no | SHA-256 hex of the returned result URLs, newline-separated |
| `top_hosts` | array | no | `[{host, count}]`, at most 10, ordered by count then host |

The query text is never stored. `query_sha256` supports aggregate analysis without exposing the
query. `top_hosts` exposes only result hostnames, not full URLs.

### Fetch / peek (`fetch`, `peek`)

| Field | JSON type | Required | Description |
| --- | --- | --- | --- |
| `type` | string | yes | `fetch` or `peek` |
| `target_scheme` | string | no | `http` or `https` |
| `target_host` | string | no | Target host |
| `target_port` | number | no | Explicit target port |
| `target_path` | string | yes | Target path |
| `target_url_sha256` | string | yes | SHA-256 hex of the full target URL |
| `redirects` | number | yes | Redirects followed |
| `final_host` | string | no | Final response host |
| `upstream_status` | number | no | Final upstream status |
| `content_type` | string | no | Final response media type |
| `content_length` | number | no | Announced content length |
| `bytes_read` | number | no | Decompressed bytes read |
| `ttfb_ms` | number | no | Time to first byte |
| `total_ms` | number | no | Total fetch/peek time |
| `truncated` | boolean | yes | The returned body hit a size limit |
| `stopped_at_head` | boolean | yes | The read stopped at the end of the document head |

The full URL is hashed, not stored. Userinfo, query string, and fragment never reach the event;
only scheme, host, port, and path are visible plus the hash. A peek always has
`stopped_at_head=true` in practice; it is a field so an implementation change is observable.

### Infrastructure (`infrastructure`)

| Field | JSON type | Required | Description |
| --- | --- | --- | --- |
| `type` | string | yes | `infrastructure` |
| `kind` | string | yes | `health`, `openapi`, `docs`, `fallback`, or `other` |

## `content_references`

An array of stored content references. The current audit crate emits none until the content
plane is wired in. Each entry:

| Field | JSON type | Required | Description |
| --- | --- | --- | --- |
| `storage` | string | yes | `pack` or `object` |
| `key` | string | yes | Opaque storage key; for a pack, the pack object key |
| `blake3` | string | yes | BLAKE3-256 hex of the decoded content |
| `sha256` | string | yes | SHA-256 hex of the decoded content |
| `size` | number | yes | Content size in bytes |
| `content_type` | string | no | Media type, when known |

There is no frame offset/length in the event. The current service resolves a `pack` reference
by searching the pack footer for the BLAKE3 hash; an hourly request index is future work
(RFD 0002). An `object` reference is directly addressable by `key`.

Absence of `content_references` means no content was captured or the request does not produce
content (search, infrastructure). The schema has no `content_captured` or
`content_capture_error` field; a future content-plane integration may add one (see Future work).

## Header captures

`request_headers` and `response_headers` are `HeaderCapture` objects, not maps of header names
to strings. Values are bounded and policy-tagged. See
[RFD 0005](../rfd/0005-header-policy.md).

```json
{
  "headers": [
    {
      "name": "content-type",
      "action": "allow",
      "redacted": false,
      "value_bytes": 24,
      "values": [
        {
          "value": "text/html; charset=utf-8",
          "encoding": "utf8",
          "length": 24,
          "truncated": false
        }
      ],
      "omitted_values": 0
    }
  ],
  "omitted_headers": 0,
  "redacted_values": 0,
  "truncated_values": 0
}
```

| Field | Meaning |
| --- | --- |
| `headers[].name` | Lowercase header name |
| `headers[].action` | `allow`, `redact`, `sanitize`, or `hash` |
| `headers[].redacted` | Values withheld |
| `headers[].value_bytes` | Original total byte length of all values |
| `headers[].values[].value` | Captured value, or the HMAC hex for a hashed header |
| `headers[].values[].encoding` | `utf8` or `base64` |
| `headers[].values[].length` | Original value length in bytes |
| `headers[].values[].truncated` | Value was cut by a limit |
| `headers[].omitted_values` | Values omitted by the per-header cap/budget |
| `omitted_headers` | Header names omitted by the per-side cap/budget |
| `redacted_values` | Values withheld by policy |
| `truncated_values` | Values truncated by a limit/budget |

A redacted header keeps its name and `value_bytes` but has an empty `values` array. A hashed
header (IP/user/chat identifiers) has an HMAC-SHA256 hex value when the policy has a key; without
a key it is redacted. Unknown headers are classified as `redact`, not dropped.

## Enumerations

| Enum | Values |
| --- | --- |
| `outcome` | `success`, `client_error`, `server_error`, `aborted`, `panic` |
| `operation.type` | `search`, `openwebui_search`, `fetch`, `peek`, `infrastructure` |
| `operation.auth_mode` | `privacy_pass`, `session` |
| `operation.kind` (infrastructure) | `health`, `openapi`, `docs`, `fallback`, `other` |
| `content_references[].storage` | `pack`, `object` |
| `headers[].action` | `allow`, `redact`, `sanitize`, `hash` |
| `headers[].values[].encoding` | `utf8`, `base64` |

`error_type` is a free string in the crate. Recommended low-cardinality values:
`invalid_param`, `unsupported_media_type`, `not_found`, `method_not_allowed`, `unauthorized`,
`forbidden`, `upstream_timeout`, `upstream_error`, `dns_error`, `tls_error`, `connection_error`,
`connection_reset`, `body_error`, `decode_error`, `redirect_error`, `internal_error`.

## Example (fetch)

This is the exact serialization from the audit crate (timestamps and IDs shortened only where
noted). The `authorization` header is redacted; the content reference is a pack.

```json
{
  "event": "neuradex.request",
  "schema_version": "1.0.0",
  "event_id": "evt-1",
  "timestamp": "2026-11-10T12:34:56.789Z",
  "service": {"name": "neuradex", "version": "0.1.0"},
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
  "span_id": "00f067aa0ba902b7",
  "request_id": "req-1",
  "http": {
    "method": "GET",
    "route": "/v1/fetch",
    "path": "/v1/fetch",
    "status": 200,
    "duration_ms": 120
  },
  "outcome": "success",
  "operation": {
    "type": "fetch",
    "target_scheme": "https",
    "target_host": "example.com",
    "target_port": 8443,
    "target_path": "/a/path",
    "target_url_sha256": "7d5e1f180f774d3f6d57faa2ae34610fdbfb752d0c0e48d840e3f2d633a27f78",
    "redirects": 0,
    "final_host": "example.com",
    "upstream_status": 200,
    "content_type": "text/html",
    "content_length": 5123,
    "bytes_read": 5123,
    "ttfb_ms": 87,
    "total_ms": 120,
    "truncated": false,
    "stopped_at_head": false
  },
  "content_references": [
    {
      "storage": "pack",
      "key": "packs/fetch-response/2026-11-10T12/4242-0.pack",
      "blake3": "9f2c000000000000000000000000000000000000000000000000000000000000",
      "sha256": "3a7d000000000000000000000000000000000000000000000000000000000000",
      "size": 5123,
      "content_type": "text/html"
    }
  ],
  "request_headers": {
    "headers": [
      {
        "name": "accept",
        "action": "allow",
        "redacted": false,
        "value_bytes": 3,
        "values": [
          {"value": "*/*", "encoding": "utf8", "length": 3, "truncated": false}
        ],
        "omitted_values": 0
      },
      {
        "name": "authorization",
        "action": "redact",
        "redacted": true,
        "value_bytes": 13,
        "values": [],
        "omitted_values": 0
      }
    ],
    "omitted_headers": 0,
    "redacted_values": 1,
    "truncated_values": 0
  },
  "response_headers": {
    "headers": [
      {
        "name": "content-type",
        "action": "allow",
        "redacted": false,
        "value_bytes": 24,
        "values": [
          {"value": "text/html; charset=utf-8", "encoding": "utf8", "length": 24, "truncated": false}
        ],
        "omitted_values": 0
      }
    ],
    "omitted_headers": 0,
    "redacted_values": 0,
    "truncated_values": 0
  }
}
```

A search event has `operation.type = "search"` and the search fields (query length/terms/hash,
auth mode, queue/upstream ms, result counts, `zero_results`, `result_digest`, `top_hosts`); it
has no `content_references` and no `target_*` fields.

## Sink projections

### Quickwit

The index mapping in `hack/quickwit/neuradex-events.yaml` follows the nested shape:

- `service.name`, `service.version`, `service.environment`
- `http.method`, `http.route`, `http.path`, `http.status`, `http.duration_ms`, body byte fields
- `operation.type` and the union fields (`query_sha256`, `target_host`, …)
- `content_references` and the header captures as stored JSON

Quickwit's `json` field accepts an object, not a top-level array. The Vector `shape_quickwit`
transform wraps `content_references` and `operation.top_hosts` as `{"items": [...]}` before
ingest. This wrapping is a hot-index detail; the archive and ClickHouse keep the original
arrays. Query the wrapper with JSON accessors, or use ClickHouse for array analytics.

### ClickHouse

ClickHouse receives a flat projection produced by the Vector `flatten` transform. It keeps the
same names with prefixes: `service_name`, `http_method`, `http_route`, `http_status`,
`operation_type`, `query_sha256`, `target_host`, etc. `content_references` and the header
captures are JSON strings.

The following columns are **derived by Vector**, not fields of `AuditEvent`:

| Column | Derivation |
| --- | --- |
| `content_reference_count` | `length(content_references)` |
| `content_bytes` | Sum of `content_references[].size` |
| `content_storage` | First reference's `storage` |
| `content_key` | First reference's `key` |
| `content_blake3` | First reference's `blake3` |
| `content_sha256` | First reference's `sha256` |

They exist so capacity and integrity queries do not parse JSON on every scan. If an event has
multiple references, only the first is summarized in the scalar columns; the full array remains
in `content_references`.

## Future work (not in schema 1.0.0)

These were assumed by earlier drafts and are **not emitted** today. Do not add them to the
Quickwit mapping or ClickHouse DDL until the audit crate emits them:

- `auth_subject`, `auth_method`, `user_id` — named-access identity; currently only search
  `auth_mode` and header HMACs exist. See RFD 0009.
- `read_action`, `read_target`, `read_reason`, `read_bytes` — read audit. The current event is
  only `neuradex.request`; `audit.read`/`content.read` event names are proposed in RFD 0009.
- `content_captured`, `content_capture_error`, `content_kind`, `content_truncated` — the event
  carries `content_references` and fetch/peek `truncated`; a future capture-status field may be
  added.
- Top-level `content_hash_blake3`, `content_hash_sha256`, `content_bytes`, `content_pack_id`,
  `content_frame_offset`, `content_frame_length` — use `content_references`; a future hourly
  request index may add the frame offset/length.
- `query_text`, `target_url`, `result_count`, `redirects_followed` — the actual names are
  `query_sha256`/length/terms, `target_*`/`target_url_sha256`, `returned_result_count`, and
  `operation.redirects`.

A future schema change should follow additive evolution: optional fields within `1.0.x`, a new
`schema_version` for a semantic change, and matching Quickwit/ClickHouse updates.
