---
rfd: 3
title: Hashing
authors:
  - neuradex maintainers
state: published
discussion: https://github.com/mkroman/neuradex/pull/70
labels:
  - content
  - security
  - architecture
created: 2026-11-10
updated: 2026-11-10
---

# RFD 3: Hashing

> **Status:** the BLAKE3/SHA-256/XXH3 rules are implemented in `neuradex-packfile` and
> `neuradex-audit`. The content orchestration that applies them to live requests is not wired.

## Summary

Use **BLAKE3-256** as the content identity and CAS key. Store an independent **SHA-256** digest
of the same bytes for audit and interoperability. Use **XXH3-64 only as a checksum of the
compressed frame**, never as content identity. Sensitive header values are recorded with
**HMAC-SHA256** under a separate audit key, not with a plain hash.

## Context and scope

Content-addressed storage needs a collision-resistant digest that is fast enough to run on
every captured body. Auditors and external tools need a digest they can verify with standard
software. The pack format needs a cheap corruption check that can be computed and verified on
the compressed bytes without decompressing. These are three different jobs and must not be
conflated.

The content rate is large: at 100,000 fetches/hour and a 4 MiB average, the service hashes about
111 MiB/s average, ~222 MiB/s at a 2× peak. Hash choice affects CPU, pack size, and the cost of
re-verification.

Out of scope: signing content, Merkle trees over packs, and encryption of content bytes. Those
may be added later as separate layers.

## Goals and non-goals

| Goal | How we know it is met |
| --- | --- |
| One unambiguous content identity | Every event and footer entry uses the same BLAKE3-256 over the same canonical bytes |
| Independent audit verification | `sha256sum` on the stored bytes matches `ContentReference.sha256` |
| Cheap frame corruption detection | XXH3-64 is verified before decompression |
| No secret leakage through hashes | Sensitive headers use HMAC, not a bare hash of a low-entropy value |
| Versioned algorithms | Pack format v1 pins BLAKE3/SHA-256/XXH3; a new algorithm needs a new version |

Non-goals:

- Using a hash as a password or a token.
- Preventing a determined attacker from confirming a known body (hashes are not encryption).
- Content deduplication across packs (see RFD 0002).
- Post-quantum hashing. No such requirement exists today.

## Options

### Content identity

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| BLAKE3-256 | Very fast, parallel, 256-bit output; good Rust support; already a workspace dependency | Less ubiquitous in external tooling than SHA-256 | **Chosen** |
| SHA-256 | Ubiquitous; hardware acceleration; auditors can verify with `sha256sum` | Slower on short inputs and less parallel; CPU cost at 100k fetches/h | Kept as the audit hash, not the CAS key |
| SHA-512/256 | Strong, standard | No advantage over BLAKE3 here; slower in software | Rejected |
| XXH3-64 | Extremely fast | 64-bit, non-cryptographic; collisions are trivial to construct | **Never** for identity |
| BLAKE2b-256 | Mature, fast in software | BLAKE3 is faster and already present | Rejected |

### Audit hash

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| SHA-256 of the same bytes | Standard tooling; independent implementation; cheap enough | Second pass or dual-pass hashing; 32 more bytes per frame | **Chosen** |
| Reuse BLAKE3 | One pass | An auditor must trust the same implementation/algorithm; no independent check | Rejected |
| No audit hash | Less storage | Cannot verify with standard tools; weaker audit story | Rejected |

### Frame checksum

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| XXH3-64 over compressed payload | Very fast; detects transfer/storage corruption before decompression; 8 bytes | Not cryptographic; not an identity | **Chosen** |
| CRC32C | Hardware support; standard | Weaker; 4 bytes; no advantage in this stack | Rejected |
| SHA-256 of compressed payload | Strong | Redundant with the content hashes; slower | Rejected |
| No checksum | Simpler | A corrupt frame is only detected after decompression/hash, and the error is less local | Rejected |

## Determination

Adopt BLAKE3-256 for content identity, SHA-256 for the audit hash, and XXH3-64 for the
compressed-frame checksum.

### Canonical content bytes

The hashes are computed over the **decompressed response bytes that neuradex actually
processed**, before UTF-8 replacement and before JSON encoding:

- `fetch`: the full decompressed body, capped at 25 MiB. If the cap is hit, the hash is of the
  captured prefix and `operation.truncated=true`.
- `peek`: the decompressed bytes read up to the end of the document head, capped at 2 MiB. If
  the parser stops early, the hash covers the bytes actually captured, not the full upstream
  response.
- `search`: no content bytes; no content reference.
- Empty body: the hashes of the empty byte string are valid and can be stored as a reference of
  size 0. An absent `content_references` array means no capture, not an empty body.

Do **not** hash:

- the HTTP response body after UTF-8 lossy decoding (that is a derived representation);
- the compressed frame bytes;
- a JSON envelope containing the body;
- headers or metadata.

The raw body may be binary even though `fetch` rejects binary content types; the canonical
bytes are still whatever decompressed bytes were read. This keeps the hash independent of
encoding choices.

### Where each hash is used

| Hash | Input | Encoding | Stored in | Used for |
| --- | --- | --- | --- | --- |
| BLAKE3-256 | Canonical content bytes | 64 lowercase hex chars in events; raw 32 bytes in packfiles | `ContentReference.blake3`; frame header; footer entry | Content identity, footer search key, duplicate rejection within a pack |
| SHA-256 | Canonical content bytes | 64 lowercase hex chars in events; raw 32 bytes in packfiles | `ContentReference.sha256`; frame header; optional footer audit table | Independent audit verification |
| XXH3-64 | Compressed frame payload | u64 in the frame/footer | Frame header; footer entry | Corruption detection before decompression |
| HMAC-SHA256 | Hashed-header value (IP/user/chat IDs) | 64 lowercase hex chars | HeaderCapture `values[].value` | Correlate identifiers without storing them |
| SHA-256 | Search query text | 64 lowercase hex chars | `operation.query_sha256` | Aggregate search analytics without query text |

`ContentReference.blake3` is the key. `ContentReference.sha256` is not used for lookup. The
frame/footer `xxh3` is never used for lookup, dedupe, or equality across frames.

### Hashed-header HMAC

For the hashed-header list in `header_policy.rs` (IP/user/chat identifiers), the HMAC is over
the raw header value bytes and is rendered as 64 lowercase hex characters. Credentials such as
`authorization` and cookies are redacted, not HMACed (RFD 0005).

- The key is a dedicated audit HMAC key, not a B2 key, not the Kagi token, and not the TLS key.
- The key is at least 32 random bytes, stored in the secret manager, and rotated on a documented
  schedule. Rotation changes every future digest; old events keep the old key. A key ID is
  future work.
- HMAC-SHA256 is used instead of plain SHA-256 because a low-entropy identifier (an IP address,
  a user ID) could otherwise be confirmed by brute force.
- The HMAC is not a content hash and must never be compared with `ContentReference.sha256`.

### Verification rules

On write:

1. Read the canonical bytes.
2. Compute BLAKE3 and SHA-256 in one pass where the implementation allows it.
3. Compress.
4. Compute XXH3-64 over the compressed payload.
5. Write the 96-byte frame header, then the zstd payload.
6. Add the footer entry (BLAKE3, frame offset, compressed/plaintext lengths, XXH3, flags) and
   the same-index SHA-256 in the optional audit table.

On read (the current reader takes a complete pack slice; a range adapter is future work):

1. Read the trailer, footer, and the selected frame.
2. Verify the footer checksum and pack BLAKE3/SHA-256 over the pre-trailer bytes.
3. Binary-search the footer by BLAKE3 and verify the frame header matches the entry.
4. Verify XXH3-64 over the compressed payload.
5. Decompress with the header's plaintext length as the bound.
6. Verify BLAKE3-256 and SHA-256 of the decoded bytes against the frame header.
7. Return bytes only if all checks pass.

If BLAKE3 matches but SHA-256 does not, treat it as corruption or an implementation bug; do not
return bytes. If the request index and footer disagree, verify the frame header and rebuild
the footer/index rather than trusting either blindly (RFD 0002).

### Algorithm agility

- Pack format version 1 pins BLAKE3-256 for content identity, SHA-256 for the audit hash, and
  XXH3-64 for the compressed-frame checksum. The frame header has no algorithm-ID bytes.
- A new content hash algorithm requires a new pack format version and a migration plan. Do not
  silently mix algorithms in one footer.
- The trailer reserves 24 bytes for a future keyed-BLAKE3 MAC; that is a separate extension,
  not an algorithm selector, and it is not implemented in v1.

## Economic, capacity, and performance considerations

| Operation | Approximate throughput | CPU at 111 MiB/s average | Notes |
| --- | --- | --- | --- |
| BLAKE3-256 | 2–5 GiB/s per core (SIMD) | ~0.02–0.06 core | Parallelizes across chunks |
| SHA-256 | 0.5–1.5 GiB/s per core with SHA-NI | ~0.07–0.22 core | The audit hash is the larger hash CPU cost |
| XXH3-64 | >10 GiB/s per core | negligible | On compressed bytes, which are smaller |
| zstd level 3 | 200–500 MiB/s per core | ~0.2–0.6 core | Dominates the content pipeline CPU |

The exact numbers depend on hardware; measure on the staging host. The design does not depend on
these estimates. Storage overhead is 64 bytes of content hashes + 8 bytes of checksum per frame
plus the footer copy; at 32 frames per 128 MiB pack this is negligible.

SHA-256 is computed to make the audit independently verifiable. If CPU becomes the bottleneck,
do not drop SHA-256 silently; measure and record the trade-off in an RFD update.

## Security considerations

- Hashes are not encryption. A BLAKE3 hash of a known body confirms that body. Content access
  control is therefore enforced on the storage and read API, not by hiding hashes.
- BLAKE3-256 and SHA-256 are collision-resistant at the 128-bit security level. XXH3-64 is not;
  an attacker who can write packs could construct a frame with a chosen XXH3. The BLAKE3 and
  SHA-256 checks are the security boundary; XXH3 is only a fast corruption check.
- HMAC keys are separate from storage credentials. Compromise of B2 does not let an attacker
  forge header HMACs, and compromise of the HMAC key does not let them read content.
- Do not log raw header values while computing HMACs. The canonical string exists only in
  memory.
- Hash comparison must be constant-time for HMAC verification; content hash comparison may be
  ordinary equality because the hashes are public to authorized readers. HMAC verification
  should use a constant-time comparison.
- A future signature over a pack footer would provide provenance; it is not in scope.

## Operational considerations

- Hash mismatches are page-worthy if they are not isolated. A single mismatch can be a truncated
  download or a disk error; repeated mismatches suggest a systemic bug or storage corruption.
- Record `content.read` events with the verification result and the pack ID/frame offset. Never
  return unverified bytes.
- `sha256sum` can verify a standalone large-body CAS object directly. Pack frames require the
  neuradex reader or a small script that extracts the frame; provide one in the runbook.
- Backfills recompute hashes from the canonical bytes and compare with the event; a mismatch is
  a data-integrity incident, not a reason to overwrite the event.
- Key rotation: keep old key versions for the retention window so old HMACs remain explainable;
  store only the key ID with the event, never the key.

## Open questions

| Question | Why it is open | Trigger / owner |
| --- | --- | --- |
| Do we need a per-pack Merkle root or signature? | Would detect a malicious pack rewrite; not required for accidental corruption | Security review after the first production month |
| Is one SHA-256 pass worth the CPU? | Audit value is high, but it is the largest hash cost | Measure on staging; keep unless it exceeds 20% of the pipeline CPU |
| Should `query_hash` be keyed (HMAC) rather than plain SHA-256? | Short queries are low-entropy and could be brute-forced | Privacy review; currently plain SHA-256 is documented as aggregate-only |
| How long do we retain old HMAC key versions? | Needed to explain old events; increases key management surface | Set with the first key-rotation runbook |
| Do we hash the compressed or uncompressed frame for `xxh3`? | Current decision is compressed; it localizes corruption before decompression | Confirm with the packfile implementation tests |

## References

- [RFD 0002: Content-addressed storage](0002-content-addressed-storage.md)
- [RFD 0005: Header policy](0005-header-policy.md)
- [Packfile format](../reference/packfile-format.md)
- [Event fields](../reference/event-fields.md)
- BLAKE3 specification: <https://github.com/BLAKE3-team/BLAKE3-specs>
- XXH3 specification: <https://github.com/Cyan4973/xxHash/blob/dev/doc/xxhash_spec.md>
- Code: `crates/neuradex-packfile`, `crates/neuradex-audit`
