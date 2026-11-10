# Testing

The repository's tests are offline and fast. Compose is a manual integration aid, not part of
`cargo test`. This page defines what to run for the audit/content work and what a new test
should cover.

## Test matrix

| Layer | Command | What it covers | Runtime | Status |
| --- | --- | --- | --- | --- |
| Workspace formatting | `cargo fmt --check` | Rust style | seconds | implemented |
| Workspace clippy | `cargo clippy --all-targets --workspace -- -D warnings` | Lints, all targets | minutes | implemented |
| Workspace tests | `cargo test --workspace` | All crates, default features | minutes | implemented |
| Lean service | `cargo test -p neuradex --no-default-features` | OpenAPI without the docs page | minutes | implemented |
| Rustdoc | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` | Public docs, all features | minutes | implemented |
| Audit unit | `cargo test -p neuradex-audit` | Event structs, enums, redaction, HMAC | seconds | planned |
| Packfile unit | `cargo test -p neuradex-packfile` | Frame/footer/trailer, hashes, corruption | seconds | planned |
| Content unit | `cargo test -p neuradex-content` | Spool, pack builder, index, local store | seconds | planned |
| Service integration | `cargo test -p neuradex` | HTTP routes, extraction, errors | minutes | implemented |
| Compose smoke | `docker compose up -d` + curl/jq | Real Quickwit/ClickHouse/MinIO mappings | manual | implemented |
| Failure injection | stop a sink; corrupt a frame | Degradation and verification | manual | planned |

The first five commands are the CI gates. The planned commands become part of `--workspace`
automatically once the crates have tests; no CI change is needed.

## Before a PR

From the repository root, in this order:

```sh
cargo fmt
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
cargo test -p neuradex --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

For docs/compose-only changes (like this one), the Rust commands are still run by CI; run the
compose validation locally:

```sh
docker compose config --quiet
python3 - <<'PY'
import tomllib, yaml
for path in ["compose.yaml", "hack/quickwit/quickwit.yaml", "hack/quickwit/neuradex-events.yaml"]:
    with open(path) as f:
        yaml.safe_load(f)
for path in ["hack/vector/neuradex-events.toml"]:
    with open(path, "rb") as f:
        tomllib.load(f)
print("yaml/toml ok")
PY
```

If Docker is available, start the stack and run one event through Vector (see
[local-development.md](local-development.md#send-a-synthetic-event)). That catches Quickwit
mapping and ClickHouse DDL drift that unit tests cannot.

## What to test in each crate

### `neuradex-audit`

| Case | Expected |
| --- | --- |
| Serialize a minimal event | All required fields present; optional fields empty, not null |
| Deserialize an old event with a missing optional field | Succeeds with defaults |
| Unknown operation type | Rejected by serde/validation |
| Header allow-list | Only allowed values survive; unknown names are redacted, not dropped |
| Sensitive header | No value in serialized output; name and `value_bytes` retained, `values` empty |
| Non-UTF-8 header | Base64 value with `encoding=base64`; no panic |
| Header limits (64 names, 512 B/value, 8 KiB/side) | Truncated/omitted counters set; bounded output |
| URL sanitization | Only `target_scheme`/`target_host`/`target_port`/`target_path` plus `target_url_sha256`; no full URL |
| Query text never stored | `query_sha256`, `query_length`, `query_terms` populated; no query text field |
| Header side budget | Names and values stay within 8 KiB; omitted counts are recorded |

Use a sentinel token in every sensitive field and assert it is absent from the serialized event,
a `Debug` rendering, and the log capture. This is the highest-value audit test.

### `neuradex-packfile`

| Case | Expected |
| --- | --- |
| Write/read one frame | Bytes and hashes match |
| Write/read many frames | Footer sorted by BLAKE3; lookup works |
| Duplicate content | Writer rejects with `DuplicateHash`; content layer dedupes before append |
| Empty content | Valid frame; hashes of empty bytes |
| zstd frame codec | Round-trip; reject a non-zstd flag |
| Truncated frame/footer/trailer | Reader returns a typed error, not a panic |
| Corrupt compressed byte | XXH3 mismatch before decompression |
| Corrupt footer byte | Footer checksum mismatch |
| Corrupt trailer | Reader rejects the pack |
| Offset/length overflow | Rejected before slicing |
| Empty pack | Footer + trailer round-trip; pipeline does not upload it |
| Pack target/age | Content-layer policy; format does not enforce it |
| Unknown format version/flags | Reader rejects with a typed error |

Use property-style loops over sizes (0, 1, 2, 4 KiB, 1 MiB, 25 MiB) without a proptest
dependency. Keep the tests sync; this crate must not need a runtime.

### `neuradex-content`

| Case | Expected |
| --- | --- |
| Capture a complete body | Frame written; event ref filled |
| Capture at cap | `truncated=true`; hash of prefix |
| Client disconnect | No canonical frame; `client_disconnect` marker |
| Spool full | Frame dropped; `spool_full` marker; response unaffected |
| Pack seals at size | Upload called once; index written after |
| Pack seals at age | Upload called once |
| Dedupe within pack | One frame; two references |
| Upload failure | Retry, then `pack_upload_failed` |
| Upload success, index failure | Orphan pack; `index_write_failed` |
| Index retry | New object key; no overwrite |
| Read path | Index → footer → frame → verify |
| Read mismatch | 502; `content.read` failure |
| Missing pack | 410/404; no empty body |
| Local store | Works without network; `s3` feature off |

Use the `local` object store and `tempfile` for most tests. An `s3`-feature test can use
`httpmock` to assert request shapes; a real MinIO test belongs in the optional compose job.

### `neuradex` service

Existing tests cover extraction, the JSON error envelope, routing, OpenAPI, and the fetch/peek
handlers. New tests for this work:

| Case | Expected |
| --- | --- |
| A fetch produces one event | `event=neuradex.request`, `operation.type=fetch`, `http.route/status/duration_ms` set |
| A failed fetch produces one event | `outcome=server_error`, no content refs |
| A 404/405 is audited | Event emitted; `http.route` absent; `http.status` 404/405 |
| `/healthz` is not audited | No event |
| Content capture failure does not change the response | 200 with body; event has no content reference (capture status is future) |
| Search event has query hash, not query text | `operation.query_sha256` set; no query text field |
| Event sink down | Response still 200; spool/alert |
| Ingress identity spoofing | Unsigned header ignored/rejected |

The existing audit test in `src/api/v1.rs` installs a global tracing subscriber; follow its
pattern for request events. Do not add a new global subscriber in another test binary.

## Fixtures

| Fixture | Location | Use |
| --- | --- | --- |
| Kagi search responses | `crates/test-support/fixtures/kagi` | Existing parser/client tests |
| Synthetic HTML pages | inline in tests or `tempfile` | Fetch/peek parser tests |
| Event JSON | inline in tests | Audit serialization and sink contract |
| Pack bytes | generated in tests | Round-trip and corruption tests |
| Compose event | [local-development.md](local-development.md#send-a-synthetic-event) | Manual end-to-end |

Do not commit real page bodies, real URLs, tokens, or user data. Use `example.com` and
synthetic HTML.

## Compose smoke test

A minimal manual sequence:

```sh
docker compose up -d
docker compose ps
# One event through the real pipeline:
mkdir -p .local/neuradex
cp /dev/null .local/neuradex/events.jsonl
# append a synthetic event (see local-development.md)
docker compose --profile ingest up -d vector
sleep 12
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/search' \
  -H 'content-type: application/json' -d '{"query":"*","max_hits":1}' | jq '.num_hits'
curl -sS 'http://127.0.0.1:8123/?user=neuradex&password=neuradex-local-secret' \
  --data-binary 'SELECT count() FROM neuradex.events'
```

Expected: one Quickwit hit and one ClickHouse row. If the Quickwit hit is zero, wait for the
10-second commit; if it is still zero, check the Quickwit log for a mapping error (usually a
header sent as a string instead of an object).

## Failure injection

Run these in a disposable stack:

| Injection | Expected |
| --- | --- |
| `docker compose stop quickwit` | Events spool; API unaffected; restart catches up |
| `docker compose stop clickhouse` | Inserts fail/spool; API unaffected; restart catches up |
| `docker compose stop minio` | Content/archive writes fail or spool; API unaffected |
| Fill the content spool | Content shed and marked; events continue |
| Fill the event spool | Events shed after an alert; API continues |
| Flip one byte in a pack frame | Read returns 502; `content.read` failure; no bad bytes |
| Kill the pack builder mid-pack | Partial pack discarded; no upload |
| Set clock ±5 minutes | Keys unique; pack may seal early/late; no loss |
| Send `X-Forwarded-User` to the ingress | Header stripped; identity comes from the signed principal |

These are not automated yet. Add them to a `tests/compose` harness once the pipeline exists.

## CI

See [ci.md](ci.md) for the pipeline and gates. In short: `ci-success` is the required check; it
aggregates lint, both test matrix jobs, rustdoc, and shellcheck. The Kind deployment job skips
docs-only PRs; a PR that changes `compose.yaml` or `hack/` is not docs-only and will build the
image.
