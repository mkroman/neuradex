# Event fields

This is the complete `schema_version: 1` event schema. The same field names are used in the
event JSON, the Quickwit mapping (`hack/quickwit/neuradex-events.yaml`), and the ClickHouse table
(`hack/clickhouse/init.sql`). Do not rename per sink.

**Status:** specified. The Rust types are not implemented beyond the crate scaffold.

## Envelope and correlation

| Field | Event JSON | Quickwit | ClickHouse | Required | Description |
| --- | --- | --- | --- | --- | --- |
| `schema_version` | number | `u64` | `UInt16` | yes | Schema version; currently `1` |
| `event` | string | `text` raw, fast | `LowCardinality(String)` | yes | Event type (see below) |
| `event_id` | string | `text` raw | `String` | yes | Unique event ID; stable across retries |
| `timestamp` | RFC 3339 string | `datetime` | `DateTime64(3,'UTC')` | yes | Completion time, UTC, millisecond precision |
| `request_id` | string | `text` raw | `String` | request events | Opaque request ID; primary correlation key |
| `trace_id` | string (32 hex) | `text` raw | `String` | no | W3C trace ID; empty if unavailable |
| `span_id` | string (16 hex) | `text` raw | `String` | no | Span ID; empty if unavailable |

`event_id` is unique per event. `request_id` is unique per request but appears in multiple
events (`fetch`, `content.pack_uploaded`, `audit.read`). Do not use `request_id` as the sole
unique key for event rows.

## Request metadata

| Field | Event JSON | Quickwit | ClickHouse | Required | Description |
| --- | --- | --- | --- | --- | --- |
| `route` | string | `text` raw, fast | `LowCardinality(String)` | no | Matched route template (`/v1/fetch`); empty for 404/405 and non-HTTP events |
| `method` | string | `text` raw, fast | `LowCardinality(String)` | no | HTTP method; empty for non-HTTP events |
| `status` | number | `u64`, fast | `UInt16` | no | Final HTTP status; `0` for non-HTTP events |
| `duration_ms` | number | `u64`, fast | `UInt32` | no | Request/operation duration in ms |
| `outcome` | string | `text` raw, fast | `LowCardinality(String)` | yes | Low-cardinality result (see below) |
| `error_type` | string | `text` raw, fast | `LowCardinality(String)` | no | Low-cardinality failure class; empty on success |
| `service_version` | string | `text` raw | `String` | no | Service package version |

`error_type` values are stable identifiers, not messages: `invalid_param`,
`unsupported_media_type`, `not_found`, `method_not_allowed`, `unauthorized`, `forbidden`,
`upstream_timeout`, `upstream_error`, `dns_error`, `tls_error`, `connection_error`,
`connection_reset`, `body_error`, `decode_error`, `redirect_error`, `content_spool_full`,
`content_pack_failed`, `content_index_failed`, `serialization_failed`, `internal_error`.

## Principal

| Field | Event JSON | Quickwit | ClickHouse | Required | Description |
| --- | --- | --- | --- | --- | --- |
| `auth_subject` | string | `text` raw, fast | `String` | no | Verified principal, e.g. `user:alice@example.com` or `service:agent` |
| `auth_method` | string | `text` raw, fast | `LowCardinality(String)` | no | `mtls`, `oidc`, `none`, `local` |
| `user_id` | string | `text` raw, fast | `String` | no | Stable user identifier, if known |

Never store the raw token, certificate, or cookie. `auth_subject` is the verified `sub` from the
trusted ingress.

## Search

| Field | Event JSON | Quickwit | ClickHouse | Required | Description |
| --- | --- | --- | --- | --- | --- |
| `query_text` | string | `text` default | `String` | no | Search query; empty unless policy enables capture |
| `query_hash` | string (64 hex) | `text` raw | `String` | search events | SHA-256 of the exact query string |
| `result_count` | number | `u64`, fast | `UInt32` | no | Results returned after `limit`/`count` truncation |

`query_text` is restricted. The default is off; aggregate with `query_hash`. A search event with
`query_text` empty is normal.

## Target

| Field | Event JSON | Quickwit | ClickHouse | Required | Description |
| --- | --- | --- | --- | --- | --- |
| `target_url` | string | `text` default | `String` | no | Requested URL (fetch/peek) or first result URL (search), sanitized |
| `upstream_host` | string | `text` raw, fast | `String` | no | Host of the final upstream response |
| `redirects_followed` | number | `u64` | `UInt16` | no | Number of redirect hops followed |

Sanitization removes userinfo, fragment, and query **values**; query keys remain (replaced with
`REDACTED`), matching the telemetry `url.full` policy. Treat URLs as sensitive; path segments can
contain tokens.

## Content

| Field | Event JSON | Quickwit | ClickHouse | Required | Description |
| --- | --- | --- | --- | --- | --- |
| `content_captured` | boolean | `bool`, fast | `Bool` | yes | Whether canonical content bytes were captured |
| `content_kind` | string | `text` raw | `LowCardinality(String)` | no | `fetch_body`, `peek_head`, or empty |
| `content_hash_blake3` | string (64 hex) | `text` raw | `String` | when captured | BLAKE3-256 content identity |
| `content_hash_sha256` | string (64 hex) | `text` raw | `String` | when captured | SHA-256 audit hash |
| `content_bytes` | number | `u64`, fast | `UInt64` | when captured | Canonical captured bytes (not compressed) |
| `content_type` | string | `text` raw | `LowCardinality(String)` | when captured | Media type without parameters |
| `content_truncated` | boolean | `bool` | `Bool` | yes | Captured prefix hit the endpoint cap |
| `content_pack_id` | string | `text` raw | `String` | when indexed | Pack identifier |
| `content_frame_offset` | number | `u64` | `UInt64` | when indexed | Absolute frame header offset in the pack |
| `content_frame_length` | number | `u64` | `UInt32` | when indexed | Header + compressed payload length |
| `content_capture_error` | string | `text` raw | `LowCardinality(String)` | no | Why content is absent/partial |

`content_captured=false` is a normal state. The marker is one of `spool_full`,
`client_disconnect`, `upstream_error`, `pack_upload_failed`, `index_write_failed`,
`hash_mismatch`, or `internal_error`. For events that never capture content (`search`,
`audit.read`), `content_captured=false` and `content_capture_error` is empty.

`content_hash_blake3` and `content_hash_sha256` identify the **captured** bytes. If
`content_truncated=true`, they identify the prefix, not the full upstream body. `content_bytes`
is the prefix length.

## Headers

| Field | Event JSON | Quickwit | ClickHouse | Required | Description |
| --- | --- | --- | --- | --- | --- |
| `request_headers` | object | `json`, indexed false, stored true | `String` (JSON) | yes | Headers neuradex sent upstream |
| `response_headers` | object | `json`, indexed false, stored true | `String` (JSON) | yes | Headers returned by upstream |
| `request_headers_truncated` | boolean | `bool` | `Bool` | yes | A capture limit was hit |
| `response_headers_truncated` | boolean | `bool` | `Bool` | yes | A capture limit was hit |

The event carries objects so Quickwit's `json` field can parse them. ClickHouse stores the
compact JSON text in a `String`; Vector's `encode_clickhouse` transform does the encoding.
Never send a JSON string to Quickwit's `json` field — the document is rejected.

Shape:

```json
{
  "accept": ["*/*"],
  "authorization": {
    "redacted": true,
    "present": true,
    "length": 42,
    "hmac_sha256": "base64url…"
  },
  "_truncated": false
}
```

Regular values are arrays of strings, lowercase names, in received order. Sensitive values are
objects with `redacted`, `present`, `length`, and `hmac_sha256`. See
[RFD 0005](../rfd/0005-header-policy.md) for the allow-list.

## Read audit

| Field | Event JSON | Quickwit | ClickHouse | Required | Description |
| --- | --- | --- | --- | --- | --- |
| `read_action` | string | `text` raw, fast | `LowCardinality(String)` | read events | `list_events`, `get_event`, `get_content`, `export_events`, `verify_pack` |
| `read_target` | string | `text` raw | `String` | read events | Request ID, pack ID, query hash, or export range |
| `read_reason` | string | `text` raw | `String` | content/exports | Ticket or incident ID |
| `read_bytes` | number | `u64` | `UInt64` | read events | Bytes returned |

Read audit events are `event=audit.read` or `event=content.read`. They never contain the body or
the query text. See [RFD 0009](../rfd/0009-access-and-read-audit.md).

## Enumerations

### `event`

`fetch`, `peek`, `search`, `openwebui_search`, `audit.read`, `content.read`,
`content.pack_sealed`, `content.pack_uploaded`, `content.pack_failed`.

### `outcome`

| Value | Meaning |
| --- | --- |
| `success` | 2xx/3xx or a completed non-HTTP operation |
| `client_error` | 4xx |
| `upstream_error` | 5xx caused by upstream/Kagi |
| `timeout` | Deadline exceeded, including search queue timeout |
| `internal_error` | 5xx caused by neuradex |
| `canceled` | Client disconnected before completion |
| `partial` | Response completed but content capture was incomplete |
| `failed` | Non-HTTP operational event failed |

### `auth_method`

`mtls`, `oidc`, `none`, `local`.

### `content_kind`

`fetch_body`, `peek_head`, empty for events that do not capture content.

### `read_action`

`list_events`, `get_event`, `get_content`, `export_events`, `verify_pack`.

## Example

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
  "content_hash_blake3": "<64 lowercase hex>",
  "content_hash_sha256": "<64 lowercase hex>",
  "content_bytes": 5123,
  "content_type": "text/html",
  "content_truncated": false,
  "content_pack_id": "019a7f3c-7b10-7a22-8c33-1d2e3f405162",
  "content_frame_offset": 4096,
  "content_frame_length": 1234,
  "content_capture_error": "",
  "request_headers": {"accept": ["*/*"], "user-agent": ["neuradex/0.1"]},
  "response_headers": {"content-type": ["text/html; charset=utf-8"]},
  "request_headers_truncated": false,
  "response_headers_truncated": false,
  "read_action": "",
  "read_target": "",
  "read_reason": "",
  "read_bytes": 0
}
```

## Validation rules

1. `schema_version` is `1`.
2. `event_id` is non-empty for every event.
3. `request_id` is non-empty for request and read events.
4. `timestamp` is RFC 3339 UTC with milliseconds.
5. `status` is 0 or a valid HTTP status; `duration_ms` is non-negative.
6. `outcome` is one of the values above.
7. `error_type` is empty for `success`; non-empty for failures where a class is known.
8. `content_captured=true` implies both hashes, `content_bytes`, `content_kind`, and a
   non-empty `content_type`.
9. `content_truncated=true` implies `content_captured=true`.
10. `query_hash` is non-empty for search events; `query_text` may be empty.
11. Header fields are objects (not strings) before sink encoding.
12. No body, result snippet, token, or cookie appears in any field.

## Compatibility

- Additive optional fields are allowed within v1. Add them to this page, the Quickwit mapping
  (if searchable), and the ClickHouse column (if queried).
- Removing/renaming/changing a type requires `schema_version: 2` and a dual-write window.
- Quickwit uses `mode: lenient`: unknown fields are ignored, not rejected. A new field is not
  searchable until the mapping is updated and the index is recreated/reindexed.
- ClickHouse ignores missing keys via column defaults; unknown keys are skipped by Vector's
  `skip_unknown_fields` only when enabled. Add the column before enabling the field.
