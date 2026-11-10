---
rfd: 9
title: Access and read audit
authors:
  - neuradex maintainers
state: published
discussion: TBD
labels:
  - security
  - audit
  - operations
created: 2026-11-10
updated: 2026-11-10
---

# RFD 9: Access and read audit

## Summary

Every audit or content read is authenticated, authorized, and itself audited. Named users and
services get one of five roles: `service`, `audit.reader`, `content.reader`, `analyst`, or
`admin`. The trusted ingress establishes identity (mTLS or OIDC) and passes a signed principal;
neuradex never trusts a client-supplied identity header. Direct B2 access is restricted to the
service and break-glass admins; analysts use ClickHouse read-only; content reads go through an
audited read API. Read audit events are immutable and retained at least as long as the data they
describe.

## Context and scope

> **Schema status:** the `AuditEvent` 1.0.0 does not emit `auth_subject`, `auth_method`,
> `user_id`, `read_action`, `read_target`, `read_reason`, or `read_bytes`. This RFD specifies the
> target access model and the read-audit fields to add; they are future work, not current
> columns.

The service handles real data for named users. A body may contain personal data, a query may
reveal intent, and a header HMAC may confirm a credential. An audit system that anyone can read
without a record is not an audit system; it is a surveillance dataset with no accountability.

This RFD defines:

- who can read events, content, and aggregates;
- where authentication and authorization are enforced;
- what a read audit event contains;
- how break-glass access works;
- what is explicitly not supported (per-request deletion, anonymous access).

Out of scope: the identity provider configuration, the production ingress vendor, and the
production secret manager. Those are deployment details, but their contracts are fixed here.

## Goals and non-goals

| Goal | How we know it is met |
| --- | --- |
| No anonymous read path | Every read endpoint/credential requires a principal |
| Least privilege | A role can read only what it needs; writer cannot read content |
| Reads are attributable | Every event/content read has a principal, target, reason, and byte count |
| Identity is not spoofable | The service trusts only the ingress-authenticated principal |
| Break-glass is exceptional | Two-person approval, time-bound credential, ticket, notification |
| Local dev is clearly insecure | Compose has no auth and is documented as local-only |

Non-goals:

- A general IAM system. Use the existing internal identity provider and ingress.
- Field-level authorization inside an event. A principal who can read events sees the event.
- Per-request deletion. Packs are immutable; retention is the control (RFD 0002).
- A public transparency portal. This is an internal audit system.

## Options

### Enforcement point

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| A. Enforce at the trusted ingress only | Central policy; service stays simple | Service must trust headers; direct store access bypasses it; read audit needs service context | Rejected as sole control |
| B. Enforce in the service, with ingress authn | Identity is signed; service owns authorization and read audit; testable | Service must implement roles; ingress contract must be strict | **Chosen** |
| C. Enforce in each store (Quickwit/ClickHouse/B2 ACLs) | Defense in depth | Three policy languages; hard to audit consistently; analysts need direct access | Partial; stores are locked down, but not the primary UX |
| D. No auth; rely on network isolation | Simple | No named access, no read audit, unacceptable for real data | Rejected |

### Content read path

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| A. Audited neuradex read API | One authorization point; read audit; verification; range GET | Service becomes a data path; must scale reads | **Chosen** |
| B. Presigned B2 URLs | Simple; direct download | URL is a bearer token; no per-read audit; expiry management; leaks in logs | Rejected |
| C. Direct B2 credentials for analysts | Flexible | No read audit; broad access; key sprawl | Rejected |
| D. Copy content into a query tool | Convenient | Duplicates data; loses chain of custody | Rejected |

### Read audit storage

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| A. Same `events` table/index, `event=audit.read` | One schema; one query surface; already retained | A privileged reader can see reads; same TTL as events | **Chosen for now** |
| B. Separate immutable table/index | Stronger separation; read audit can outlive data | Another pipeline; another access model | Future |
| C. Append-only file in B2 only | Cheap; immutable | Not searchable; slow investigation | Rejected as sole store |

## Determination

Adopt B/B/A:

- Authentication at the trusted ingress (mTLS for services, OIDC for humans). The ingress strips
  any client-supplied identity headers and adds a signed principal (e.g. a short-lived JWT or a
  mTLS-derived header) on the internal hop.
- Authorization in neuradex, using the roles below. Once the auth fields land, the service will
  record the principal in `auth_subject`, `auth_method`, and `user_id` (not in schema 1.0.0).
- Content reads go through the audited read API; direct B2 credentials are limited to the
  service and break-glass admins.
- Read audit events use the same event schema (`event=audit.read` or `event=content.read`) and
  are written to the same archive, Quickwit, and ClickHouse. They are immutable from the
  application's perspective. A separate immutable store is a future hardening step.

### Roles

| Role | Can read events | Can read content | Can query ClickHouse | Can write events/content | Can change retention | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| `service` | no | no | no | yes | no | The neuradex writer identity |
| `audit.reader` | yes | no | no | no | no | Incident response, compliance |
| `content.reader` | yes | yes | no | no | no | Senior incident responders; content is more sensitive |
| `analyst` | aggregates only | no | yes (read-only) | no | no | Dashboards and capacity |
| `admin` | yes | yes | yes | yes | yes | Break-glass only; two-person approval |

A principal can hold multiple roles, but the read audit records the role used for the action.
`content.reader` implies `audit.reader` because a content ref is useless without its event.

### Enforcement matrix

| Action | Endpoint / credential | Required role | Read audit |
| --- | --- | --- | --- |
| Fetch/peek/search | `/v1/*` | `service` or an authenticated API client | Request event (not a read audit) |
| List/search events | `/v1/audit/events` (planned) | `audit.reader` | `audit.read` |
| Read one event | `/v1/audit/events/{request_id}` (planned) | `audit.reader` | `audit.read` |
| Read content | `/v1/audit/content/{request_id}` (planned) | `content.reader` | `content.read` |
| Export events | `/v1/audit/export` (planned) | `audit.reader` | `audit.read` with bytes/count |
| Query aggregates | ClickHouse HTTP | `analyst` | ClickHouse query log + daily review |
| Read raw B2 | B2 API | `service` or break-glass `admin` | B2 access log + `audit.read` |
| Change retention/lifecycle | B2 console/API | break-glass `admin` | Change ticket + B2 audit log |

The audit endpoints are **planned**. The current service exposes only the fetch/peek/search
API. Do not assume the paths above exist until they are added to OpenAPI.

### Identity contract

The service accepts identity only from the trusted ingress. The contract:

- The ingress terminates TLS and authenticates the client (mTLS or OIDC).
- It strips `X-Forwarded-User`, `X-Auth-Request-User`, `X-Forwarded-Email`, and any
  `Authorization` header from the client request.
- It adds a signed principal header/JWT with `sub`, `roles`, `iss`, `aud`, `exp`, and a key ID.
- The service verifies the signature, issuer, audience, and expiry on every request. It does not
  parse roles from an unsigned header.
- If verification fails, the request is rejected with the standard JSON error envelope. It is
  still audited as `client_error` with `auth_method=none` and no principal.
- The service will record the verified `sub` as `auth_subject` and the role used as
  `auth_method` (or the authentication method: `mtls`, `oidc`). Never record the raw token.
  These fields are future schema work.

For local development, the service may run with auth disabled and a local principal. This is
explicitly insecure and must be impossible to enable in production (a build-time or
environment-gated flag that the Helm chart never sets).

### Read audit event

`event=audit.read` or `event=content.read`:

| Field | Meaning |
| --- | --- |
| `request_id` | The read request's own ID, not the target's |
| `auth_subject` | Who read |
| `auth_method` | `mtls` or `oidc` |
| `user_id` | Stable user identifier |
| `read_action` | `list_events`, `get_event`, `get_content`, `export_events`, `verify_pack` |
| `read_target` | Request ID, pack ID, query hash, or export range |
| `read_reason` | Ticket/incident ID, required for content and exports |
| `read_bytes` | Bytes returned (0 for metadata-only) |
| `result_count` | Number of events or frames returned |
| `outcome` | `success`, `client_error`, `internal_error`, `failed` |
| `error_type` | Low-cardinality failure reason |

Read audit events do **not** contain the content bytes, the query text, or the header values.
They may contain a query hash and a target request ID. They are written through the same
at-least-once pipeline and are searchable in Quickwit/ClickHouse.

### Read API behaviour

- A read requires a reason for content and exports. An empty reason is a 400.
- A read returns the event metadata and, for content, a stream with `content-type`,
  `content-length` (captured length), `content-hash-blake3`, and `content-hash-sha256` headers.
- The service verifies the frame hashes before returning bytes. A mismatch is a 502 with a
  generic message; details go to logs and a `content.read` failure event.
- A read of an expired pack returns 410 Gone with a typed error. The event still exists; content
  does not.
- Pagination is cursor-based and bounded. An export is limited to a time range and a maximum
  row/byte count; a larger export requires an async job (future).
- Rate limits are per principal and per role. A content reader cannot turn the API into a bulk
  downloader without an export job and an explicit approval.

### Break-glass

1. Open a ticket with the incident/legal reason and the requested target.
2. Get a second approver from the service owners.
3. Issue a time-bound credential (≤ 4 hours) with the minimum role.
4. Notify the security channel.
5. All actions are read-audited; the ticket is linked in `read_reason`.
6. Revoke the credential at the end of the window, even if unused.
7. Review the read audit within one business day.

Break-glass is for incidents and legal requests, not for convenience. A recurring break-glass
pattern is a signal that a role is missing or a dashboard is inadequate.

## Economic, capacity, and performance considerations

Read traffic is expected to be tiny compared with ingest: investigations read a handful of
frames, not whole packs. Range GETs keep egress to the footer plus the requested frame. A bulk
export is the exception; it should be an async job with a budget, not a synchronous API call.

Read audit events add one row per read. At even 10,000 reads/day, this is negligible compared
with 3.36M request events/day. The cost of not auditing reads is much higher.

Serving content through the service adds a data path that must be scaled and monitored. If read
volume grows, put the read API behind the same autoscaling policy as the main service and cache
immutable footers by pack ID.

## Security considerations

- **Threat: insider reads without accountability.** Read audit + least privilege + break-glass
  review addresses it. It does not prevent a compromised `admin`; that is why break-glass is
  time-bound and reviewed.
- **Threat: identity spoofing.** The service verifies a signed principal and ignores unsigned
  headers. The ingress must strip client identity headers; a misconfiguration is a critical
  vulnerability. Test it.
- **Threat: token leakage in logs.** Never log `Authorization`, cookies, or the signed principal
  token. Log `auth_subject` and `key_id`, not the token.
- **Threat: direct store access.** B2 credentials are scoped and rotated; analysts do not get
  them. ClickHouse users are read-only and cannot see raw content (it is not there). Quickwit
  has no bodies.
- **Threat: read audit tampering.** The application cannot delete or update read audit events.
  B2 lifecycle is the only deletion path; restrict lifecycle changes. A future separate
  immutable store strengthens this.
- **Threat: content in tickets/chat.** Captured URLs and bodies are sensitive. Train responders
  to attach request IDs, not bodies. Redact before sharing.
- **Encryption:** TLS in transit; B2 SSE at rest; ClickHouse/Quickwit disk encryption at the
  infrastructure layer. A customer-managed key is an open question.
- **Retention:** 365 days for content and events. There is no per-request deletion. A legal
  deletion request requires a pack rewrite, which is not supported initially; document the
  limitation and escalate.
- **Local stack:** no auth, localhost only, synthetic data. Never expose it or load real data.

## Operational considerations

- Alert on `audit.read` failures, content reads with hash mismatch, exports over the byte
  budget, and break-glass credential issuance.
- Review read audit weekly for anomalous patterns: a principal reading many unrelated requests,
  a content reader with no incident ticket, repeated 403s, or a spike in export bytes.
- A 403 is audited but should not leak whether a target exists. Return the same error shape for
  "not found" and "not authorized" where possible.
- A read audit write failure must not silently drop the read. Fail closed for content reads if
  the read audit cannot be recorded; fail open for metadata reads only if the event pipeline is
  degraded and the read is still logged to the local spool. Document the chosen behaviour in the
  implementation.
- Credential rotation: ingress signing keys, B2 keys, ClickHouse users, HMAC key. Keep a key ID
  and an overlap window.
- Runbook: [operations/runbook.md](../operations/runbook.md#access-and-read-audit).

## Open questions

| Question | Why it is open | Trigger / owner |
| --- | --- | --- |
| Do read audit events need a separate immutable store? | Same-table storage is simpler but a privileged reader can see reads | Security review after the read API exists |
| Should content reads fail closed if read audit is down? | Fail-closed protects accountability; fail-open preserves incident response | Decide with the first read API implementation |
| What is the exact ingress principal format? | Depends on the ingress/JWT library | Deployment review |
| Do analysts need row-level filters? | A future schema may add query text or auth fields; column grants can hide them | Add a restricted view if needed |
| Is a customer-managed encryption key required? | B2 SSE may be enough; legal may require more | Security/legal review |
| How is a legal per-request deletion handled? | Packs are immutable; rewrite is expensive | Legal/security review; unsupported today |
| What is the break-glass SLA? | Depends on on-call and security coverage | Operations review |

## References

- [RFD 0001: Audit event model](0001-audit-event-model.md)
- [RFD 0002: Content-addressed storage](0002-content-addressed-storage.md)
- [RFD 0005: Header policy](0005-header-policy.md)
- [RFD 0008: Capacity, cost, and retention](0008-capacity-cost-and-retention.md)
- [Access and security](../architecture/access-security.md)
- [Event fields](../reference/event-fields.md)
- Code: `crates/neuradex-audit`, `crates/neuradex/src/api/v1/error.rs`
