# Event pipeline chart

This is an umbrella Helm chart for the production audit event pipeline from
[RFD 0010](../../docs/rfd/0010-production-event-pipeline.md). It has no Kubernetes resources of
its own: the upstream [Vector chart](https://vector.dev/docs/setup/installation/package-managers/helm/)
is vendored under `charts/`, and `values.yaml` supplies its `vector.*` values, including the
audit filter and sinks through `customConfig`.

The upstream chart supplies the ConfigMap, DaemonSet, ServiceAccount, RBAC, host log mounts, and
`/var/lib/vector` data directory. This repository owns only the data-plane configuration.

## Prepare

Edit `values.yaml` (under `vector:`):

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
helm upgrade --install event-pipeline chart/event-pipeline -n neuradex
```

There is no separate ConfigMap step: the upstream chart renders `customConfig` into its own
ConfigMap and mounts it at `/etc/vector`.

## Update the upstream chart

The Vector chart tarball is vendored to keep installs reproducible and avoid a chart repository
dependency at deploy time. Update it with:

```sh
helm pull vector/vector --version <version> --destination chart/event-pipeline/charts
```

Then update `Chart.yaml` and `values.yaml` (`vector.image.tag`) in the same change.

## Validate and verify

The CI Chart Lint job installs Vector with `hack/install-tools.sh` and validates the rendered
ConfigMap. The equivalent local check is:

```sh
helm template event-pipeline chart/event-pipeline \
  --show-only charts/vector/templates/configmap.yaml > /tmp/vector-configmap.yaml
# extract the vector.yaml block, then:
CLICKHOUSE_PASSWORD=test vector validate --no-environment /tmp/vector.yaml
```

After a request, the event should appear in Quickwit, ClickHouse, and under `events/` in the
archive bucket with the same `request_id`. Delivery is at-least-once; deduplicate by `event_id`
when querying.

## Retention

The `events/` prefix is covered by `neuradex-admin content-lifecycle` (365 days by default). If
you change `archive.prefix` in `values.yaml`, change the lifecycle policy in the same review.
