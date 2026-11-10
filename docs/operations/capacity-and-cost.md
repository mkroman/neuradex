# Capacity and cost

The full model and its assumptions live in
[RFD 0008](../rfd/0008-capacity-cost-and-retention.md). This page is the operating view: what to
measure, what to alarm on, and how to change retention without losing data.

## Model at a glance

Assumptions: 100,000 fetches/hour, 40,000 searches/hour, 4 MiB average captured body, 365-day
retention, B2 at $6/TiB-month. The ClickHouse `content_bytes` and `content_blake3` columns are
derived by Vector from `content_references`, not fields of `AuditEvent`. The service flushes
content every 5 seconds; at the modelled volume a 128 MiB pack fills in ~1.2 seconds, so the
model is size-bound, not flush-bound. At low traffic the 5-second flush creates smaller packs.

| Quantity | Value |
| --- | --- |
| Captured content/day | ~9.15 TiB |
| Captured content/year (steady state) | ~3.34 PiB |
| Pack count/day at 128 MiB | ~75,000 |
| Pack count/hour | ~3,125 (one every ~1.2 s) |
| Event rows/day | 3.36 million |
| Event archive/year | ~470 GB compressed |
| Request index/year | ~60 GB compressed |
| Quickwit hot (30 days) | ~40 GB compressed + index |
| ClickHouse (365 days) | ~60–120 GB compressed |
| B2 monthly at steady state | ~$20k (TiB) / ~$22k (decimal TB) |

The 4 MiB average is a **guesstimate**. Measure it before treating the bill as committed.

## Sensitivity

| Average body | Content/day | 365-day steady state | B2/month at $6/TiB | Packs/day |
| --- | --- | --- | --- | --- |
| 256 KiB | 0.57 TiB | ~209 TiB | ~$1.3k | ~4,700 |
| 512 KiB | 1.14 TiB | ~418 TiB | ~$2.5k | ~9,400 |
| 1 MiB | 2.29 TiB | ~835 TiB | ~$5.0k | ~18,750 |
| 2 MiB | 4.58 TiB | ~1,671 TiB | ~$10.0k | ~37,500 |
| 4 MiB | 9.15 TiB | ~3,342 TiB | ~$20.0k | ~75,000 |
| 8 MiB | 18.31 TiB | ~6,683 TiB | ~$40.1k | ~150,000 |

Formula: `monthly ≈ 100,000 × 24 × avg_MiB × 365 / 1,048,576 × $6`.

## Measure this first

Run for 7 days in staging, then compute:

```sql
-- Body-size distribution and capture rate.
SELECT
    toDate(timestamp) AS day,
    count() AS fetches,
    countIf(content_reference_count > 0) AS captured,
    countIf(truncated) AS truncated,
    sum(content_bytes) AS bytes,
    quantile(0.5)(content_bytes) AS p50,
    quantile(0.9)(content_bytes) AS p90,
    quantile(0.99)(content_bytes) AS p99,
    max(content_bytes) AS max
FROM neuradex.events
WHERE operation_type = 'fetch'
GROUP BY day
ORDER BY day;
```

```sql
-- Duplicate content rate: how much a global dedupe index could save.
SELECT
    day,
    count() AS distinct_hashes,
    sum(hits) AS total_frames,
    sum(hits) - count() AS duplicate_frames
FROM (
    SELECT toDate(timestamp) AS day, content_blake3, count() AS hits
    FROM neuradex.events
    WHERE operation_type = 'fetch' AND content_reference_count > 0
    GROUP BY day, content_blake3
)
GROUP BY day
ORDER BY day;
```

```sql
-- Compression ratio is not in ClickHouse; read it from pack metadata or the uploader metric.
-- uncompressed_bytes / compressed_bytes per sealed pack.
```

Also measure:

- actual fetches/hour and peak/average ratio;
- percentage of non-2xx or empty bodies;
- peek volume, if captured;
- B2 PUT/GET request counts and egress;
- average pack fill time and seal reason (size vs age);
- content capture errors by type.

## Cost composition

| Component | Share at 4 MiB | Notes |
| --- | --- | --- |
| B2 storage | dominant | Linear in average body and retention |
| B2 transactions | small | ~75k PUTs/day; range GETs for reads |
| B2 egress | small | Free up to 3× stored/month; range GETs keep reads low |
| Quickwit compute/disk | small | 30-day hot window |
| ClickHouse compute/disk | small | 365-day typed events, no bodies |
| Service compute | moderate | zstd/hash/network, not modelled here |

A B2 invoice may differ by ~10% because B2 bills in decimal TB, not TiB. Treat the model as
planning, not accounting.

## Alerts and thresholds

| Signal | Threshold | Action |
| --- | --- | --- |
| Captured bytes/day | > 125% of 7-day model for 3 days | Check body-size distribution and capture bugs |
| Pack count/day | > 150% of model | Check average body/seal reason; small bodies create more packs |
| Compression ratio | < 1.5 for a day | Content may already be compressed; revisit the model |
| Duplicate rate | > 20% for a day | A global dedupe index may be worth it |
| B2 4xx/5xx | > 1% for 15 min | Credentials/outage; see runbook |
| Lifecycle deletions | > 10% above model | Retention bug; stop and investigate |
| Spool depth | > 80% for 5 min | Capacity/backpressure |
| ClickHouse disk | > 80% | TTL/partition check; expand |

## Changing retention

Retention is a policy change, not a code tweak. The order matters:

1. Write the proposal: current usage, requested window, cost delta, security/privacy impact.
2. Get approval from the service owner, security, and the budget owner.
3. Change the B2 lifecycle rules, ClickHouse TTL, and Quickwit retention as one coordinated
   change. A mismatch leaves orphan data or a lookup gap.
4. Record the change in RFD 0008 (or a new RFD) and the operations log.
5. Verify with object counts/ages and ClickHouse partition metadata.
6. Do not shorten retention and then lengthen it; deleted data cannot be recovered.

Current policy: 365 days content/events/index/archive; 30 days Quickwit. `CONTENT_RETENTION_DAYS`
only records the content policy value; the service does not delete objects, so B2/S3 lifecycle
rules are the enforcement point. Per-request deletion is not supported. A legal deletion request
needs a pack rewrite, which is not implemented.

## Capacity planning

| Resource | Headroom rule |
| --- | --- |
| B2 | Keep 30 days of budget cushion for backfills/investigations |
| Spool disk | ≥ 24 hours of content at peak, or explicit shedding policy |
| ClickHouse | ≥ 30% free disk for merges/TTL |
| Quickwit | ≥ 30% free disk for splits/merges |
| Network | ≥ 2× average upload bandwidth for peak |
| CPU | zstd is the largest cost; leave 2 cores for the pipeline at peak |

At 4 MiB average, content upload is ~111 MiB/s average and ~222 MiB/s at 2× peak. If the host
cannot sustain that, content writes queue in temporary files; if the disk fills, content is
lost for new requests. Audit events remain independent.

## Monthly review

- Compare the B2 invoice with the model.
- Recompute the body-size distribution and duplicate rate.
- Check the pack seal reason: if age dominates, traffic is lower than modelled; if size
  dominates, it is higher.
- Review retention with security and the budget owner.
- Update [RFD 0008](../rfd/0008-capacity-cost-and-retention.md) if the numbers or assumptions
  changed.

## Open questions

See [RFD 0008](../rfd/0008-capacity-cost-and-retention.md#open-questions). The main one is
whether 365 days survives the first measurement; do not silently shorten it in code.
