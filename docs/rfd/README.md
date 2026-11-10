# Requests for Discussion (RFDs)

This repository records significant design decisions as **Requests for Discussion** (RFDs),
not ADRs. The process is adapted from Oxide Computer's RFD 1
([Requests for Discussion](https://rfd.shared.oxide.computer/rfd/0001)) and RFD 5
([Phases of Engineering](https://rfd.shared.oxide.computer/rfd/0005)).

An RFD is written to be read by the next engineer, not to satisfy a template. It records what
was known, which options were real, why the chosen one won, and what is still uncertain.

## When to write one

Write an RFD for a decision that is expensive to reverse or that future code will depend on:

- a storage format, hash algorithm, retention rule, or wire schema;
- a new crate boundary or a dependency direction;
- an access-control or audit requirement;
- a local infrastructure stack that other engineers will run;
- a capacity/cost commitment.

Skip an RFD for a local refactor, a bug fix, or a change that is fully described by an existing
RFD and its reference page.

## States

States are the six from Oxide RFD 1. This repository uses them unchanged.

| State | Meaning | Who merges |
| --- | --- | --- |
| `prediscussion` | A placeholder; the author is still shaping the idea and does not want review yet. | Author's branch only |
| `ideation` | A scoped topic description; no promise of active revision. Useful as a scratchpad. | Author's branch only |
| `discussion` | The RFD is ready for review; the PR is the discussion. | After review converges |
| `published` | The direction is agreed. The RFD is merged and remains the current record. | Merged |
| `committed` | The design is implemented. Changes should be rare and usually need a new RFD. | Merged |
| `abandoned` | The idea is deliberately not being pursued. Keep it for the history. | Merged |

A design decision in this repository is `published` when the PR that adds it merges. An
implemented item becomes `committed` in a follow-up PR that also updates the code. A
`discussion` RFD (currently [0008](0008-capacity-cost-and-retention.md)) is explicitly not a
commitment yet.

## Metadata

Every RFD starts with YAML front matter. Markdown is used instead of AsciiDoc, so the Oxide
attributes become YAML keys.

```yaml
---
rfd: 1
title: Audit event model
authors:
  - neuradex maintainers
state: published
discussion: TBD
labels:
  - audit
  - storage
created: 2026-11-10
updated: 2026-11-10
---
```

| Key | Required | Rules |
| --- | --- | --- |
| `rfd` | yes | Integer, matching the file name (`0001` → `1`). Never reuse a number. |
| `title` | yes | Short, specific, and stable. The file name and index must match. |
| `authors` | yes | List of author strings. Use `neuradex maintainers`; do not put personal names or email addresses in the file. |
| `state` | yes | One of the six states above. |
| `discussion` | yes | URL of the PR or issue where the RFD is discussed. Use `TBD` only before the PR exists. |
| `labels` | yes | List of lowercase labels. Reuse the canonical set below where possible. |
| `created` | yes | `YYYY-MM-DD`. |
| `updated` | yes | `YYYY-MM-DD`; bump on every substantive edit. |

Labels in use: `audit`, `storage`, `content`, `analytics`, `security`, `architecture`,
`operations`, `testing`, `local-development`, `process`.

## Body

Every RFD has these sections, in this order. Not every section needs the same depth, but none
should be silently omitted.

1. **Summary** — one paragraph a busy reader can act on.
2. **Context and scope** — the problem, the constraints, and what is out of scope.
3. **Goals and non-goals** — testable goals; explicit non-goals.
4. **Options** — a table of viable options, with benefits and drawbacks. Include "do nothing"
   when it is real.
5. **Determination** — the chosen option and why. State the reversibility and the blast radius.
6. **Design** — enough detail for an implementer: formats, fields, state machines, interfaces.
7. **Economic, capacity, and performance considerations** — numbers, assumptions, and the
   measurement that would change the answer.
8. **Security considerations** — data classification, access, encryption, retention, deletion.
9. **Operational considerations** — failure modes, observability, migration, rollback.
10. **Open questions** — unresolved decisions, each with an owner or a trigger.
11. **References** — prior RFDs, external docs, code paths.

For a small RFD, combine sections, but keep the options table and the determination.

## Lifecycle in this repository

1. Pick the next number. Create `docs/rfd/NNNN-slug.md` from
   [TEMPLATE.md](TEMPLATE.md).
2. Set `state: prediscussion` or `ideation` while writing. Iterate on your branch.
3. Set `state: discussion` and open a PR. The PR description is the discussion entry point.
4. Address review in the branch. Keep the options table honest; do not rewrite history to make
   the chosen option look inevitable.
5. When the discussion converges, set `state: published`, fill in the PR URL, and merge.
6. When the design is implemented, a follow-up PR sets `state: committed` and links the
   implementing code. If the implementation diverges, update the reference docs in the same PR
   or write a new RFD.

Do not edit a merged RFD to reverse a decision. Supersede it with a new RFD and add
`supersedes`/`superseded-by` links to both documents.

## Index

| RFD | Title | State | Owner area |
| --- | --- | --- | --- |
| [0001](0001-audit-event-model.md) | Audit event model | committed | audit schema |
| [0002](0002-content-addressed-storage.md) | Content-addressed storage | published | content plane |
| [0003](0003-hashing.md) | Hashing | committed | integrity / identity |
| [0004](0004-storage-and-analytics.md) | Storage and analytics | published | storage topology |
| [0005](0005-header-policy.md) | Header policy | committed | privacy / audit |
| [0006](0006-crate-boundaries.md) | Crate boundaries | committed | workspace |
| [0007](0007-local-testing-stack.md) | Local testing stack | committed | developer experience |
| [0008](0008-capacity-cost-and-retention.md) | Capacity, cost, and retention | discussion | operations / finance |
| [0009](0009-access-and-read-audit.md) | Access and read audit | published | security |

## References

- Oxide Computer, [RFD 1: Requests for Discussion](https://rfd.shared.oxide.computer/rfd/0001).
- Oxide Computer, [RFD 4: User Facing API](https://rfd.shared.oxide.computer/rfd/0004).
- Oxide Computer, [RFD 5: Phases of Engineering](https://rfd.shared.oxide.computer/rfd/0005).
- Oxide Computer, [RFD 63: Network Architecture](https://rfd.shared.oxide.computer/rfd/0063).
- [RFD template](TEMPLATE.md).
