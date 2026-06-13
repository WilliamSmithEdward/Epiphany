# ADR-0002: Runtime persistence format

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Epiphany maintainers
- **Phase:** 0 (decision) / 1 (implementation)

## Context
The engine must survive restarts and crashes quickly. Note the split with
[ADR-0003](0003-model-as-code-serialization.md): the **model-as-code text is the
source of truth**; runtime persistence is a *fast-restart cache* of live data
(cell values, runtime state) that is always reconstructible.

## Decision (recommended)
A **custom append-only write-ahead transaction log (WAL) + periodic binary
snapshots.** Recovery = load the latest snapshot, replay the WAL tail. The engine
*is* the database; we own the format for performance and determinism.

## Alternatives considered
- **Embedded KV/DB (redb, sled, SQLite)** — less code, proven durability; but a
  poor fit for a sparse multidimensional cell store, opaque to our perf/format
  control, and an external dependency in the hottest path.
- **Custom WAL + snapshots** — full control over layout, fsync policy, and
  bulk-load fast paths; more code and more responsibility for correctness.

## Consequences
- Durability correctness is on us — covered by crash-recovery tests (kill at
  random points, assert identical recovered state) in Phase 1/8.
- fsync cadence is a perf/durability knob to tune against the §8 budgets.
