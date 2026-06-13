# ADR-0001: Concurrency model

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Epiphany maintainers
- **Phase:** 0 (decision) / 1 (implementation)

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
- Requires care on memory (version retention and garbage collection) and a deterministic version ordering. Validate with concurrency stress tests and the determinism suite.
