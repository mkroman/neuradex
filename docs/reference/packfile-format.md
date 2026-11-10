# Packfile format

**Status:** specified, pack format v1. `crates/neuradex-packfile` is scaffolded; the format is
not implemented yet. This page is the contract for the implementation and for any independent
reader.

A packfile is an immutable object containing many content frames, a sorted footer index, and a
trailer. It is the unit of upload and retention. The footer is authoritative within the pack;
the hourly request index maps a request to a pack and frame.

## Goals

- One PUT per pack, not per request.
- Content-addressed frames (BLAKE3-256) with an independent SHA-256 audit hash.
- Cheap corruption detection on the compressed bytes (XXH3-64).
- Range-readable footer and frames without downloading the whole pack.
- A format that is simple enough to implement and test without an async runtime.
- Versioned algorithms and flags so the format can evolve.

## Layout

All integers are **little-endian**. Hash bytes are raw (not hex). Offsets are absolute from the
start of the file. There is no alignment padding.

```text
offset 0
+------------------------------+
| Pack header (64 B)           |
+------------------------------+
| Frame 0 (header + payload)   |
| Frame 1                      |
| ...                          |
+------------------------------+
| Footer (32 B + N * 112 B)    |
+------------------------------+
| Trailer (64 B)               |
+------------------------------+ EOF
```

| Region | Size | Notes |
| --- | --- | --- |
| Pack header | 64 B | Magic, version, pack ID, creation time |
| Frame | `112 + content_type_len + compressed_len` | One content record |
| Footer | `32 + entry_count * 112` | Sorted by BLAKE3 |
| Trailer | 64 B | Footer location and SHA-256 |

A pack with zero frames is not produced and must not be uploaded. The minimum meaningful file is
one frame + footer + trailer.

## Pack header (64 bytes)

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 8 | `magic` | ASCII `NRPACK01` |
| 8 | 2 | `version` | `1` |
| 10 | 2 | `header_len` | `64` |
| 12 | 4 | `flags` | Reserved, must be `0` |
| 16 | 16 | `pack_id` | 128-bit opaque ID (UUIDv7 bytes) |
| 32 | 8 | `created_unix_ms` | First-frame append time, ms since Unix epoch |
| 40 | 24 | `reserved` | Zero |

`created_unix_ms` drives the 10-minute max-age seal. It is not the upload time.

## Frame

```text
+------------------------------+  frame_offset
| Frame header (112 B + type)  |
+------------------------------+
| Compressed payload           |
+------------------------------+  frame_offset + frame_length
```

`frame_length = header_len + compressed_len`, where `header_len = 112 + content_type_len`.
`content_type_len` is 0–255; the type bytes are UTF-8 without a NUL.

### Frame header (112 bytes + content type)

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 4 | `magic` | ASCII `NRFR` |
| 4 | 2 | `version` | `1` |
| 6 | 2 | `header_len` | `112 + content_type_len` |
| 8 | 2 | `flags` | Bit 0: `truncated`; bits 1–15 reserved |
| 10 | 1 | `codec` | `0` raw, `1` zstd |
| 11 | 1 | `hash_alg` | `1` BLAKE3-256 |
| 12 | 1 | `audit_hash_alg` | `2` SHA-256 |
| 13 | 1 | `checksum_alg` | `1` XXH3-64 |
| 14 | 2 | `reserved` | Zero |
| 16 | 2 | `content_type_len` | 0–255 |
| 18 | 8 | `uncompressed_len` | Canonical content bytes |
| 26 | 8 | `compressed_len` | Payload bytes |
| 34 | 32 | `blake3` | BLAKE3-256 of canonical bytes |
| 66 | 32 | `sha256` | SHA-256 of canonical bytes |
| 98 | 8 | `xxh3` | XXH3-64 of the compressed payload |
| 106 | 6 | `reserved` | Zero |
| 112 | var | `content_type` | UTF-8 media type, no parameters preferred |

If `codec=0`, `compressed_len == uncompressed_len` and the payload is the canonical bytes. If
`codec=1`, the payload is a zstd frame whose decoded length is `uncompressed_len`.

`flags.truncated` means the canonical bytes are a prefix cut at the endpoint cap. The hashes
identify the prefix, not the full upstream body.

## Footer

### Footer header (32 bytes)

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 8 | `magic` | ASCII `NRIDX001` |
| 8 | 2 | `version` | `1` |
| 10 | 2 | `header_len` | `32` |
| 12 | 4 | `entry_count` | Number of entries, 1–65,536 |
| 16 | 2 | `entry_size` | `112` |
| 18 | 2 | `reserved` | Zero |
| 20 | 4 | `reserved` | Zero |
| 24 | 8 | `reserved` | Zero |

`footer_length = 32 + entry_count * 112`.

### Footer entry (112 bytes)

Entries are sorted ascending by the raw 32-byte BLAKE3 value (unsigned lexicographic). The writer
must not emit duplicate BLAKE3 values; a duplicate is a writer bug and a reader may reject it.

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 32 | `blake3` | Content identity |
| 32 | 32 | `sha256` | Audit hash |
| 64 | 8 | `frame_offset` | Absolute offset of the frame header |
| 72 | 8 | `compressed_len` | Payload bytes |
| 80 | 8 | `uncompressed_len` | Canonical bytes |
| 88 | 8 | `xxh3` | Checksum of the compressed payload |
| 96 | 4 | `flags` | Bit 0: `truncated`; bits 1–31 reserved |
| 100 | 4 | `reserved` | Zero |
| 104 | 8 | `reserved` | Zero |

The footer entry deliberately omits the content type: the reader gets it from the frame header.
This keeps the entry fixed-size for binary search.

## Trailer (64 bytes)

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 8 | `magic` | ASCII `NREND001` |
| 8 | 2 | `version` | `1` |
| 10 | 2 | `trailer_len` | `64` |
| 12 | 8 | `footer_offset` | Absolute offset of the footer |
| 20 | 8 | `footer_length` | `32 + entry_count * 112` |
| 28 | 32 | `footer_sha256` | SHA-256 of the footer bytes |
| 60 | 4 | `reserved` | Zero |

The trailer is always the last 64 bytes. `footer_sha256` covers the footer header + entries, not
the trailer itself. The pack as a whole has no separate checksum; frame hashes and the footer
hash cover the content, and B2 provides transport/storage integrity.

## Identifiers and naming

- `pack_id`: 16 raw bytes. UUIDv7 is the provisional encoding; the format treats it as opaque.
- Object key: `packs/v1/date=YYYY-MM-DD/hour=HH/pack-<pack_id>.nrp`, where the date/hour are
  the pack creation time in UTC.
- The file name uses the canonical hyphenated lowercase UUID form.
- Frames are located by `(pack_id, frame_offset)`; `frame_length` is a convenience for range GET
  and validation.

## Limits

| Limit | Value | Enforcement |
| --- | --- | --- |
| Pack target | 134,217,728 B (128 MiB) | Seal before a frame crosses it |
| Pack hard cap | 167,772,160 B (160 MiB) | Never exceed; a single frame may go over target but not hard cap |
| Max pack age | 600 s | Seal at the next frame boundary after 10 min |
| Max entries | 65,536 | Seal before adding another |
| Max content type | 255 B | Reject/truncate at write |
| Max frame payload | 26,214,400 B (25 MiB) current capture cap | Reject at write |
| Footer max | 7,340,064 B (~7.0 MiB) at 65,536 entries | Bounds range reads |
| Trailer | 64 B | Fixed |

A body at or above the 30 MiB large-body threshold is stored as a standalone CAS object, not a
frame. Because the current capture cap is 25 MiB, this path is effectively disabled.

## Write procedure

1. Write the 64-byte pack header with a new `pack_id` and `created_unix_ms`.
2. For each content record:
   - Compute BLAKE3-256 and SHA-256 over canonical bytes.
   - If the BLAKE3 hash is already in this pack's in-memory index, reuse that frame and do not
     append.
   - Compress with zstd (level 3 by default). If compressed length ≥ uncompressed length, use
     codec 0 (raw) instead.
   - Compute XXH3-64 over the payload.
   - Record `frame_offset = current file length`.
   - Write the frame header, content type, and payload.
   - Add a footer entry.
3. On seal (target, age, or entry cap):
   - Sort entries by BLAKE3.
   - Write the footer.
   - Compute SHA-256 of the footer bytes.
   - Write the trailer.
   - `fsync` the local file; verify its size.
4. Upload the whole object in one PUT (or multipart if the implementation later exceeds the
   single-PUT timeout). Verify the remote size with HEAD.
5. Write the hourly request index **after** the pack PUT succeeds.

A crash before seal leaves a local temp file that is discarded on startup and never uploaded.

## Read procedure

1. HEAD the pack; require size ≥ `64 + 32 + 112 + 64` (one frame) and record the size.
2. Range GET the last 64 bytes. Verify `NREND001`, version, `trailer_len=64`, and that
   `footer_offset + footer_length + 64 == size`.
3. Range GET the footer. Verify `footer_sha256`, magic, version, `header_len=32`,
   `entry_size=112`, and that `entry_count` fits the footer length.
4. Binary search the sorted entries for the BLAKE3 hash.
5. Range GET the frame header (`112 + content_type_len`). Verify magic/version/header_len,
   offsets/lengths, and that the footer entry matches the frame header (hashes, lengths, flags).
6. Range GET the compressed payload.
7. Verify XXH3-64 over the payload.
8. Decompress if codec 1; verify `uncompressed_len`.
9. Verify BLAKE3-256 and SHA-256 over the canonical bytes.
10. Return bytes only if all checks pass.

If the footer entry and the request index disagree, the footer wins. If a hash check fails, do
not return partial bytes; record a `content.read` failure.

## Integrity

| Check | Covers | Cryptographic? | When |
| --- | --- | --- | --- |
| XXH3-64 | Compressed payload | No | Before decompression |
| BLAKE3-256 | Canonical bytes | Yes (128-bit security) | After decompression |
| SHA-256 | Canonical bytes | Yes | After decompression |
| Footer SHA-256 | Footer bytes | Yes | Before parsing entries |
| B2 TLS/SSE | Transport/at rest | Yes | In transit/at rest |

XXH3 is a corruption check, not an identity. An attacker who can write packs could construct a
chosen XXH3; BLAKE3/SHA-256 are the security boundary.

## Worked checks

Inspect the header and trailer:

```sh
stat -c '%s bytes' pack-<id>.nrp
xxd -g1 -l 64 pack-<id>.nrp
tail -c 64 pack-<id>.nrp | xxd -g1
```

A standalone CAS object (future large-body path) can be verified with standard tools:

```sh
b3sum body.bin
sha256sum body.bin
```

A frame requires the pack reader because it is compressed and offset-addressed. The reader
exposes the same hashes as the event; do not trust `xxh3` as a content hash.

## Compatibility rules

- A reader must reject unknown `version`, codec, hash algorithm, audit hash algorithm, or
  checksum algorithm IDs with a typed error. Do not guess.
- A reader must bounds-check every offset/length before slicing or allocating.
- New flags must be defined in a new format version if they change the meaning of existing
  bytes. Reserved bits must be written as zero and ignored by readers.
- A format change that affects identity or compression requires a new pack version and a
  migration/reindex plan. Old packs remain readable.
- Do not add per-frame encryption without a new version; B2 SSE is the current at-rest control.

## Test plan

The crate tests must cover:

- one/many frames, sorted footer, duplicate dedupe;
- empty content and exactly-at-target packs;
- raw and zstd codecs;
- truncated frame, footer, and trailer;
- one-byte corruption in each region;
- offset/length overflow and entry-count cap;
- unknown algorithm/version IDs;
- round-trip of a 25 MiB frame.

See [testing.md](../development/testing.md#neuradex-packfile).
