# ADR-0011: MDX evaluator seam, execute-time resolution, and zero-suppression

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Epiphany maintainers
- **Phase:** 3 (subsets, views, and MDX)

## Context

Phase 3 adds dynamic subsets (MDX-driven) and views/cellsets. Three design
points needed pinning so later phases build on stable contracts:

1. **Dependency direction.** `epiphany-mdx` already depends on `epiphany-core`
   (it evaluates over core's `Dimension`/`Cube`). The core query model (subsets,
   views, `execute_view`) must resolve dynamic subsets *without* a core ->
   epiphany-mdx edge, which would be a compile-time cycle.
2. **When dynamic membership is resolved.** A dynamic subset's members can change
   as the cube changes. We must define whether they are frozen at define time or
   computed at execute time, and against which version.
3. **Zero-suppression semantics.** The exact rule for dropping tuples, so the API
   and web agree with the engine and results are reproducible.

## Decision

**1. A `SetEvaluator` trait seam, owned by core, implemented in epiphany-mdx,
injected at the composition root.** `epiphany_core::SetEvaluator::eval_set(cube,
dim, mdx) -> Result<Vec<u32>, QueryError>` is defined in core. `epiphany-mdx`
provides `MdxEvaluator` (parse + evaluate). `epiphany-core` ships a
`NoSetEvaluator` that rejects dynamic subsets, so the whole core query model and
its tests build and run with zero MDX dependency. The server constructs the real
`MdxEvaluator` and injects it; the API holds `Arc<dyn SetEvaluator + Send +
Sync>` (the core trait), so production `epiphany-api` depends only on core - the
same injection pattern as the determinism `Clock`/`IdGen`. This mirrors and
reuses the ADR-0001 / ADR-0009 seam style.

**2. Dynamic subsets resolve at EXECUTE time against the pinned snapshot.**
`execute_view` takes `eval: &dyn SetEvaluator`; static subsets never touch it,
dynamic subsets call `eval.eval_set` against the dimension of the pinned
`ReadSnapshot`. Resolution therefore reflects the live model at query time, and
the cellset carries the snapshot `version` so a client can disambiguate. Reads
stay lock-free on the MVCC snapshot (ADR-0001).

**3. Zero-suppression rule.** With suppression on, drop a row tuple only if every
surviving opposite-axis (column) cell is exactly `Fixed::ZERO`, then drop a
column tuple only if every cell over the *surviving* rows is `Fixed::ZERO` -
rows first, then columns, order-preserving. A validly-empty axis or a
suppression that removes everything is a valid empty cellset, not an error.

**4. `editable`/`kind` are derived at the API layer.** Core's `Cellset` carries
member-name tuples and exact `Fixed` values only. The API re-resolves each
tuple's members to element kinds and marks a cell editable only when every member
across its row tuple, column tuple, and the context is a leaf (generalizing the
cell-write leaf check). The web grid trusts the server flag, never infers it.

**5. Definitions are durable via checkpoint-on-define.** Subset/view definitions
live in the durable `Model` (cube + subsets + views) the store owns; defining or
deleting one validates then checkpoints (rewrites the snapshot), leaving the
WAL/cell-write path byte-identical (ADR-0002). No new WAL record types.

## Alternatives considered

- **core depends on epiphany-mdx:** rejected - compile-time cycle, and it would
  force MDX into every core consumer and test.
- **Resolve dynamic subsets at define time (freeze members):** rejected - dynamic
  subsets would silently go stale as the model changes, defeating their purpose.
- **WAL-granular definition records (a `Mutation` enum with
  DefineSubset/DefineView):** rejected for M3 - it perturbs the Phase-2 atomic
  batch path and its acceptance tests for no DoD benefit. The engine
  define/delete wrappers are the seam to grow if WAL-granular durability is
  wanted later.

## Consequences

- core stays MDX-free; MDX is swappable and independently testable; the API is
  testable with `NoSetEvaluator` (static) or the real evaluator (a dev-dependency
  for dynamic-subset tests).
- The deferred MDX tail and the numeric-only/view scope are recorded in
  `docs/ROADMAP.md` (Phase 3). MDX errors surface as `MDX_ERROR` (422) with a
  message; precise parse spans for the editor are a deferred enhancement (the
  message is the only structured detail today).
- Realized in M3 as `epiphany-core::query` (model, `SetEvaluator`, `Subset`,
  `View`, `Cellset`, `execute_view`, `validate_*`), `epiphany-mdx`
  (lexer/parser/evaluator + `MdxEvaluator`), the engine define/delete commit
  path, and the REST subset/view/cellset surface. Gated by
  `epiphany-api/tests/m3_acceptance.rs`.
