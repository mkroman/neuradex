---
rfd: 8
title: Capacity, cost, and retention
authors:
  - neuradex maintainers
state: discussion
discussion: https://github.com/mkroman/neuradex/pull/70
labels:
  - operations
  - storage
  - cost
created: 2026-11-10
updated: 2026-11-10
---

# RFD 8: Capacity, cost, and retention

> **State: discussion.** The 365-day retention is the initial operating policy, but the cost
> model rests on an unmeasured 4 MiB average body size. This RFD is not a commitment to spend
> ~$20k/month. It is a model, a set of risks, and a measurement gate.

## Summary

At 40,000 searches/hour and 100,000 fetches/hour, with a **guesstimated 4 MiB average fetch
body**, content storage reaches roughly **9.15 TiB/day**, **3.34 PiB at 365-day steady state**,
and about **$20k/month** on B2 at $6/TiB-month. Packfiles keep the object count to ~75,000/day
instead of 2.4 million requests/day. Event archives, the request index, Quickwit, and
ClickHouse are small by comparison. The dominant uncertainty is the average body size; a 512 KiB
average would reduce the steady state to ~418 TiB and ~$2.5k/month. Measure before committing to
a year of content.

## Context and scope

The retention decision (365 days initially) was made before we had production body-size
measurements. The workload numbers are estimates:

- 40,000 searches/hour;
- 100,000 fetches/hour;
- average fetch body ~4 MiB (a guesstimate, not measured);
- 25 MiB maximum captured body;
- 128 MiB pack target, 10 minute maximum pack age;
- no immediate per-request deletion.

This RFD models storage, object count, event volume, network, and CPU, and proposes the
measurement that turns the model into a decision. It covers B2, Quickwit, ClickHouse, and the
event archive. It does not cover compute instance pricing, egress from Kagi, or salaries.

Out of scope: a formal FinOps process, reserved-capacity discounts, and multi-region
replication.

## Goals and non-goals

| Goal | How we know it is met |
| --- | --- |
| A transparent cost model | Every number has an assumption and a formula |
| A sensitivity table | The cost at 0.25, 0.5, 1, 2, 4, and 8 MiB averages is visible |
| A measurement gate | A 7-day staging measurement decides whether 365 days is affordable |
| Retention options | 30/90/180/365 days are compared |
| Object-count sanity | Pack count/day and PUT rate are modelled |
| A review date | The model is revisited after one month of real data |

Non-goals:

- Predicting the bill to the dollar. B2 pricing, compression, dedupe, and average body size vary.
- Justifying retention on compliance grounds. There is no stated legal requirement yet.
- A cost-optimization implementation (sampling, tiering). Those are options, not decisions.

## Options

### Retention

| Option | Benefits | Drawbacks | 365-day steady state at 4 MiB | Monthly B2 at $6/TiB |
| --- | --- | --- | --- | --- |
| A. 30 days | Cheapest; matches Quickwit | Too short for quarterly investigations; content disappears before events | ~275 TiB | ~$1.6k |
| B. 90 days | Covers a quarter; much cheaper | Still short for annual review; events outlive content | ~824 TiB | ~$4.9k |
| C. 180 days | Half-year investigations | Cost is half of a year; still not annual | ~1,648 TiB | ~$9.9k |
| D. 365 days (initial policy) | Full annual window; matches ClickHouse TTL | Highest cost; 4 MiB is unmeasured | ~3,342 TiB | ~$20k |
| E. 365 days, sample non-2xx / large bodies | Cuts cost while keeping successes | Sampling policy is complex; audit completeness suffers | Depends on rate | Depends |
| F. 365 days, 512 KiB average (optimistic) | Affordable | Not a policy; a different assumption | ~418 TiB | ~$2.5k |

The initial policy is **D**. The discussion is whether D survives the measurement gate. Option E
is the fallback if the average is high and the budget is fixed; it requires a separate privacy
and audit review because missing bodies are a completeness risk.

### Pack target

| Option | Benefits | Drawbacks | Verdict |
| --- | --- | --- | --- |
| 64 MiB | Lower loss blast radius; more parallel uploads | 2× the objects and PUTs; more footers | Rejected |
| 128 MiB | One PUT per ~1.2 s at 4 MiB; manageable object count | A lost pack loses up to 128 MiB | **Chosen** |
| 256 MiB | Half the objects | Longer upload; larger loss blast radius; more local spool | Rejected for now |
| 1 GiB | Fewer objects | Very large PUT; slow retries; poor for small packs at low traffic | Rejected |

### Average-body assumption

| Average body | Content/day | 365-day steady state | Monthly B2 at $6/TiB | Packs/day |
| --- | --- | --- | --- | --- |
| 256 KiB | 0.57 TiB | ~209 TiB | ~$1.3k | ~4,688 |
| 512 KiB | 1.14 TiB | ~418 TiB | ~$2.5k | ~9,375 |
| 1 MiB | 2.29 TiB | ~835 TiB | ~$5.0k | ~18,750 |
| 2 MiB | 4.58 TiB | ~1,671 TiB | ~$10.0k | ~37,500 |
| **4 MiB (assumed)** | **9.15 TiB** | **~3,342 TiB** | **~$20.0k** | **~75,000** |
| 8 MiB | 18.31 TiB | ~6,683 TiB | ~$40.1k | ~150,000 |

The relationship is linear: monthly B2 cost ≈ `100,000 × 24 × avg_MiB × 365 / 1,048,576 × $6`.
Use the measured average, not this table, for a budget.

## Determination

**Provisional:** keep 365 days for the first operating period, with a hard measurement gate.

1. Collect 7 days of staging data: `content_bytes` distribution (p50, p90, p99), capture rate,
   truncation rate, compression ratio, duplicate rate, and actual fetches/hour.
2. Recompute the table with measured values. If the modelled steady-state B2 cost exceeds the
   agreed monthly budget, choose one:
   - reduce retention to 90 or 180 days;
   - lower the capture cap;
   - sample non-2xx/large bodies (requires audit review);
   - increase the budget.
3. Record the decision in this RFD and move it to `published`/`committed` as appropriate.

Until then, do not treat 365 days as an immutable requirement. Do not silently shorten it in
code either; the retention policy is configuration and must be visible.

Why 365 days initially:

- It matches the ClickHouse TTL, so the analytical and raw records have the same window.
- It covers an annual review cycle and avoids a content gap while the pipeline is new.
- We can shorten retention later without rewriting old data; lengthening it later cannot
  recover data that was already deleted.

The asymmetry favors starting long and measuring, provided the spend is visible and capped.

## Design

### Volume model

Assumptions:

- 100,000 fetches/hour, 24 hours/day, 30.4 days/month, 365 days/year.
- 4 MiB average captured body, before compression.
- zstd compression ratio 2.5–4× on HTML/text; the model uses raw captured bytes for B2 because
  pack compression is already applied and the 4 MiB is a *captured* (decompressed) average.
  The pack stores compressed frames, so the actual B2 bytes are lower than the raw model. The
  table is conservative if the 4 MiB is raw and compression is 3×; it is accurate if the 4 MiB
  is already the compressed average. **This ambiguity is part of the risk.**
- No global dedupe; within-pack dedupe only.
- 128 MiB pack target, 3,125 packs/hour at 4 MiB average.

| Quantity | Formula | At 4 MiB |
| --- | --- | --- |
| Captured bytes/day | `100,000 × 24 × 4 MiB` | 9,600,000 MiB = 9,375 GiB = 9.15 TiB |
| Captured bytes/year | `× 365` | 3,341.7 TiB = 3.26 PiB |
| Packs/day | `9,375 GiB / 0.125 GiB` | 75,000 |
| Packs/hour | `75,000 / 24` | 3,125 |
| Seconds per pack | `3,600 / 3,125` | 1.15 s |
| PUTs/day | one per pack | 75,000 |
| Object count at 365 days | packs + 8,760 index + 8,760 archive | ~27.4M packs + ~17.5k metadata |

The object count is dominated by packs, not events. A listing of a year prefix is not a normal
operation; use the request index and pack ID. Lifecycle rules operate by prefix/age, not by
listing.

### Event and index volume

| Stream | Rate | Size model | Per day | Per year |
| --- | --- | --- | --- | --- |
| Events | 140,000/h | 1.5 KiB raw | 5.16 GB raw | 1.88 TB raw |
| Event archive | same | 4× zstd | ~1.29 GB | ~471 GB |
| Request index | 2.4M fetch requests/day | 200 B/request | 672 MB raw | 61 GB compressed at 4× |
| Quickwit hot | 30 days | ~1.3 GB/day compressed + index | ~40 GB | n/a |
| ClickHouse | 3.36M rows/day | ~500 B/row raw, 5× | ~336 MB | ~123 GB |

Note: the request index covers fetch and peek requests with captured content. The model uses
2.4M/day (100k/h × 24) as an upper bound; if peek adds volume, adjust.

### Network and CPU

| Resource | At 4 MiB average | At 2× peak |
| --- | --- | --- |
| B2 upload | ~111 MiB/s (~0.93 Gbit/s) | ~222 MiB/s (~1.9 Gbit/s) |
| B2 PUT rate | ~0.87/s | ~1.7/s |
| zstd level 3 | ~0.2–0.6 core | ~0.4–1.2 cores |
| SHA-256 | ~0.1–0.2 core | ~0.2–0.4 core |
| BLAKE3 | ~0.02–0.06 core | ~0.04–0.12 core |

Network is the bottleneck, not hashing. The pack builder must not buffer 128 MiB in memory; use
a local spool file. At 4 MiB average, a pack fills in ~1.2 s, so the uploader must sustain
~0.87 PUTs/s and ~111 MiB/s. If the uploader falls behind, the spool grows; if the spool fills,
content is shed (events are kept). This is a deliberate degradation.

### Retention mechanics

| Store | Retention | Mechanism | Precision |
| --- | --- | --- | --- |
| B2 `packs/` | 365 days | Lifecycle rule | Daily; hours late is normal |
| B2 `index/` | 365 days | Lifecycle rule | Daily |
| B2 `events/` | 365 days | Lifecycle rule | Daily |
| B2 `cas/` | 365 days | Lifecycle rule | Daily |
| Quickwit | 30 days | `retention.period` + daily schedule | Daily |
| ClickHouse | 365 days | TTL + `ttl_only_drop_parts` | Part drop; can lag |
| Jaeger | process lifetime | In-memory | Immediate on restart |

The event archive and ClickHouse both cover 365 days, but they are independent. A ClickHouse
backfill can rebuild from the archive; an archive loss cannot be rebuilt from ClickHouse because
ClickHouse omits bodies and some fields.

### Cost table (B2 only)

Using $6/TiB-month and 365-day steady state at 4 MiB:

| Retention | Steady state | Monthly | Annual |
| --- | --- | --- | --- |
| 30 days | ~275 TiB | ~$1.65k | ~$19.8k (if never growing) |
| 90 days | ~824 TiB | ~$4.94k | ~$59.3k |
| 180 days | ~1,648 TiB | ~$9.89k | ~$118.6k |
| 365 days | ~3,342 TiB | ~$20.05k | ~$240.6k |

B2 bills in decimal TB, not TiB. The difference is ~10%: 3,342 TiB ≈ 3,675 TB, so a bill at
$6/TB would be ~$22k/month. Treat these as planning numbers, not invoices.

Not included: B2 transactions (PUT/GET/LIST), egress beyond the free allowance, Quickwit and
ClickHouse compute, and engineering time. At 75k PUTs/day and range GETs for investigations,
transaction cost is small relative to storage, but verify against the first invoice.

## Economic, capacity, and performance considerations

- **The 4 MiB assumption is the biggest risk.** It could be 10× too high or 4× too low. The
  model is linear; measure the distribution before committing budget.
- **The 25 MiB cap is not the average.** A few large pages do not dominate unless the
  distribution is heavy-tailed. Use p50/p90/p99, not the cap.
- **Compression is already in the pack.** If the 4 MiB is decompressed HTML, B2 stores less
  than the model; if it is already compressed, the model is accurate. Record which one is
  measured.
- **Dedupe is local.** Repeated pages across packs are stored again. A global index could reduce
  storage but adds a consistency problem; measure the duplicate rate first.
- **Pack count is high.** 75k packs/day is 27M/year. B2 handles it, but lifecycle, listing, and
  reconciliation must be prefix-scoped. Do not build a dashboard that lists all packs.
- **The 10-minute max age is not the trigger at this volume.** Size fills a pack in ~1.2 s.
  Max age protects low-traffic periods and stalled builders.
- **Events are cheap; content is expensive.** Do not optimize the event archive before
  measuring content. Event retention can stay 365 days even if content retention shrinks.

## Security considerations

- Longer retention increases the exposure window for personal data and third-party content.
  The 365-day policy needs a privacy review, not just a cost review.
- A reduction to 90 days is a security improvement and a cost improvement, but it may break
  investigations. Document the trade-off.
- Sampling non-2xx bodies is not neutral: errors are often the most valuable audit records.
  Require a security/audit review before enabling.
- B2 lifecycle deletion is irreversible. Restrict who can edit lifecycle rules and log changes.
- Cost dashboards must not expose query text or URLs. Use aggregates only.
- Per-request deletion is not supported. If a legal request requires it, the only path is a pack
  rewrite, which changes pack identity; this is a known limitation.

## Operational considerations

- Add a monthly cost review to the ops calendar. Compare the model with the B2 invoice and with
  ClickHouse's `content_daily` table.
- Alert on bytes/day exceeding the model by 25% for 3 consecutive days. That is either a traffic
  change or a capture bug.
- Alert on pack count/day exceeding the model by 50%. It may indicate small bodies or a seal
  bug.
- Measure the compression ratio per pack (`uncompressed_bytes / compressed_bytes`). A ratio near
  1 means the content is already compressed and the cost model is wrong.
- Measure the duplicate rate (same BLAKE3 seen more than once per day). If it is high, a global
  content index may pay for itself.
- Keep a 30-day cushion in the budget for backfills and investigations.
- Review retention with security and the budget owner, not only engineering.

### Measurement queries

```sql
-- Captured bytes and body-size distribution per day.
SELECT
    toDate(timestamp) AS day,
    count() AS fetches,
    sum(content_bytes) AS bytes,
    quantile(0.5)(content_bytes) AS p50,
    quantile(0.9)(content_bytes) AS p90,
    quantile(0.99)(content_bytes) AS p99,
    countIf(truncated) AS truncated
FROM neuradex.events
WHERE operation_type = 'fetch' AND content_reference_count > 0
GROUP BY day
ORDER BY day;

-- Duplicate content rate (same BLAKE3 seen more than once in a day).
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

## Open questions

| Question | Why it is open | Trigger / owner |
| --- | --- | --- |
| What is the real average fetch body size? | The 4 MiB figure is a guesstimate | 7-day staging measurement |
| Is 4 MiB decompressed or compressed? | Changes the B2 estimate by 2.5–4× | Record the measurement method |
| Is 365 days affordable? | Depends on the measurement and budget | Budget owner after the gate |
| Do we sample non-2xx bodies if cost is high? | Errors are valuable; sampling is a completeness risk | Security/audit review |
| Do we need a global dedupe index? | Could cut cost; adds consistency work | Measure duplicate rate |
| Should pack target be 64 or 256 MiB? | Affects loss blast radius and PUT rate | Load test |
| What is the production egress pattern? | Investigations are range GETs; bulk replay is different | First month of B2 invoice |
| Is there a legal retention requirement? | None stated; 365 days is operational, not legal | Legal/security review |

## References

- [RFD 0002: Content-addressed storage](0002-content-addressed-storage.md)
- [RFD 0003: Hashing](0003-hashing.md)
- [RFD 0004: Storage and analytics](0004-storage-and-analytics.md)
- [RFD 0009: Access and read audit](0009-access-and-read-audit.md)
- [Operations capacity and cost](../operations/capacity-and-cost.md)
- [ClickHouse schema](../reference/clickhouse-schema.md)
- B2 pricing: <https://www.backblaze.com/cloud-storage/pricing>
