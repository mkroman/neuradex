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
| Audit unit | `cargo test -p neuradex-audit` | Event structs, enums, redaction, HMAC | seconds | implemented |
| Packfile unit | `cargo test -p neuradex-packfile` | Frame/footer/trailer, hashes, corruption | seconds | implemented |
| Content unit | `cargo test -p neuradex-content` | Pack writer, local/S3 store, policy, stored content | seconds | implemented |
| Lifecycle policy | `cargo test -p neuradex-admin` | 12 B2 rules, prefixes, retention, unique IDs | seconds | implemented |
| Service integration | `cargo test -p neuradex` | HTTP routes, extraction, errors | minutes | implemented |
| Compose smoke | `docker compose up -d` + curl/jq | Real Quickwit/ClickHouse/MinIO mappings | manual | implemented |
| Kind e2e | [`hack/e2e.sh`](#kind-end-to-end-e2e) | Chart install, fetch/peek/search, audit, S3 packs | minutes | implemented |
| Failure injection | stop a sink; corrupt a frame | Degradation and verification | manual | planned |

The first five rows are the Rust CI gates. They are also mise tasks (`mise run fmt`,
`mise run clippy`, `mise run test`, `mise run test-lean`, `mise run doc`); `mise run check`
runs those plus `deny` and `shellcheck`. The per-crate commands are faster iteration loops;
their tests also run under `cargo test --workspace`.

## Before a PR

Install mise first (see [local-development.md](local-development.md#prerequisites)). From the
repository root, in this order:

```sh
mise install --locked
mise run fmt
mise run clippy
mise run test
mise run test-lean
mise run doc
mise run deny
mise run shellcheck
```

`mise run check` runs the same seven tasks. For changes to the chart, deployment workflow, or
`hack/e2e.sh`, also run the [kind e2e suite](#kind-end-to-end-e2e). Each task wraps the raw
command CI uses (the Rust tasks add `--locked`); when a task fails, run the equivalent directly
for clearer output:

```sh
cargo fmt --all -- --check       # `cargo fmt` rewrites the files
cargo clippy --all-targets --workspace --locked -- -D warnings
cargo test --workspace --locked
cargo test -p neuradex --no-default-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
cargo deny --log-level warn --manifest-path Cargo.toml --all-features check advisories
shellcheck hack/*.sh
```

The `doc` task sets `RUSTDOCFLAGS="-D warnings"`, matching the rustdoc CI gate.

For docs/compose-only changes, the Rust commands are still run by CI; run the compose validation
locally:

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
[local-development.md](local-development.md#send-events-to-the-optional-stack)). That catches
Quickwit mapping and ClickHouse DDL drift that unit tests cannot.

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
| Content body over limit / store full | Append fails; no content reference; response unaffected |
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
| Audit JSONL write fails | Response still returns; `audit_sink` warning; event lost |
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
| Compose event | [local-development.md](local-development.md#send-events-to-the-optional-stack) | Manual end-to-end |
| E2E page/Kagi mock | `hack/e2e/fixtures` | Kind fetch/peek/search, audit, and content checks |

Do not commit real page bodies, real URLs, tokens, or user data. Use `example.com` and
synthetic HTML.

## Compose smoke test

Follow [local-development.md](local-development.md#run-the-service) to run the service against
the stack, then [send events to the optional stack](local-development.md#send-events-to-the-optional-stack)
and verify both sinks:

```sh
curl -sS -XPOST 'http://127.0.0.1:7280/api/v1/neuradex-events/search' \
  -H 'content-type: application/json' -d '{"query":"*","max_hits":1}' | jq '.num_hits'
curl -sS 'http://127.0.0.1:8123/?user=neuradex&password=neuradex-local-secret' \
  --data-binary 'SELECT count() FROM neuradex.events'
```

Expected: one Quickwit hit and one ClickHouse row. If the Quickwit hit is zero, wait for the
10-second commit; if it is still zero, check the Quickwit log for a mapping error (usually a
header sent as a string instead of an object).

## Kind end-to-end (e2e)

`hack/e2e.sh` installs the chart into a disposable kind cluster and runs a hermetic suite. It
deploys an nginx fixture service that serves a known page, a redirect, a binary response, and a
mock Kagi search flow, plus MinIO for the S3 content store. It then drives `/v1/fetch`,
`/v1/peek`, `/v1/search`, and `/v1/openwebui_search`; asserts the JSON bodies and error
envelopes; verifies the audit events (including no `/healthz` event and query hashing); and
checks that every content reference points at a real pack in MinIO.

Prerequisites: `docker`, `kind`, `kubectl`, `helm`, `curl`, and `jq`. Build and load the image,
create a cluster, then run the suite:

```sh
docker build -t ghcr.io/mkroman/neuradex:ci .
mise exec -- kind create cluster --name neuradex-e2e
mise exec -- kind load docker-image ghcr.io/mkroman/neuradex:ci --name neuradex-e2e
mise exec -- ./hack/e2e.sh
mise exec -- kind delete cluster --name neuradex-e2e
```

`E2E_KEEP=1 mise exec -- ./hack/e2e.sh` keeps the `neuradex-e2e` namespace after a successful
run; failures always leave it in place for inspection. `E2E_NAMESPACE`, `E2E_IMAGE`, and
`E2E_PORT` override the defaults. The suite needs no Kagi account: the fixture service is the
only search upstream, and the chart's NetworkPolicy already allows the in-cluster port-80
traffic.

## Failure injection

Run these in a disposable stack:

| Injection | Expected |
| --- | --- |
| `docker compose stop quickwit` | JSONL continues; Vector/Quickwit lag; API unaffected |
| `docker compose stop clickhouse` | JSONL continues; Vector/ClickHouse lag; API unaffected |
| `docker compose stop minio` | S3 content uploads fail; API unaffected |
| Fill the content disk | Content append/upload fails; no reference; API continues |
| Fill the audit-log disk | JSONL write fails; event lost; API continues |
| Flip one byte in a pack frame | Read returns 502; `content.read` failure; no bad bytes |
| Kill the pack builder mid-pack | Partial pack discarded; no upload |
| Set clock ±5 minutes | Keys unique; pack may seal early/late; no loss |
| Send `X-Forwarded-User` to the ingress | Header stripped; identity comes from the signed principal |

These are not automated yet. The pure failure paths (sink writes, append failures, corrupt
frames) already have unit tests; the highest-value stack cases belong in a `tests/compose`
harness.

## CI

See [ci.md](ci.md) for the pipeline and gates. In short: `ci-success` is the required check; it
aggregates lint, both test matrix jobs, rustdoc, and shellcheck. The Kind deployment job skips
docs-only PRs; a PR that changes `compose.yaml` or `hack/` is not docs-only and will build the
image and run the e2e suite.
