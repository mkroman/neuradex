# Deploying with Helm

The chart in `chart/neuradex` runs the service as a stateless Deployment. Audit output and
content capture are optional and off (or stdout-only) by default. This page covers the
Kubernetes-specific wiring; [configuration.md](../reference/configuration.md) documents the
service's environment variables, and [runbook.md](runbook.md) covers failure handling.

## Minimal install

Create the Kagi secret first. Do not put the token in `values.yaml` in production.

```sh
kubectl create namespace neuradex

kubectl -n neuradex create secret generic neuradex-kagi \
  --from-literal=kagi-session-token="$KAGI_SESSION_TOKEN"

helm install neuradex chart/neuradex -n neuradex \
  --set existingSecret=neuradex-kagi
```

`--set existingSecret=...` disables the chart-managed Kagi secret. The chart still validates its
own values, but CI renders with `--set kagiSessionToken=ci-dummy`; use the same flag for
`helm lint`/`helm template` checks that do not need a real token.

The default NetworkPolicy allows ingress to the service port and egress to DNS, 80, and 443. The
service talks to Kagi over HTTPS, so the minimal install needs no extra egress.

## Audit output

Audit events are JSON Lines. With `audit.logPath` empty (the default, or `-`) they go to stdout,
independently of `RUST_LOG` and `LOG_FORMAT`. The chart sets `LOG_FORMAT=json` by default, so a cluster log
pipeline receives one JSON object per line for both tracing and audit records; route on the
audit event shape if the pipeline needs to separate them.

To hash identifier and IP headers instead of redacting them, supply an HMAC key:

```sh
kubectl -n neuradex create secret generic neuradex-audit \
  --from-literal=audit-hmac-key="$(openssl rand -hex 32)"

helm upgrade neuradex chart/neuradex -n neuradex \
  --reuse-values \
  --set audit.existingSecret=neuradex-audit
```

The chart's default key name is `audit-hmac-key`; override it with `audit.hmacKeyKey`. Without a
key, those headers are redacted. The service never logs the key.

Writing to a file instead of stdout requires a writable volume and an existing parent directory,
because the container root filesystem is read-only:

```sh
helm upgrade neuradex chart/neuradex -n neuradex \
  --reuse-values \
  --set audit.logPath=/var/log/neuradex/audit.jsonl \
  --set extraVolumes[0].name=audit \
  --set extraVolumes[0].emptyDir={} \
  --set extraVolumeMounts[0].name=audit \
  --set extraVolumeMounts[0].mountPath=/var/log/neuradex
```

An `emptyDir` is lost on restart; use a PVC or stdout for durable audit. The service appends to
the file and does not rotate it, so file-based audit needs an external shipper and rotation
policy.

## Event pipeline

The optional event pipeline ships audit events to Quickwit, ClickHouse, and an S3-compatible
archive. It is a separate deployment: the upstream Vector Helm chart runs a DaemonSet, and this
repository provides only the Vector configuration and chart values in `chart/event-pipeline`.
See [the deployment README](../../chart/event-pipeline/README.md) and
[RFD 0010](../rfd/0010-production-event-pipeline.md).

It requires audit events on stdout. Do not set `audit.logPath` on a release using the pipeline.

```sh
kubectl -n neuradex create secret generic neuradex-clickhouse \
  --from-literal=clickhouse-password="$CLICKHOUSE_PASSWORD"
kubectl -n neuradex create secret generic neuradex-events \
  --from-literal=access-key-id="$B2_EVENTS_KEY_ID" \
  --from-literal=secret-access-key="$B2_EVENTS_APP_KEY"

helm upgrade --install event-pipeline chart/event-pipeline \
  -n neuradex
```

Edit the endpoints, archive bucket, and `serviceSelector` in
[`chart/event-pipeline/values.yaml`](../../chart/event-pipeline/values.yaml) first. The chart
renders the values through its `customConfig`; there is no separate ConfigMap step. The collector
has no Service or Ingress and runs as root because node log files are root-owned. It uses bounded
disk buffers and delivers at-least-once; deduplicate by `event_id` when querying. The `events/`
archive prefix is covered by the retention policy.

## Content store

`content.store` is `off` by default. It captures fetch response bodies, peek head bytes, and
search/Open WebUI request and response JSON. The request still succeeds if content capture
fails; the failure is logged and the content reference is omitted from the audit event.

### S3-compatible bucket (recommended)

```sh
kubectl -n neuradex create secret generic neuradex-content \
  --from-literal=access-key-id="$B2_KEY_ID" \
  --from-literal=secret-access-key="$B2_APP_KEY"

helm upgrade neuradex chart/neuradex -n neuradex \
  --reuse-values \
  --set content.store=s3 \
  --set content.s3.endpoint=https://s3.us-west-004.backblazeb2.com \
  --set content.s3.region=us-west-004 \
  --set content.s3.bucket=neuradex-content \
  --set content.s3.existingSecret=neuradex-content
```

`content.s3.prefix` is optional. Path-style addressing is on by default and is required by B2
and MinIO. For an in-cluster MinIO endpoint, allow its port in the NetworkPolicy:

```yaml
extraNetworkPolicyEgress:
  - to:
      - podSelector:
          matchLabels:
            app.kubernetes.io/name: minio
    ports:
      - port: 9000
        protocol: TCP
```

Adjust the selector to the labels your MinIO workload actually uses. Use HTTPS for any endpoint
outside the cluster; plain HTTP is only for local MinIO.

### Local directory

Local content is for a single replica or a volume that supports `ReadWriteMany`. The default
`ReadWriteOnce` PVC cannot be mounted by pods on different nodes, so do not combine it with an
HPA that can scale past one replica.

```sh
helm upgrade neuradex chart/neuradex -n neuradex \
  --reuse-values \
  --set replicaCount=1 \
  --set content.store=local \
  --set content.local.persistence.enabled=true \
  --set content.local.persistence.size=100Gi
```

The chart creates a PVC and mounts it at `content.local.path`. With
`content.local.persistence.enabled=false` it mounts an `emptyDir` instead; that content is lost
whenever the pod restarts or moves. An existing claim can be used with
`content.local.persistence.existingClaim` (and `enabled=true`).

### Policy defaults

| Value | Default | Meaning |
| --- | --- | --- |
| `content.packTargetBytes` | `134217728` | Target plaintext bytes per pack |
| `content.maxPackAgeSecs` | `600` | Open-pack age backstop; the service flushes every 5 seconds |
| `content.maxBodyBytes` | `26214400` | Maximum captured body bytes |
| `content.retentionDays` | `365` | Policy value; the service does not delete objects |
| `content.compressionLevel` | `3` | zstd level |

## Retention

The service records `CONTENT_RETENTION_DAYS` but never deletes packs or objects. Retention is
enforced by bucket lifecycle rules. `neuradex-admin content-lifecycle` generates the B2 policy
for the prefixes the content plane uses now and the stores it will use later:

| Prefix | Contents |
| --- | --- |
| `packs/` | Content packfiles |
| `objects/` | Large standalone bodies |
| `index/` | Future hourly request index |
| `events/` | Future event archive |

Use the same value as `content.retentionDays` (the chart sets `CONTENT_RETENTION_DAYS` from it;
default 365). `CONTENT_S3_PREFIX`, when set, nests the same four prefixes under it.

### B2 hide/delete pairing

B2's S3 API maps `Expiration` to **hide**, not delete. A hidden version still consumes storage
until a noncurrent-version rule deletes it. B2 also requires each `Expiration.Days` rule to be
paired with a rule that has the same prefix and `Expiration.ExpiredObjectDeleteMarker: true`.
The script emits three rules per prefix (12 total):

| Rule | S3 field | B2 action |
| --- | --- | --- |
| Hide the current version | `Expiration.Days = CONTENT_RETENTION_DAYS` | Hide |
| Remove orphan delete markers | `Expiration.ExpiredObjectDeleteMarker = true` | Remove delete markers |
| Delete the hidden version | `NoncurrentVersionExpiration.NoncurrentDays = 1` | Delete |

The noncurrent delay is one day. B2 runs lifecycle rules once per day and requires a positive
delay, so deletion happens roughly one to two days after the retention window closes. Lifecycle
is a safety net, not a precise deletion SLA. `AbortIncompleteMultipartUpload` is omitted because
the service uploads packs and objects with single PUTs.

The four prefixes do not overlap, which B2's S3 API requires. Do not add a bucket-wide (`""`)
or parent-prefix rule: it would overlap every managed prefix. The policy is generated, not
hard-coded, so a retention change does not require editing a checked-in JSON file.

### Apply the policy

```sh
export CONTENT_RETENTION_DAYS=365
export CONTENT_S3_BUCKET=neuradex-content
export CONTENT_S3_ENDPOINT=https://s3.us-west-004.backblazeb2.com
export CONTENT_S3_REGION=us-west-004
export CONTENT_S3_ACCESS_KEY_ID=...
export CONTENT_S3_SECRET_ACCESS_KEY=...
export CONTENT_S3_FORCE_PATH_STYLE=true   # optional; default true
export CONTENT_S3_PREFIX=neuradex         # optional; must match the service

cargo run -p neuradex-admin -- content-lifecycle print > lifecycle.json
cargo run -p neuradex-admin -- content-lifecycle check
cargo run -p neuradex-admin -- content-lifecycle apply
```

`print` needs no AWS access and writes the JSON policy to stdout for review. `check` and `apply`
read the same `CONTENT_S3_*` settings as the service and use the AWS SDK directly; they do not
need the AWS CLI. `apply` verifies that bucket versioning is enabled, then replaces the bucket's
entire lifecycle configuration. Exit status is 0 for success or in sync and 1 for drift or an
operation failure; clap uses 2 for usage errors.

Prerequisites and access:

- Versioning must be enabled on the bucket. `--check` and `--apply` verify it.
- Use a separate lifecycle-admin key, not the service's content key. B2 exposes lifecycle reads
  through bucket metadata and gates writes behind `writeBuckets`, which is not available to
  bucket-restricted keys; versioning reads need `readBuckets`. Keep that key away from the data
  plane.
- The bucket must exist before the policy is applied.

**The S3 API is the single writer.** Do not create, update, or delete these rules in the B2 web
console or through the B2 Native API (`b2_update_bucket`). B2 regenerates rule IDs when either is
used, and a second writer can silently drop rules the policy expects. `--check` ignores IDs but
not rule content; any drift is a reviewed change, not an in-place edit.

B2 is the target. A local MinIO can exercise the AWS CLI path, but MinIO does not reproduce
B2's hide-then-delete mapping; do not use it to validate the B2 deletion semantics.

Never delete a pack by hand to remove one request: packs are immutable and shared, and the
audit event is the durable record. See
[RFD 0002](../rfd/0002-content-addressed-storage.md) and
[capacity-and-cost.md](capacity-and-cost.md) for the retention policy.

Quickwit retention (30 days) lives in the index mapping, and ClickHouse retention (365 days)
lives in the table TTL. The service does not write to either store; the optional local Vector
profile is a development bridge, not a production collector.

## Verify a rollout

```sh
kubectl -n neuradex rollout status deployment/neuradex
kubectl -n neuradex port-forward svc/neuradex 8080:80

curl -i http://127.0.0.1:8080/healthz          # 204
curl -sS -X POST http://127.0.0.1:8080/v1/search \
  -H 'content-type: application/json' \
  -d '{"query":"test","limit":3}'
```

With audit on stdout, the search should produce one JSONL audit line in the pod log. With
content enabled, wait for the 5-second flush, then list the configured bucket under `packs/` to
confirm a pack; no per-request object should appear.
