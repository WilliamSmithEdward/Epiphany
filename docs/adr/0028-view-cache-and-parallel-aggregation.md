# ADR-0028: Persistent view cache and deterministic parallel aggregation

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Maintainer
- **Phase:** Post-roadmap (ROADMAP section 13 deferred performance items)

## Context

ROADMAP section 8 sets two view-latency budgets: a cold view query at p99 under
about 1 s, and a cached or repeat view query at p99 under about 100 ms. The cold
budget is met today with wide headroom (a cold consolidated read that scans 100k
cells is about 10 ms on the development machine, roughly 100x under budget). The
cached-read budget has no implementation: every view execution recomputes from
the snapshot, so a dashboard re-reading the same view pays full price each time.

Section 13 lists two deferred, benchmark-gated performance items: a persistent
view (cellset) cache, and parallel aggregation. The architecture was built to
allow both (ADR-0001 MVCC snapshots; the `CellResolver` value seam; ADR-0009
determinism strategy) without precluding them. This ADR locks the design for
both and sequences them so the higher-risk work is gated on measurement.

Forces and constraints:

- **Correctness and security are the hard part, not speed.** A cellset's values
  depend on more than the cube and the view. They also depend on the caller's
  element deny mask (ADR-0015, per principal), the active what-if sandbox
  (ADR-0014, per user), the view's zero-suppression flag, and the cube's MVCC
  version. A cache keyed on too little would serve one principal another
  principal's element-masked or sandboxed numbers. This is a fail-closed
  violation and the primary risk to manage.
- **Determinism is binding (ADR-0009).** Any parallelism must produce results
  bit-identical to the serial path regardless of thread count or scheduling.
- **Dependency-light ethos.** No new runtime dependency unless strongly
  justified; `unsafe_code = "deny"` workspace-wide.
- **The 24-byte-per-cell memory budget is CI-enforced** (`epiphany-core/tests/
  memory.rs`). New allocations must not regress it.
- **Benchmark-gated.** Section 13 promotion is conditional on benchmarks showing
  the work is warranted.

## Decision

Ship one ADR covering two independently gated stages over a shared correctness
backbone (the MVCC version as the linearization point).

### Stage A: persistent view cache (ship first)

1. **Cache the core `Cellset`** (`epiphany-core::query::Cellset`), not the
   `CellsetDto`. The DTO transformation (`cellset_dto`) derives per-cell
   editability, element kind, and sandbox-overlay flags; that is cheap,
   principal-or-sandbox-contextual presentation work and stays on every read.
   The cached payload is the expensive-to-compute, presentation-free cellset.

2. **Store entries as `Arc<Cellset>`** so a hit is an O(1) refcount bump rather
   than an O(rows x cols) clone. `cellset_dto` is changed to take `&Cellset`
   (cloning only the small axis-name and context vectors it moves into the DTO),
   so the cold path and the hit path both feed it a borrow.

3. **Cache key** = `(cube, version, view_shape, sandbox_scope, mask_scope)`,
   and every component is **lossless** (the key is compared by derived `Eq`, so
   the cache never relies on hash-collision resistance for correctness or
   security; the hash is only a bucket):
   - `cube` (String): namespace and per-cube eviction/metrics granularity.
   - `version` (u64): the cube's MVCC commit version. Every commit (cell writes,
     subset/view/rule definitions, sandbox writes) increments the global
     monotonic id, so a write makes the next read miss. This is the entire
     invalidation story (see decision 6).
   - `view_shape`: the submitted view spec verbatim, normalized to the fields
     that affect values (rows, columns, context, and `suppress_zeros`). Name,
     owner, and visibility are excluded because they do not change cell values.
     Stored in full (not hashed) so two distinct shapes can never alias.
   - `sandbox_scope` (`None | Some(scope_id)`): the active sandbox's scope id
     (`sandbox.created.max(1)`, the same value that scopes the calc memo). Two
     distinct sandboxes never alias; a base read is `None`.
   - `mask_scope` (`Unmasked | Masked(denied_pairs)`): see decision 4.

4. **Two security tiers, fail-closed.**
   - **Safe-to-share tier:** when the principal's mask is absent or
     `is_empty()` AND no sandbox is active, the read is identical for every
     principal; `mask_scope = Unmasked`, `sandbox_scope = None`, and the entry
     is shared across all users. This is the common, high-value case (admins,
     any user with no element denials, base views).
   - **Scoped tier:** when a mask is present, the entry is keyed on the mask's
     exact denied-element set, the sorted list of `(dimension, element index)`
     pairs (a new `ElementMask::denied_pairs`). This is stored losslessly, so
     two principals with identical denials correctly share one entry and any
     difference in denials produces a different, non-aliasing key. The key is
     over the effective denial set that drives the result, never over username
     or group identity. When a sandbox is active the entry is additionally
     scoped by `sandbox_scope`.
   - **Fail-closed guard:** the safe-to-share entry is gated strictly on the
     mask being empty AND `sandbox.is_none()`. Because the masked key is
     lossless, there is no hash-collision path and no code path that serves a
     masked or sandboxed result to a principal whose effective context differs.

5. **Bounded approximate-LRU**, no new dependency. A `HashMap<ViewCacheKey,
   (Arc<Cellset>, tick)>` plus a monotonic access counter; on insert over the
   cap, evict the lowest tick. Default cap 256 entries, configurable via
   `EPIPHANY_VIEW_CACHE_ENTRIES` (0 disables the cache). A doubly-linked-list LRU
   is not worth the complexity for a small cache. To bound a hostile ad-hoc
   client minting unbounded distinct view shapes, the cache is split into two
   pools: saved-view reads (cap `EPIPHANY_VIEW_CACHE_ENTRIES`) and ad-hoc reads
   (a smaller sub-cap), so ad-hoc flooding can only evict ad-hoc entries and
   never the bounded-cardinality saved-view entries. Cellsets above a per-entry
   cell ceiling (1,048,576 cells) are not cached (computed fresh, stored
   nothing), so one giant view cannot consume the budget.

6. **Invalidation is implicit via version-keying.** No separate invalidation
   protocol. A cell write, model edit, or sandbox write all bump the version, so
   the next read forms a key with a new `version` and misses; superseded entries
   age out by LRU. A sandbox discard makes its `sandbox_scope` unreachable
   (no principal reproduces it) and its entries age out. This is the strongest
   coherence story: there is no invalidation code to get wrong.

7. **Location.** A new `crates/epiphany-api/src/view_cache.rs` exposing
   `ViewCache`, held in `AppState` as `Arc<ViewCache>` (shared across the cheap
   `AppState` clones). The cache lives at the API layer because its key
   components (mask, sandbox, principal context) are API concepts; core stays
   pure. A shared `cached_or_compute` helper wraps the execute call in both
   `execute_saved_view` and `execute_adhoc`. The cache is read-through only and
   is never populated during bulk load, preserving the memory test's snapshot
   isolation (decision in Stage A consequences).

### Stage B: deterministic parallel aggregation (benchmark-gated)

8. **Parallelize across independent output cells, never within a reduction.** A
   cellset is a row-major grid; each output cell is computed independently into
   its own pre-indexed slot `cells[i]`. Completion order across threads cannot
   affect the result because thread k always writes slot `i_k` and no other. The
   within-cell consolidation (`Cube::consolidate_with`) keeps its existing fixed,
   leaf-index-sorted reduction order unchanged; it is not parallelized. This is
   the only model that satisfies ADR-0009 without relying on float associativity.

9. **Per-shard calc state.** Each worker gets its own `CalcEngine` (its own
   `RefCell` memo) over the same immutable `&Cube`/overlay borrow. The memo's
   internals are not modified: no locking, no `DashMap`, no shared mutable calc
   state. Within-shard memo reuse is kept; cross-shard reuse is given up (a minor
   cost, since shards are contiguous row bands and most sub-results are
   cell-local). This touches none of the thread-safety blockers in the calc map.

10. **Threading via `std::thread::scope`, no new dependency.** Scoped threads
    borrow the stack-resident `&Cube`, resolved index vectors, and immutable
    overlay without `'static`/`Arc`, and write disjoint `chunks_mut` sub-slices
    of the output, so no `Mutex` and no `unsafe` are needed. No `rayon`: a fixed
    contiguous partition is both simpler and friendlier to the per-shard-memo
    reasoning than work-stealing. Worker count =
    `min(available_parallelism, ceil(ncells / threshold), cap)`. The worker
    count cannot change the result (decision 8), which the determinism test pins.

11. **Threshold.** Parallelize only when the grid has at least `CELL_THRESHOLD`
    cells (initial 1024); below that the serial loop runs and the result is
    byte-identical to today. The threshold is on cell count alone (a large
    pure-leaf grid parallelizes harmlessly: each `value` is cheap and the result
    is still identical), which keeps the gate simple and the common small read
    untouched. The policy is a `Parallelism` value (`auto`, `serial`, or
    `forced(n)` for tests/benches); `execute_view` uses `auto`.

12. **Determinism is proven, not assumed.** A test computes a battery of
    generated cubes/views serially and in parallel at worker counts {1, 2, 3, 7}
    and asserts bit-identical `Cellset` equality, plus a repeat-run stability
    test at a fixed worker count. Fixtures are generated with the determinism
    crate's seeded RNG (single-threaded, before any thread spawns; no randomness
    inside the parallel region).

13. **Promotion gate.** Stage B ships only if `bench_parallel_view_speedup` on a
    deliberately high-dimensionality stress fixture shows at least a 1.5x
    speedup at the target size with the cache already in place. If the serial
    path already clears the cold budget on the stress fixture, Stage B is
    recorded as not-yet-warranted and deferred, with the fixture and benchmark
    left in place to revisit. The benchmark must be measured before Stage B is
    implemented.

### Benchmarks

14. New self-contained benches in `crates/epiphany-core/benches/view_exec.rs`
    plus a shared `make_representative_cube` fixture builder (multi-dimensional,
    consolidation hierarchy, realistic sparsity), reused for cache and parallel
    benches. Cases: cold consolidated view (validates the cold p99 budget and
    the serial baseline), cache key + fingerprint overhead (must be well under
    1 ms so a 100%-miss workload is not penalized), and, for Stage B, a serial-
    vs-parallel speedup case and a high-dimensionality consolidation case. The
    cached-hit p99 < 100 ms budget is validated by an `epiphany-api` timing test
    exercising the `ViewCache` get path.

15. **Memory budget is preserved.** The view cache and per-shard memos are
    query-time / startup allocations, outside the `memory.rs` snapshot window
    that measures cell-store growth during the populate loop. The cache MUST NOT
    be populated eagerly during bulk load. Cache memory is bounded by its own cap
    (decision 5) and reported separately, never folded into bytes-per-cell.

## Alternatives considered

- **Cache only the safe subset (no masked/sandboxed entries).** Simpler, but
  caches nothing for any user with element ACLs, defeating the cache for
  security-conscious deployments where Phase 7 element security is in use. The
  scoped tier costs cloning the small denied-element set into the key and is
  provably correct because the key is the exact denial set that produced the
  result. Rejected in favor of the two-tier design.
- **Cache the `CellsetDto`.** Larger payload and entangles presentation
  (editability, sandbox-overlay flags) that is principal/sandbox-contextual,
  forcing more into the key. Caching the pre-DTO cellset keeps the safe-tier
  payload principal-agnostic. Rejected.
- **Active invalidation by subscribing to change events.** Unnecessary for
  correctness because version-keying self-invalidates. It would add coupling for
  a memory-reclaim-only benefit. Left as a possible future optimization.
- **Thread-safe shared memo (`Mutex`/`RwLock`/`DashMap`) for parallel agg.** The
  `Mutex` serializes the hot path; the `RwLock` degrades to it because every miss
  writes; `DashMap` adds a dependency. All rejected for v1 in favor of per-shard
  memos.
- **`rayon` for parallelism.** A new dependency against the dependency-light
  ethos, and work-stealing makes shard boundaries dynamic, complicating the
  per-shard-memo and determinism reasoning. Rejected for `std::thread::scope`.

## Consequences

- The common dashboard case (repeated reads of the same base view between writes)
  becomes an `Arc` clone plus the DTO transformation, meeting the cached-read
  budget. Reads after any write correctly miss and recompute (read-after-write
  consistency holds by construction).
- Element security and what-if isolation are preserved: a masked or sandboxed
  result is never served outside its exact context, enforced by the key and a
  fail-closed gate, and covered by a test asserting different denial sets yield
  different keys and never share an entry.
- Stage A is shippable and taggable on its own; Stage B may be deferred
  indefinitely if measurement does not warrant it. Either way the benchmark
  fixture and harness land, closing the section-13 "benchmark-gated" item with
  data rather than assertion.
- Parallel aggregation, if shipped, is determinism-safe by construction
  (indexed-slot writes, unchanged within-cell reduction order, no randomness in
  the parallel region) and proven by a serial-vs-parallel equality test across
  worker counts.
- New surface: `ViewCache` (api), `ElementMask::denied_pairs` (core), a
  `view_cache` field on `AppState`, an `EPIPHANY_VIEW_CACHE_ENTRIES` config knob,
  and `view_exec` benches. The 24-byte-per-cell test is unaffected.
- Validation: the determinism tests, the cache-key security test, the
  read-after-write test, the cached-hit timing test, and the speedup benchmark
  together validate the decision. PERFORMANCE.md and ROADMAP section 13 are
  updated with the observed numbers and the Stage B gate outcome.

### Outcome (both stages shipped)

The `view_exec` benchmark on the development machine (release, 14 cores) showed
serial cold view execution well within the section-8 budget even on a pathological
all-consolidated crossjoin (about 454 ms for a 40k-cell grid, p99 budget 1 s), so
the cache (Stage A) is the primary win for repeat reads. Stage B was still
warranted for cold large reads, which the cache cannot accelerate: parallel
aggregation cut a 16.6k-cell consolidated view from about 139 ms to 20 ms (6.9x),
a 40k-cell view from about 454 ms to 72 ms (6.3x), and a 4-dimensional view 4.5x,
while small views (below the 1024-cell threshold) stay serial with no measurable
overhead (1.0x). The serial-vs-parallel equality test confirms bit-identical
results across worker counts {2, 3, 5, 7} and across repeated runs, so the speedup
costs no determinism. Both stages shipped.
