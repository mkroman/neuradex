---
rfd: 0
title: Template
authors:
  - neuradex maintainers
state: prediscussion
discussion: TBD
labels:
  - process
created: 2026-01-01
updated: 2026-01-01
---

# RFD 0: Template

> Copy this file to `docs/rfd/NNNN-slug.md`, set the front matter, and delete this block.
> The RFD number must match the file name. Keep the options table and the determination even
> for a short RFD.

## Summary

One paragraph: what is being decided, for whom, and what changes if it is accepted.

## Context and scope

What problem are we solving? What constraints are real (traffic, retention, access, budget,
team size, existing systems)? What is explicitly out of scope? Link the code, issues, and prior
RFDs a reader needs.

## Goals and non-goals

| Goal | How we know it is met |
| --- | --- |
| … | … |

Non-goals:

- …
- …

## Options

| Option | Benefits | Drawbacks | Cost / risk |
| --- | --- | --- | --- |
| A. … | … | … | … |
| B. … | … | … | … |
| C. Do nothing | … | … | … |

State why options were ruled out before the table, if that is useful. Do not list strawmen.

## Determination

We choose **Option A**. The reasons are:

1. …
2. …

What this commits us to, and how reversible it is:

- Reversibility: …
- Blast radius: …
- What would make us revisit this: …

## Design

Enough detail to implement without guessing:

- data model and field names;
- formats and versioning;
- interfaces and crate boundaries;
- state machines and failure transitions;
- migration and rollout.

Use tables and Mermaid diagrams where they make the design clearer. Keep diagram sources
inline so they render on GitHub and in review.

## Economic, capacity, and performance considerations

| Quantity | Assumption | Result | Confidence |
| --- | --- | --- | --- |
| … | … | … | low / medium / high |

State the measurement that would change the decision. Separate measured numbers from guesses.

## Security considerations

- Data classification and what is sensitive.
- Authentication and authorization.
- Encryption in transit and at rest.
- Retention and deletion, including what cannot be deleted per request.
- Auditability of reads.
- Threats considered and accepted.

## Operational considerations

- Failure modes and degradation.
- Observability: signals, thresholds, and runbook links.
- Migration, backfill, and rollback.
- On-call impact and ownership.

## Open questions

| Question | Why it is open | Trigger / owner |
| --- | --- | --- |
| … | … | … |

## References

- `NNNN-slug.md` — related decision.
- Code: `crates/…`.
- External: …
