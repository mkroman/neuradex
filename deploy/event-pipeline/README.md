# Event pipeline deployment

This directory is a reference deployment of the production audit event pipeline from
[RFD 0010](../../docs/rfd/0010-production-event-pipeline.md). It uses the upstream
[Vector Helm chart](https://vector.dev/docs/setup/installation/package-managers/helm/) rather
than maintaining a custom DaemonSet, RBAC, or ConfigMap template.

- `vector.yaml` is the Vector configuration. It reads the selected neuradex pods, keeps audit
  events, and writes to Quickwit, ClickHouse, and an S3-compatible archive.
- `values.yaml` is the values file for the Vector chart. It mounts that config and injects the
  ClickHouse and archive credentials from existing Secrets.

## Prepare

Edit the endpoints and bucket in `vector.yaml` for the target cluster. The default label selector
matches a release named `neuradex`; change `extra_label_selector` if the release name differs.

```sh
kubectl -n neuradex create secret generic neuradex-clickhouse \
  --from-literal=clickhouse-password="$CLICKHOUSE_PASSWORD"
kubectl -n neuradex create secret generic neuradex-events \
  --from-literal=access-key-id="$B2_EVENTS_KEY_ID" \
  --from-literal=secret-access-key="$B2_EVENTS_APP_KEY"
```

## Install

```sh
kubectl -n neuradex create configmap neuradex-event-pipeline \
  --from-file=vector.yaml=deploy/event-pipeline/vector.yaml

helm repo add vector https://helm.vector.dev
helm repo update
helm upgrade --install vector vector/vector \
  -n neuradex \
  -f deploy/event-pipeline/values.yaml
```

The chart creates a DaemonSet, ServiceAccount, ClusterRole, and ClusterRoleBinding, and mounts
the node log and `/var/lib/vector` paths. The ConfigMap is mounted at `/etc/vector`.

## Validate and verify

```sh
docker run --rm -e CLICKHOUSE_PASSWORD=test \
  -e AWS_ACCESS_KEY_ID=test -e AWS_SECRET_ACCESS_KEY=test \
  -v "$PWD/deploy/event-pipeline/vector.yaml:/etc/vector/vector.yaml:ro" \
  timberio/vector:0.61.0-debian validate --no-environment /etc/vector/vector.yaml
```

After a request, the event should appear in Quickwit, ClickHouse, and under `events/` in the
archive bucket with the same `request_id`. The collector is at-least-once; deduplicate by
`event_id` when querying.

## Retention

The `events/` prefix is covered by `neuradex-admin content-lifecycle` (365 days by default). If
you change the archive prefix in `vector.yaml`, change the lifecycle policy in the same review.
