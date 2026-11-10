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

> **Status:** the policy is implemented in `crates/neuradex-audit/src/header_policy.rs` (PR #67).
> The service does not call it yet.

## Summary

Capture headers through a bounded, allow-list policy implemented in
`crates/neuradex-audit/src/header_policy.rs`. Safe headers keep their values; credentials and
personal data are redacted; IP/user/chat identifiers are HMAC-SHA256 hashed when a key is
configured; URL-valued headers lose their query string and fragment; unknown headers keep only
their name and original byte length. The result is a `HeaderCapture` object, not a name→value
map.

## Context and scope

Headers are the most dangerous part of an audit event: they routinely carry bearer tokens,
session cookies, API keys, user identifiers, and IP addresses. A single missed header can leak a
credential into Quickwit, ClickHouse, and the B2 archive for a year. At the same time, content
type, encoding, cache behaviour, and redirect location are essential for debugging fetch
anomalies.

This policy covers the `http::HeaderMap` values passed to `HeaderPolicy::capture`, which the
service will use for the upstream request and response. It does not define how the service
obtains inbound client identity; that is an ingress concern (RFD 0009).

Out of scope: bodies, trailers, HTTP/2 pseudo-headers, cookie semantics, and read-audit fields.

## Goals and non-goals

| Goal | How we know it is met |
| --- | --- |
| No secret value reaches storage | A sentinel-token test finds it nowhere in the serialized event, logs, Quickwit, or ClickHouse |
| Useful debugging headers survive | Content type, encoding, cache, server, and location are captured |
| Unknown headers do not leak values | They are redacted but retain a name and byte length |
| Bounded output | 64 headers, 8 KiB per side, 512 bytes per value, 16 values per header |
| Deterministic shape | Header names are lowercase and sorted; values preserve order |
| Sensitive presence is still provable | Redacted headers keep `value_bytes`; hashed headers keep an HMAC |
| Policy is testable | Table tests cover allow/redact/sanitize/hash, limits, non-UTF-8, and shape |

Non-goals:

- Reconstructing exact wire bytes. This is an audit summary.
- Capturing cookies for replay.
- Hashing every header. Only a fixed identifier/IP list is HMACed; credentials are redacted.
- Making unknown headers searchable. Only the name and byte length are kept.

## Options

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| A. Capture all headers, deny-list secrets | Maximum debugging data | One missed header leaks a token; unknown headers unbounded; PII in custom headers | Rejected |
| B. Capture no headers | Zero leak risk | Cannot debug content type, encoding, cache, redirects | Rejected |
| C. Allow-list + redact/hash/sanitize | Predictable surface; unknown values withheld; identifiers still comparable | Some custom headers are lost; HMAC key management | **Chosen** |
| D. Allow-list, drop everything sensitive | Simplest; no key management | Cannot correlate IP/user/chat identifiers; loses value length for redacted headers | Rejected |
| E. Hash every header | Compact; comparable | Low-entropy values can be brute-forced; loses readability; hashes are not values | Rejected |

## Determination

Adopt **Option C**, as implemented by `HeaderPolicy`:

- **Allow**: a fixed safe list keeps values.
- **Redact**: credentials and personal data keep the header name and `value_bytes`, but
  `values` is empty and `redacted=true`.
- **Hash**: a fixed IP/user/chat list is HMAC-SHA256 hashed when `with_hmac_key` is configured.
  Without a key, these are redacted.
- **Sanitize**: `location` and `referer` keep the value up to the first `?` or `#`.
- **Unknown**: classified as `Redact`; the name and original byte length remain.
- **Limits**: 64 headers, 8 KiB per side (including names), 512 bytes per value, 16 values per
  header. Values are truncated on a UTF-8 boundary; counts are recorded.
- **Encoding**: valid UTF-8 is stored as-is after control characters are stripped; non-UTF-8 is
  base64-encoded with `encoding=base64`.
- **HMAC**: lowercase hex, 64 characters. The key is a dedicated audit key, not a B2 key, Kagi
  token, or TLS key.

The policy does not decide *which* headers the service passes in. The service should pass the
upstream request headers and upstream response headers; inbound client headers are not part of
schema 1.0.0.

## Design

### Safe headers (values captured)

```text
accept, accept-charset, accept-encoding, accept-language, accept-ranges,
access-control-allow-credentials, access-control-allow-headers,
access-control-allow-methods, access-control-allow-origin,
access-control-expose-headers, access-control-max-age, age, allow, alt-svc,
cache-control, connection, content-encoding, content-language,
content-length, content-range, content-type, date, etag, expect, expires,
host, if-match, if-modified-since, if-none-match, if-range,
if-unmodified-since, last-modified, pragma, range, retry-after, server,
strict-transport-security, te, trailer, transfer-encoding, upgrade,
user-agent, vary, via, warning, x-content-type-options, x-frame-options
```

These are readability/debugging headers. `content-length` may describe compressed bytes; it is a
hint, not the captured size. `user-agent` is neuradex's outbound client string when capturing
request headers.

### Sensitive headers (always redacted)

```text
authorization, cookie, proxy-authenticate, proxy-authorization, set-cookie,
www-authenticate, x-api-key, x-auth-token, x-openwebui-user-email,
x-openwebui-user-name
```

The header name and `value_bytes` remain; values never enter the event. `authorization` and
cookies are not HMACed because the values are high-risk and the HMAC does not help debugging.

### Hashed headers (HMAC when keyed)

```text
cf-connecting-ip, forwarded, true-client-ip, x-client-ip, x-cluster-client-ip,
x-forwarded-for, x-openwebui-chat-id, x-openwebui-user-id, x-real-ip
```

With an HMAC key, these become lowercase hex HMAC-SHA256 values. The original value length is
kept in `values[].length`; the HMAC itself is 64 hex chars. Without a key, they are redacted.
HMAC allows correlation (same IP/user → same digest) without storing the identifier.

### Sanitized headers

`location` and `referer` keep everything before the first `?` or `#`. A relative `location`
(`/next?continue=secret`) becomes `/next`. A non-UTF-8 URL value is withheld rather than
base64-encoded, because base64 would preserve the secret.

### Shape

```json
{
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
}
```

`headers` is sorted by name (the implementation groups into a `BTreeMap`). `values` preserves
the order of repeated values. `omitted_values` counts values dropped by the per-header cap or
the side byte budget. `redacted_values` and `truncated_values` summarize the capture.

### Limits and failure

| Limit | Value | On overflow |
| --- | --- | --- |
| Headers per side | 64 | Omit the rest; increment `omitted_headers` |
| Total bytes per side | 8 KiB including names | Omit/truncate; increment counts |
| Value bytes | 512 | Truncate on a UTF-8 boundary; `truncated=true` |
| Values per header | 16 | Omit the rest; `omitted_values` |
| HMAC key | optional | Missing key → hashed headers are redacted |

A zero limit disables that output (e.g. `with_limits(0, …)` captures no headers). Capture never
fails the request; it returns a bounded object.

## Economic, capacity, and performance considerations

A typical allow-listed capture is well under 1 KiB. The hard cap is 8 KiB per side. At 3.36M
events/day, an average 1 KiB of headers is ~3.4 GB/day raw before compression; the cap bounds the
worst case. HMAC over a handful of headers is negligible compared with body hashing. The policy
is CPU-light and allocation-bounded.

## Security considerations

- The allow-list is the security boundary. Adding a header requires a review of whether its
  value can contain a secret, PII, or internal topology.
- Unknown headers are redacted by default; this is the safe failure mode. Do not change
  `classify` to allow unknown names.
- Redaction happens before serialization, so a redacted value cannot reappear in Quickwit,
  ClickHouse, or the archive.
- HMAC is not encryption; a low-entropy identifier can be confirmed by guessing. Keep the key
  restricted and rotate it on a documented schedule. A key change changes every digest, so
  record a key ID if rotation is implemented.
- The HMAC key must not be the B2 key, Kagi token, TLS key, or ingress signing key.
- `referer` and `location` can carry tokens in the path, not just the query. Sanitization removes
  query/fragment only. Treat captured URLs as sensitive; if path tokens are a concern, drop
  `referer` from the safe list.
- Header names themselves can be sensitive (e.g. a custom auth scheme). Unknown names are kept;
  review before adding a new `x-*` header to the safe list.
- Do not log the `HeaderPolicy` key. Its `Debug` implementation prints only whether a key is
  configured.

## Operational considerations

- A spike in `redacted_values` is normal (auth/cookies). A spike in `omitted_headers` means a
  site sends many headers or a large header; it is not an incident by itself.
- A spike in `truncated_values` can indicate an oversized cookie or user agent; investigate if it
  correlates with a fetch failure.
- When adding a header, update the allow-list, the tests, and the event-fields reference.
- When rotating the HMAC key, keep old key versions for the retention window so old digests can
  be explained. Store only the key ID with the event if rotation is implemented.
- Never add a "debug capture authorization" flag. Use the redacted representation.

## Open questions

| Question | Why it is open | Trigger / owner |
| --- | --- | --- |
| Do we need inbound client headers? | Current schema captures upstream request/response only; identity is future work | RFD 0009 / ingress review |
| Is path-level referer sanitization enough? | Paths can contain session IDs; query/fragment removal is not sufficient | Privacy review; drop `referer` if not |
| Should `authorization` be HMACed instead of redacted? | HMAC could prove token reuse; high risk and little debugging value | Security review |
| Do we add a key ID to hashed values? | Needed for rotation; changes the shape | Add with the first rotation |
| Should unknown headers be dropped instead of redacted? | Redaction preserves auditability of names; dropping reduces size | Keep redacted; revisit if event size is a problem |

## References

- [RFD 0001: Audit event model](0001-audit-event-model.md)
- [RFD 0003: Hashing](0003-hashing.md)
- [RFD 0009: Access and read audit](0009-access-and-read-audit.md)
- [Event fields](../reference/event-fields.md)
- Code: `crates/neuradex-audit/src/header_policy.rs`
- RFC 9110, HTTP Semantics
