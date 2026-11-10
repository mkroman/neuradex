# Event pipeline deployment

This directory is the production audit event pipeline from
[RFD 0010](../../docs/rfd/0010-production-event-pipeline.md). It is a single values file for the
upstream [Vector Helm chart](https://vector.dev/docs/setup/installation/package-managers/helm/).
The chart supplies the DaemonSet, ServiceAccount, RBAC, host log mounts, and ConfigMap; this
repository supplies only the audit filter and sink configuration via the chart's native
`customConfig`.

## Prepare

Edit `values.yaml`:

- `serviceSelector` must match the neuradex service pods (label selector, not the pipeline
  release name);
- `quickwit`, `clickhouse`, and `archive` are the sink endpoints and object keys;
- the secret names/keys in `env` must exist in the namespace.

```sh
kubectl -n neuradex create secret generic neuradex-clickhouse \
  --from-literal=clickhouse-password="$CLICKHOUSE_PASSWORD"
kubectl -n neuradex create secret generic neuradex-events \
  --from-literal=access-key-id="$B2_EVENTS_KEY_ID" \
  --from-literal=secret-access-key="$B2_EVENTS_APP_KEY"
```

## Install

```sh
helm repo add vector https://helm.vector.dev
helm repo update
helm upgrade --install vector vector/vector \
  -n neuradex \
  --version 0.61.0 \
  -f deploy/event-pipeline/values.yaml
```

There is no separate ConfigMap step: the chart renders `customConfig` into its own ConfigMap and
mounts it at `/etc/vector`. The chart also creates the read-only pod/namespace RBAC and mounts
the node log and `/var/lib/vector` paths.

## Validate and verify

The CI Chart Lint job installs Vector with `hack/install-tools.sh` and validates the rendered
ConfigMap. The equivalent local check is:

```sh
helm template vector vector/vector --version 0.61.0 \
  -f deploy/event-pipeline/values.yaml \
  --show-only templates/configmap.yaml > /tmp/vector-configmap.yaml
# extract the vector.yaml block, then:
CLICKHOUSE_PASSWORD=test vector validate --no-environment /tmp/vector.yaml
```

After a request, the event should appear in Quickwit, ClickHouse, and under `events/` in the
archive bucket with the same `request_id`. Delivery is at-least-once; deduplicate by `event_id`
when querying.

## Retention

The `events/` prefix is covered by `neuradex-admin content-lifecycle` (365 days by default). If
you change `archive.prefix` in `values.yaml`, change the lifecycle policy in the same review.
