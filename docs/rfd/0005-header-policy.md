---
rfd: 5
title: Header policy
authors:
  - neuradex maintainers
state: published
discussion: TBD
labels:
  - audit
  - security
  - privacy
created: 2026-11-10
updated: 2026-11-10
---

# RFD 5: Header policy

## Summary

Capture upstream **request and response headers by allow-list**, never by denylist. Lowercase
names, preserve repeated values as arrays, sanitize URL-valued headers, and cap the serialized
size. Sensitive headers (`authorization`, `cookie`, `set-cookie`, API keys) are **never stored**;
record presence, length, and an HMAC-SHA256 under a dedicated audit key. Unknown headers are
dropped. Inbound client headers are not copied into the event; identity is recorded in the
`auth_*` fields.

## Context and scope

Headers are useful for debugging: content type, encoding, cache behaviour, redirect target, and
server/CDN identity explain most fetch anomalies. They are also a common leak: bearer tokens,
session cookies, API keys, CSRF tokens, and client certificates live in headers. Real data means
we must assume every captured value will be read by a human and possibly copied into a ticket.

This policy covers the headers neuradex sends to upstreams and the headers upstreams return.
It does not cover inbound client headers to neuradex, which are terminated at the trusted
ingress; only the authenticated principal is recorded.

Out of scope: request/response bodies (content plane), trailers, HTTP/2 pseudo-headers, and
`Set-Cookie` semantics. Cookies are treated as opaque secrets.

## Goals and non-goals

| Goal | How we know it is met |
| --- | --- |
| No secret header value reaches storage | A redaction test with a sentinel token finds it nowhere in the event, Quickwit, ClickHouse, or logs |
| Useful debugging headers survive | A fetch of a normal page has content type, encoding, cache, and server fields |
| Bounded event size | Serialized headers ≤ 32 KiB per direction; values ≤ 8 KiB; names ≤ 64 |
| Deterministic shape | Header JSON keys sorted; repeated values ordered as received; lowercase names |
| Sensitive presence is provable | HMAC, length, and presence are recorded without the value |
| Policy is testable | A table-driven test covers every allowed, dropped, and HMACed header |

Non-goals:

- Reconstructing the exact wire bytes of headers. This is an audit summary, not a packet capture.
- Capturing inbound client headers for every request. Auth identity is enough.
- Storing cookies for replay. Cookies are secrets and are never replayed.
- Header-based analytics beyond the allow-listed fields.

## Options

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| A. Capture all headers, redact a denylist | Maximum debugging data; simple to implement | One missed header leaks a token; header names are unbounded; PII in custom headers; large events | Rejected |
| B. Capture no headers | Zero leak risk | Cannot debug content type, encoding, cache, or redirects; loses the most useful signal | Rejected |
| C. Allow-list + HMAC for sensitive names | Predictable surface; unknown headers dropped; presence still provable for auth/cookies | Some useful custom headers are missed; HMAC key management | **Chosen** |
| D. Allow-list, drop sensitive entirely | Simplest; no key management | Cannot prove a request was authenticated or a cookie was present | Rejected |
| E. Capture hashes of every header | Compact; comparability | Low-entropy values can be brute-forced; loses readability; hashes are not values | Rejected |

Option C keeps the useful, known headers and makes the sensitive ones auditable without storing
them. The allow-list is deliberately small; adding a header is a reviewed change.

## Determination

Adopt **Option C**.

- `request_headers` and `response_headers` are JSON objects in the event, serialized as compact
  JSON by the sinks. Quickwit receives an object for its `json` field; ClickHouse receives a
  JSON-encoded string for its `String` column. The field names and shapes are identical.
- Names are lowercase ASCII. Values are arrays of strings, preserving order and repeats.
- Every captured value must be valid UTF-8; a non-UTF-8 value is recorded as
  `"<non-utf8>"` with its byte length in the sensitive/present object.
- Header names not on the allow-list are dropped silently (counted in a metric, not stored).
- Sensitive allow-listed names are represented as an object:
  ```json
  {"redacted": true, "present": true, "length": 42, "hmac_sha256": "base64url…"}
  ```
  If the header is absent, it is omitted entirely.
- URL-valued headers (`location`, `referer`, `content-location`) are sanitized like
  `target_url`: scheme, host, path, and query keys are kept; userinfo, fragment, and query
  values are removed. If the value is not a valid absolute URL, store only `<invalid-url>`.
- Limits: ≤ 64 names, ≤ 8 KiB per value, ≤ 32 KiB serialized per direction. When a limit is
  crossed, stop adding headers, add `"_truncated": true`, and set the corresponding
  `*_headers_truncated` event field.
- The event fields are the **upstream** request headers (the defaults neuradex sent) and the
  **upstream** response headers, not the client's request headers to neuradex.
- Inbound auth material is represented by `auth_subject`, `auth_method`, and `user_id`. It is
  populated by the trusted ingress, never by a header the client can set directly.

## Design

### Captured request headers (neuradex → upstream)

| Header | Why | Treatment |
| --- | --- | --- |
| `accept` | Content negotiation | value array |
| `accept-encoding` | Explains compressed vs decoded size | value array |
| `accept-language` | Content negotiation | value array |
| `cache-control` | Cache behaviour | value array |
| `content-type` | Request body type (future POST support) | value array |
| `if-none-match` | Conditional request | value array; may contain an ETag, not a secret |
| `if-modified-since` | Conditional request | value array |
| `pragma` | Legacy cache directive | value array |
| `range` | Partial content | value array |
| `referer` | Traffic source | sanitized URL |
| `user-agent` | Identifies neuradex's client | value array |

### Captured response headers (upstream → neuradex)

| Header | Why | Treatment |
| --- | --- | --- |
| `age` | CDN cache age | value array |
| `cache-control` | Cache behaviour | value array |
| `content-encoding` | Explains compressed bytes | value array |
| `content-language` | Content metadata | value array |
| `content-length` | Announced size (may be compressed) | value array |
| `content-location` | Alternate location | sanitized URL |
| `content-type` | Media type | value array |
| `date` | Origin response time | value array |
| `etag` | Cache validator | value array |
| `expires` | Cache expiry | value array |
| `last-modified` | Cache validator | value array |
| `location` | Redirect target | sanitized URL |
| `server` | Origin/CDN identity | value array |
| `strict-transport-security` | TLS policy | value array |
| `vary` | Cache key dimensions | value array |
| `via` | Proxy chain | value array |
| `x-cache` | CDN cache result | value array |
| `x-cache-status` | CDN cache result | value array |

### Sensitive headers (HMAC + presence, never value)

| Header | Reason | HMAC |
| --- | --- | --- |
| `authorization` | Bearer/basic credentials | yes |
| `proxy-authorization` | Proxy credentials | yes |
| `cookie` | Session/authentication cookies | yes |
| `set-cookie` | Third-party session cookies | yes |
| `x-api-key` | API key | yes |
| `x-auth-token` | Auth token | yes |
| `x-csrf-token` | CSRF token | yes |
| `x-xsrf-token` | CSRF token | yes |
| `x-amz-security-token` | Cloud credential | yes |
| `www-authenticate` | May reveal realm/challenge details | yes |
| `proxy-authenticate` | May reveal proxy details | yes |
| `x-forwarded-client-cert` | Client certificate chain | yes |

The HMAC input and key rules are in [RFD 0003](0003-hashing.md). HMAC only proves presence and
equality; it does not reveal the value. Do not store a cookie name or domain: those can also be
sensitive. The `length` is the byte length of the value as received, capped at 1 MiB; larger
values are recorded as `length: -1`.

### Dropped headers

Everything not in the tables above is dropped, including but not limited to:

`x-real-ip`, `x-forwarded-for`, `forwarded`, `x-forwarded-proto`, `x-request-id`,
`traceparent`, `tracestate`, `baggage`, custom `x-*` headers, `link`, `refresh`, `report-to`,
`nel`, `server-timing`, `alt-svc`, and any `access-control-*` header.

Some of these are useful operationally, but they are either covered by OpenTelemetry, are
high-cardinality, can contain internal topology, or are not needed to explain a fetch. Add one
only with a test and a privacy review.

### JSON shape

```json
{
  "accept": ["text/html,application/xhtml+xml"],
  "accept-encoding": ["gzip, deflate, br, zstd"],
  "user-agent": ["neuradex/0.1"],
  "authorization": {
    "redacted": true,
    "present": true,
    "length": 42,
    "hmac_sha256": "5m9…"
  },
  "_truncated": false
}
```

- Object keys are sorted for deterministic output.
- A repeated header is an array in received order. A single value is still a one-element array
  for regular headers.
- A sensitive header is an object, not an array, because there is exactly one logical value
  (repeats are joined with `, ` before HMAC and length).
- `_truncated` is present only when a limit was hit; the event field
  `request_headers_truncated`/`response_headers_truncated` mirrors it for typed queries.

### Limits and failure

| Limit | Value | On overflow |
| --- | --- | --- |
| Header names per direction | 64 | Stop; set truncated |
| Value bytes | 8 KiB | Truncate to 8 KiB; set truncated; HMAC/length over the full value if sensitive |
| Serialized bytes per direction | 32 KiB | Stop; set truncated |
| Sensitive value length | 1 MiB | Record `length: -1`; HMAC only the first 1 MiB with a domain-separated prefix |
| Total event size | 64 KiB | Drop response headers first, then request headers; set truncated |

Dropping headers never fails the request. A serialization error is a bug; the event is still
emitted with empty header strings and an `error_type=header_serialization_failed` field.

### Header capture and response timing

The response headers are available before the body is read, so they can be captured even if the
body read fails or is truncated. The request headers are the configured defaults, known at
startup. This means a failed fetch can still have useful header metadata.

Inbound client headers are not part of this event. If a future endpoint needs them, add a
separate `client_headers` field with its own allow-list and review; do not overload
`request_headers`.

## Economic, capacity, and performance considerations

Header capture is small: the allow-listed set is typically < 1 KiB. The 32 KiB cap bounds the
worst case. At 3.36 million events/day, an average 1 KiB of headers adds ~3.4 GB/day raw; zstd
compression reduces this substantially. The event size model in RFD 0001 includes headers.

HMAC-SHA256 over a handful of headers is negligible compared with body hashing. The key lookup
is cached at startup.

## Security considerations

- The allow-list is a security boundary. Adding a header is a code review that must consider
  whether the value can contain a secret, PII, or internal topology.
- Sensitive headers are never stored in plaintext, even in debug logs. Redaction must happen
  before any tracing field is recorded. A sentinel-token test should assert absence in logs,
  events, Quickwit, and ClickHouse.
- HMAC is not encryption; it proves presence/equality. A low-entropy value could still be
  confirmed by an attacker who can guess and HMAC it. Keep the HMAC key restricted to the audit
  service and rotate it.
- `referer` and `location` can carry tokens in the path, not just the query. Sanitization removes
  userinfo, fragment, and query values, but not path segments. Treat captured URLs as sensitive;
  do not paste them into public tickets. If this is unacceptable, drop `referer` entirely.
- Header values may contain non-UTF-8 bytes. Do not lossy-decode them into something that looks
  valid; use the explicit `<non-utf8>` marker.
- Inbound client headers are terminated at the ingress. Do not trust `X-Forwarded-User` or
  similar unless the ingress strips and re-adds it; see RFD 0009.
- The header policy applies to event archives and both derived stores. A header dropped before
  serialization cannot reappear in Quickwit or ClickHouse.

## Operational considerations

- A spike in `headers_truncated` means a site is sending huge headers; it is not an incident by
  itself, but it can indicate an attack or a misconfigured upstream.
- A spike in `header_serialization_failed` is a bug; page the service owner.
- When adding a header, update the allow-list table, the tests, and
  [reference/event-fields.md](../reference/event-fields.md) if the shape changes.
- When rotating the HMAC key, keep the old key available for the retention window so old HMACs
  can be explained. Store a key ID if multiple versions are active.
- Do not add `authorization` to a "temporarily capture" debug flag. Use the HMAC representation.

## Open questions

| Question | Why it is open | Trigger / owner |
| --- | --- | --- |
| Should inbound client headers be captured at all? | Useful for abuse/debugging; high leak risk and the ingress already authenticates | Security review; currently no |
| Is `referer` path sanitization sufficient? | Paths can contain session IDs | Privacy review; drop it if not |
| Should `link` and `server-timing` be captured? | Useful for performance debugging; high cardinality | Add with a test if needed |
| Do we need a key ID in each HMAC object? | Needed for rotation; adds a field and a lookup | Add when the first rotation happens |
| Should header capture be configurable per environment? | Staging may want more; production should not | Keep one policy; use debug logs for one-off cases |

## References

- [RFD 0001: Audit event model](0001-audit-event-model.md)
- [RFD 0003: Hashing](0003-hashing.md)
- [RFD 0009: Access and read audit](0009-access-and-read-audit.md)
- [Event fields](../reference/event-fields.md)
- RFC 9110, HTTP Semantics: <https://www.rfc-editor.org/rfc/rfc9110>
- Code: `crates/neuradex-audit` (header policy and HMAC)
