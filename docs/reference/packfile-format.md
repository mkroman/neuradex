# Packfile format

**Status:** implemented (PR #68) in `crates/neuradex-packfile`. This page mirrors the code in
`frame.rs`, `footer.rs`, `trailer.rs`, `writer.rs`, and `reader.rs`. Format version `1`.

A pack is a sequence of self-describing frames, followed by a sorted footer index, followed by
a fixed-size trailer. **Frames start at offset 0; there is no pack header.** The footer is a
rebuildable index: frames are the source of truth.

## Layout

All integers are little-endian. Hash bytes are raw, not hex. Offsets are absolute from the start
of the pack. There is no alignment padding.

```text
offset 0
+------------------------------+
| Frame 0 (96 B header + zstd) |
| Frame 1                      |
| ...                          |
+------------------------------+
| Footer (32 B + N*64 B        |
|         + optional N*32 B)   |
+------------------------------+
| Trailer (128 B)              |
+------------------------------+ EOF
```

| Region | Size | Notes |
| --- | --- | --- |
| Frame | `96 + compressed_len` | Header then a zstd frame |
| Footer | `32 + N*64 + (audit ? N*32 : 0)` | Sorted by BLAKE3; optional SHA-256 audit table |
| Trailer | 128 B | Footer location; pack BLAKE3/SHA-256; footer checksum copy |

The writer can produce an empty pack (zero frames, footer + trailer). The content pipeline
should not upload one; the format itself permits it and the reader handles it.

## Frame

### Frame header (96 bytes)

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 4 | `magic` | ASCII `NDPF` |
| 4 | 2 | `version` | `1` |
| 6 | 2 | `flags` | Bit 0: `zstd`; other bits must be zero |
| 8 | 8 | `plaintext_len` | Decoded plaintext length, u64 |
| 16 | 4 | `compressed_len` | zstd payload length, u32 |
| 20 | 4 | `reserved` | Zero |
| 24 | 32 | `blake3` | BLAKE3-256 of decoded plaintext |
| 56 | 32 | `sha256` | SHA-256 of decoded plaintext |
| 88 | 8 | `xxh3` | XXH3-64 of compressed payload |

The payload starts immediately after the 96-byte header and is exactly `compressed_len` bytes.
`plaintext_len` is u64 in the frame, but the writer rejects a body that does not fit the footer's
u32 `plaintext_len`; in practice the maximum representable content is `u32::MAX`.

`FRAME_FLAG_ZSTD` (`0x0001`) must be set. The writer always produces a zstd frame; there is no
raw/uncompressed codec in v1. `FrameHeader::decode` rejects unknown flag bits and a frame
without the zstd flag.

There is **no content type, truncation flag, pack ID, timestamp, or algorithm ID** in the frame.
Content truncation is an event-level `operation.truncated` flag; the frame stores the truncated
bytes as plaintext. The packfile format pins BLAKE3-256, SHA-256, and XXH3-64 in this version.

### Frame checksum

`xxh3` is computed over the compressed payload bytes, not the plaintext. It is a fast
non-cryptographic corruption check. Content identity is BLAKE3; SHA-256 is the audit hash.

## Footer

### Footer header (32 bytes)

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 4 | `magic` | ASCII `NDPX` |
| 4 | 2 | `version` | `1` |
| 6 | 2 | `flags` | Bit 0: audit table present; other bits zero |
| 8 | 4 | `entry_count` | u32 |
| 12 | 4 | `reserved` | Zero |
| 16 | 8 | `XXH3-64` | Checksum of the footer encoding |
| 24 | 8 | `reserved` | Zero |

`footer_len = 32 + entry_count * 64 + (has_audit ? entry_count * 32 : 0)`.

The checksum is XXH3-64 over the complete footer encoding with the checksum field itself
(bytes 16..24) treated as zero. The trailer stores a copy of this checksum.

### Footer entry (64 bytes)

Entries are sorted **strictly increasing** by the raw 32-byte BLAKE3 value. The writer rejects
unsorted entries and duplicate BLAKE3 hashes.

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 32 | `blake3` | Content identity; sort key |
| 32 | 8 | `frame_offset` | Absolute offset of the frame header |
| 40 | 4 | `compressed_len` | Payload length, u32 |
| 44 | 4 | `plaintext_len` | Decoded length, u32 |
| 48 | 8 | `xxh3` | Checksum of compressed payload |
| 56 | 2 | `flags` | Copy of the frame flags |
| 58 | 2 | `reserved` | Zero |
| 60 | 4 | `reserved` | Zero |

The entry has no SHA-256 field. If the audit table is present, its entry at the same index is
the SHA-256 for this BLAKE3 hash. The entry also has no content type; the frame header is
self-describing for the few fields it carries.

### Optional audit table

`flags` bit 0 (`FOOTER_FLAG_AUDIT`) means the footer ends with `entry_count * 32` bytes of
SHA-256 hashes, in the same order as the entries. `Footer::from_entries` accepts `None` or a
same-length table. The writer always emits `Some(audit)` (one SHA-256 per entry); the reader
handles both forms.

## Trailer (128 bytes)

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 8 | `magic` | ASCII `NDPKTRLR` |
| 8 | 2 | `version` | `1` |
| 10 | 2 | `flags` | Zero |
| 12 | 4 | `reserved` | Zero |
| 16 | 8 | `footer_offset` | Absolute footer offset, u64 |
| 24 | 8 | `footer_len` | Footer length, u64 |
| 32 | 32 | `pack_blake3` | BLAKE3-256 of all bytes before the trailer |
| 64 | 32 | `pack_sha256` | SHA-256 of all bytes before the trailer |
| 96 | 8 | `footer_checksum` | Copy of the footer's XXH3-64 |
| 104 | 24 | `reserved` | Zero; reserved for a future keyed-BLAKE3 extension |

The trailer is always the last 128 bytes. The pack hashes cover every byte before the trailer:
all frames and the footer. The trailer itself is not hashed.

There is **no keyed MAC** in v1. The 24 reserved trailer bytes are room for a future
keyed-BLAKE3 extension; no key management policy exists in this crate yet.

## Writer behavior

`PackWriter<W: Write>`:

- `new` uses zstd level 3 (`DEFAULT_COMPRESSION_LEVEL`); `with_compression_level` validates the
  zstd range.
- `append(body)` computes BLAKE3 + SHA-256 over the plaintext, rejects a duplicate BLAKE3 hash
  in the same pack (`PackError::DuplicateHash`), zstd-compresses, computes XXH3 over the
  compressed bytes, writes the 96-byte header and payload, and records a footer entry.
- It writes frames in append order. The footer is sorted at `finish`.
- `finish()` writes the footer, computes the pack BLAKE3/SHA-256 over frames+footer, writes the
  trailer, flushes the sink, and returns `PackSummary { entry_count, footer_offset, footer_len,
  pack_blake3, pack_sha256, bytes }`.
- After `finish` or an I/O failure the writer is poisoned; further appends fail.
- It buffers one compressed frame at a time, not the whole pack.

The writer does **not** enforce the pipeline's 128 MiB target, 10-minute max age, or 25 MiB body
cap. Those are content-layer policies. The format limits are the u32 lengths above.

## Reader behavior

`PackReader::new(bytes)` takes a complete pack byte slice and validates:

1. The trailer magic/version/flags/reserved bytes.
2. `footer_offset + footer_len + TRAILER_LEN == pack_len`.
3. The footer magic/version/flags/reserved bytes, entry count/length, sorted entries, and
   footer checksum.
4. Every footer entry's frame range is within `[0, footer_offset)`.
5. The pack BLAKE3/SHA-256 over `bytes[..pack_len - 128]`.

Frame payloads are verified lazily:

- `lookup(&ContentHash)` binary-searches by BLAKE3. If the audit table is present, the supplied
  SHA-256 must also match.
- `frame(entry)` decodes the 96-byte header and checks flags, plaintext/compressed lengths,
  BLAKE3, and XXH3 against the footer entry. If the audit table is present, it also checks the
  frame's SHA-256.
- `FrameRef::decode()` checks the compressed length, verifies XXH3, decompresses with a bound of
  `plaintext_len + 1`, verifies the decoded length, then BLAKE3 and SHA-256.

If the footer entry and frame header disagree, the reader returns `PackError::EntryMismatch`.
The frame header is authoritative for the bytes it describes; the footer is an index and must
match it.

### Range reads

The format is range-friendly (trailer at the end, fixed-size footer header/entries, fixed-size
frame header), but the current `PackReader` API takes a complete `&[u8]`. A storage adapter can
implement range GETs later: read the last 128 bytes, then the footer, then the selected frame.
Do not claim the current reader does this.

## Integrity summary

| Check | Covers | Cryptographic? | When |
| --- | --- | --- | --- |
| XXH3-64 | Compressed payload | No | Before decompression |
| BLAKE3-256 | Decoded plaintext | Yes | After decompression |
| SHA-256 | Decoded plaintext | Yes | After decompression |
| Footer XXH3-64 | Footer encoding (checksum field zeroed) | No | Before parsing entries |
| Pack BLAKE3/SHA-256 | Frames + footer | Yes | `PackReader::new` |
| Trailer footer checksum copy | Footer checksum consistency | No | `PackReader::new` |

## Worked checks

Inspect the first frame and the trailer:

```sh
stat -c '%s bytes' pack.nrp
xxd -g1 -l 96 pack.nrp          # first frame header; offset 0
tail -c 128 pack.nrp | xxd -g1  # trailer
```

The first frame's magic is `NDPF`; there is no pack header before it. The last 128 bytes start
with `NDPKTRLR`. A standalone CAS object (future large-body path) can be verified with
`b3sum`/`sha256sum`; a pack frame requires the reader because it is compressed and
offset-addressed.

## Compatibility

- Unknown version, flags, magic, or reserved bytes are rejected. Do not guess.
- The format pins BLAKE3-256/SHA-256/XXH3-64 for version 1. A new algorithm needs a new format
  version and a migration/reindex plan.
- A new keyed MAC can use the reserved trailer bytes, but it must be specified and versioned; it
  is not part of v1.
- Do not add per-frame encryption without a new version. B2 SSE is the current at-rest control.

## Test coverage

The crate tests include: hash vectors, frame header round-trip and malformed flags, writer →
footer → lookup → frame decode, multiple bodies, empty/small/~1 MiB bodies, duplicate rejection,
footer without audit, footer rebuild from frame headers, truncated frame/footer/trailer, all
prefixes of a pack, corrupted checksums/hashes/offsets, bad magic/version, invalid zstd,
out-of-bounds entries, and a deterministic single-byte mutation sweep.

See [testing.md](../development/testing.md#neuradex-packfile).
