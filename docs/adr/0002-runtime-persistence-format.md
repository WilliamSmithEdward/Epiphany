# ADR-0002: Runtime persistence format

- **Status:** Accepted
- **Date:** 2026-06-12 (decision), 2026-06-13 (implemented)
- **Deciders:** Epiphany maintainers
- **Phase:** 0 (decision) / 1 (implementation)

## Context
The engine must survive restarts and crashes quickly. Note the split with [ADR-0003](0003-model-as-code-serialization.md): the model-as-code text is the source of truth, and runtime persistence is a fast-restart cache of live data (cell values, runtime state) that is always reconstructible.

## Decision (recommended)
A custom append-only write-ahead transaction log (WAL) plus periodic binary snapshots. Recovery loads the latest snapshot, then replays the WAL tail. The engine is the database, so we own the format for performance and determinism.

## Alternatives considered
- **An embedded key-value store or database** (redb, sled, SQLite): less code and proven durability, but a poor fit for a sparse multidimensional cell store, opaque to our performance and format control, and an external dependency in the hottest path.
- **A custom WAL plus snapshots:** full control over layout, fsync policy, and bulk-load fast paths, at the cost of more code and more responsibility for correctness.

## Consequences
- Durability correctness is on us. It is covered by crash-recovery tests (kill at random points, then assert identical recovered state) in Phase 1 and 8.
- fsync cadence is a performance-versus-durability knob to tune against the budgets in ROADMAP section 8.

## Phase 1 implementation note
The WAL is implemented as specified: a binary, append-only log in `epiphany-persist`. Each record is framed `[len][payload][crc32]` (little-endian), so a write torn by a crash is detected (the length runs past the file end, or the CRC fails) and the torn tail is discarded on recovery. The default fsync-per-write policy makes every acknowledged write survive a crash; it is a tunable knob.

The snapshot reuses the canonical model-as-code text (ADR-0003) as its payload rather than a bespoke binary format. The text serializer already round-trips a whole cube deterministically and is covered by its own tests, so reusing it avoids a second serialization path to keep correct, and the snapshot stays human-readable. A compact binary snapshot remains a benchmark-gated optimization for later (ROADMAP section 8) if snapshot size or load time demands it; the `Store` API does not change when that lands. The snapshot is written atomically (temp file then rename) so a crash mid-checkpoint leaves the previous snapshot intact.
