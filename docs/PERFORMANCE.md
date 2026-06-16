# Performance and memory

The performance and memory mandate is a binding non-functional requirement
(ROADMAP sections 1 and 8). This page records the budgets, how they are
measured, and the numbers observed, validated as part of Phase 8.

The budgets are initial targets to validate and tune against real hardware and
models; they are not promises. Measurements below are from a development machine
in release mode and are indicative.

## Budgets (ROADMAP section 8)

| Dimension | Target |
|---|---|
| Memory per numeric leaf cell | about 24 bytes or less, including index overhead |
| Bulk-load throughput | about 1M cells per second per core or more |
| Cold view query (typical) | p99 under about 1 s |
| Point read | sub-microsecond |
| Startup (snapshot load, large model) | seconds, not minutes |

## How it is measured

All harnesses are self-contained (no external benchmark framework), so they
build on the GNU toolchain and add no dependencies.

- **Bytes per cell** is a CI test (`epiphany-core/tests/memory.rs`): a counting
  global allocator snapshots live heap bytes around populating a cube's cell
  store, so the delta is the store's growth alone. It asserts the 24-byte budget,
  so a regression fails CI.
- **Throughput and latency** are a benchmark (`epiphany-core/benches/cube_ops.rs`,
  run with `cargo bench -p epiphany-core`): best-of-N wall-clock timing of
  bulk-load, point reads, and a cold consolidated read that scans every populated
  cell. It validates the mandate; it does not gate correctness.
- **Scheduler scale** is a benchmark (`epiphany-flow/benches/scheduler.rs`, run
  with `cargo bench -p epiphany-flow`): one reconcile tick's pure due-selection
  over many declared jobs against a populated run ledger (ADR-0013), so the cost
  of waking the loop is visible and bounded.

## Observed results (release, development machine)

| Measurement | Observed | Budget |
|---|---|---|
| Bytes per leaf cell | about 17 bytes/cell (u64 key + i64 value + table overhead) | <= 24 bytes (asserted in CI) |
| Bulk-load | about 13 M cells/sec/core (100k cells) | ~1 M/sec/core |
| Point read (`get_leaf`) | about 37 ns/op | sub-microsecond |
| Cold consolidated read (scans 100k cells) | about 10 ms/call | p99 < ~1000 ms |
| Cold consolidated view, serial (40k-cell all-Total crossjoin) | about 454 ms/call | p99 < ~1000 ms |
| Same view, parallel aggregation (14 cores, ADR-0028) | about 72 ms/call (6.3x) | p99 < ~1000 ms |
| Repeat view read (cached, ADR-0028) | a refcount bump plus the DTO transform | p99 < ~100 ms |
| Reconcile due-scan | 2000 jobs against a 1000-run ledger in about 11 ms/tick | cheap relative to the tick period (default 1 s) |

Bulk-load clears the budget by an order of magnitude, the cold consolidation is
roughly 100x under the latency budget, and bytes-per-cell is within budget and
CI-enforced. View execution adds a persistent cache for repeat reads and parallel
aggregation for large cold reads (both ADR-0028); the serial path already meets
the cold budget, and parallelism (gated above 1024 cells) cuts large cold reads
several-fold on a multicore host.

## Notes and known scaling characteristics

- The reconcile selector is linear in (jobs x ledger size) per tick, because each
  job's `last_succeeded_fire` scans the ledger. This is comfortably fast for the
  expected scale (tens to hundreds of jobs); a per-job `last_fired` index is a
  deferred optimization if a deployment ever runs thousands of jobs at a sub-second
  tick.
- Parallel aggregation and a persistent view cache are implemented (ADR-0028).
  The view cache (`epiphany-api`, bounded and version-keyed) serves repeat reads
  of an identical view between writes; it is keyed losslessly on cube version,
  view shape, sandbox scope, and the caller's element-deny set, so it never serves
  a stale or cross-principal result. Parallel aggregation (`execute_view_with`,
  `epiphany-core`) fills the cellset grid across `std::thread::scope` workers, one
  per disjoint row band, above a 1024-cell threshold; it is determinism-safe by
  construction (each output cell writes only its own slot, the within-cell
  reduction order is unchanged) and proven bit-identical to the serial path by a
  serial-vs-parallel equality test across worker counts. The `view_exec`
  benchmark measures both. Configure the cache cap with `EPIPHANY_VIEW_CACHE_ENTRIES`
  (default 256; 0 disables).
