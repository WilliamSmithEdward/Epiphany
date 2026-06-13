# ADR-0009: Determinism strategy

- **Status:** Accepted
- **Date:** 2026-06-12
- **Deciders:** Epiphany maintainers
- **Phase:** 0

## Context
"Directly and deterministically test every feature; know for certain the app
works at every milestone" is a binding mandate (ROADMAP §1). Determinism must be
a property of the *design*, not a testing afterthought. The hazards: wall-clock
reads, RNG, hash-map iteration order, concurrency, and float summation order.

## Decision (accepted)
1. **Inject the clock, RNG, and id generator** — logic takes them as parameters;
   it never calls `SystemTime::now`, an ambient RNG, or generates ids directly.
   Implemented in the `epiphany-determinism` crate (`Clock`/`ManualClock`/
   `SystemClock`, `DeterministicRng` (SplitMix64), `IdGen`, `Deterministic`).
2. **Deterministic mode** — a server-wide configuration used by tests: fixed
   clock, seeded RNG, fixed hash seed, ordered iteration wherever output is
   observable, and deterministic parallel reduction.
3. **Consistent reads** via MVCC snapshots ([ADR-0001](0001-concurrency-model.md)).
4. **Exact numerics + ordered aggregation** ([ADR-0008](0008-numeric-model-and-precision.md)).
5. **Executable, deterministic acceptance suite per phase/milestone**; flaky =
   bug, fixed or quarantined immediately.

## Alternatives considered
- **Test-only seeding bolted on later** — rejected; nondeterminism leaks in
  through ambient APIs and is painful to retrofit.
- **External RNG crate (`rand`)** — fine, but a dependency-free SplitMix64 keeps
  the harness reproducible and zero-dep.

## Consequences
- A small discipline cost on every feature (thread the clock/RNG through), repaid
  by reproducible bugs and trustworthy milestone gates.
- Implemented and tested in Phase 0 (`epiphany-determinism` — 6 passing tests
  proving reproducibility). The whole-server deterministic mode lands with the
  server wiring in Phase 2.
