# ADR-0001: Concurrency model

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Epiphany maintainers
- **Phase:** 0 (decision) / 1 (implementation)

## Context
Epiphany is an in-memory store serving many concurrent readers *and* writers
(planning is write-back heavy). Three constraints shape the choice:
- **Performance mandate** — reads must not stall behind writes.
- **Determinism mandate** — a read must see a consistent, reproducible snapshot.
- **Sandboxes** (Phase 6) need cheap per-user what-if overlays over a base.

## Decision (recommended)
**MVCC / copy-on-write snapshots.** Writers produce new versions; readers hold an
immutable snapshot for the duration of a query. Sandboxes are natural COW
overlays over a base snapshot. To be prototyped early (Phase 1) because it likely
shapes the whole engine.

## Alternatives considered
- **Single `RwLock` per cube** — simplest; but big writes/loads block all readers
  ("server is locked"), the classic incumbent pain point.
- **Sharded locks** — better write concurrency; still blocks readers on a shard,
  and snapshot consistency across shards is awkward.
- **MVCC/COW** — reads never block writes; consistent, reproducible snapshots;
  sandboxes fall out naturally; enables deterministic parallel aggregation. Cost:
  more memory churn and implementation complexity.

## Consequences
- Enables the determinism and "reads never block" goals; underpins sandboxes.
- Requires care on memory (version retention/GC) and on a deterministic version
  ordering. Validate with concurrency stress tests + the determinism suite.
