# ADR-0001: Concurrency model

- **Status:** Accepted
- **Date:** 2026-06-12 (decision), 2026-06-13 (realized in M2)
- **Deciders:** Epiphany maintainers
- **Phase:** 0 (decision) / 2 (realization)

## Context
Epiphany is an in-memory store serving many concurrent readers *and* writers (planning is write-back heavy). Three constraints shape the choice:
- **Performance mandate:** reads must not stall behind writes.
- **Determinism mandate:** a read must see a consistent, reproducible snapshot.
- **Sandboxes** (Phase 6) need cheap per-user what-if overlays over a base.

## Decision (recommended)
**MVCC / copy-on-write snapshots.** Writers produce new versions; readers hold an immutable snapshot for the duration of a query. Sandboxes are a natural copy-on-write overlay over a base snapshot. We will prototype this early (Phase 1) because it likely shapes the whole engine.

## Alternatives considered
- **Single `RwLock` per cube:** the simplest option, but large writes or loads block all readers (the "server is locked" problem), which is the classic incumbent pain point.
- **Sharded locks:** better write concurrency, but readers still block on a shard, and snapshot consistency across shards is awkward.
- **MVCC / copy-on-write:** reads never block writes, snapshots are consistent and reproducible, sandboxes fall out naturally, and it enables deterministic parallel aggregation. The cost is more memory churn and implementation complexity.

## Consequences
- Enables the determinism and "reads never block" goals, and underpins sandboxes.
- **Atomic multi-cell write follows directly:** stage a batch of writes against a snapshot and publish one new version all-or-nothing; readers see the full batch or none of it (snapshot isolation), and a batch staged on a stale base is rejected or retried. Surfaced as a transactional batch cell-write in Phase 2.
- Requires care on memory (version retention and garbage collection) and a deterministic version ordering. Validate with concurrency stress tests and the determinism suite.
- **Realized in M2** as `epiphany-engine`: each cube is published as an immutable `Arc` behind `arc_swap::ArcSwap` (lock-free snapshot reads that never block writes); a write clones the cube, applies the validated batch, durably logs it, then atomically publishes the new version under a per-cube writer lock. Whole-cube clone-on-commit is the M2 cut (cheap at demo scale); a structural-sharing store is a benchmark-gated optimization behind the same handle (section 13).
