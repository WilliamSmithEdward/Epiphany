# ADR-0007: Rule evaluation strategy (compiled rules, on-demand eval, in-query memoization)

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 4 (rules, feeds, and calculation engine)

## Context

Rules turn the cube from storage into a calculation engine. The evaluation
strategy is foundational and costly to retrofit, so it is decided by ADR. Four
points needed pinning: the compiled form, when and how rules evaluate, how the
calc layer reaches cell values without a dependency cycle, and how repeated work
within a query is avoided. The numeric model, exact scaled-integer `Fixed` with
round-half-to-even, is already fixed by ADR-0008; this ADR builds on it.

## Decision

**1. Compile rules to a resolved, index-addressed AST, not bytecode and not
re-parsed text.** `epiphany-calc::compile` runs once per published model version
and lowers each parsed rule to a `CompiledRule`: an area predicate over element
indices and a `CExpr` tree whose cell references are resolved to
`(cube ordinal, per-dimension address slot)`. Each address slot is either
`Pinned(index)` (an explicit member) or `FromTarget(position)` (copied from the
cell being evaluated). No dimension, member, attribute, or cube name is resolved
again at eval time, and nothing is re-parsed per cell. A resolved closure-style
AST was chosen over a bytecode VM because it is simpler, allocation-light, and
fast enough for M4; a bytecode or JIT backend can replace the evaluator behind
the same compiled-model boundary later without touching callers.

**2. Cross-cube references are fully addressed; relative cross-cube mapping is
deferred.** A same-cube reference defaults un-overridden dimensions to
`FromTarget`. A cross-cube reference must address every dimension of the
referenced cube explicitly, because `FromTarget` would copy an element index
between unrelated dimensions, which is meaningless. The `with (src -> dst)` name
mapping that would make cross-cube references relative is parsed but rejected at
compile time (`Unsupported`) for M4. String-cell formulas and text functions are
likewise parsed but deferred; M4 evaluates numeric rules only.

**3. On-demand evaluation with a per-query memo and precise cycle detection.**
`CalcEngine::value(cube, coord)` resolves a value lazily: a leaf with a matching
rule fires that rule; a leaf with no rule reads stored data; a consolidated
coordinate aggregates via the dense `consolidate_with` (correctness never depends
on feeders, see ADR-0005). Results are memoized for the life of one query, keyed
by `(cube ordinal, coordinate)`, so a cell referenced many times in a cellset is
computed once. A coordinate currently being evaluated is tracked so a rule that
depends on itself fails with a precise `Cycle` error rather than overflowing the
stack. The memo lives in the engine instance (a fresh one per resolver call), so
it never leaks stale values across writes; reads run on the pinned MVCC snapshot
(ADR-0001).

**4. The calc layer reaches values through a core-owned seam, so dependency
direction is preserved.** `epiphany-calc` depends on `epiphany-core` only, never
the reverse, and `epiphany-engine` stays calc-free. The API composition root
implements a `CellResolverFactory` (core's `CellResolver` value seam) with a
`CalcFactory` that snapshots every cube, compiles each cube's rules once into a
pinned multi-cube registry, and hands back a rule-aware resolver; deployments and
tests without rules inject the engine's stored-only resolver instead. This is the
same injection pattern as the determinism `Clock`/`IdGen` and the MDX
`SetEvaluator` (ADR-0011).

## Alternatives considered

- **Bytecode VM or JIT now:** rejected for M4 as premature. The resolved AST
  meets the correctness and performance bar for this phase; the compiled-model
  boundary lets a faster backend land later without a caller change. A JIT
  remains a possible future ADR.
- **Re-parse or re-resolve per cell:** rejected. It puts string work on the hot
  path and defeats the point of a compile step.
- **A global/persistent calculation cache:** rejected for M4. A per-query memo is
  simple and correct by construction (it cannot serve stale values across a
  write, because each query gets a fresh memo over a fresh snapshot). A
  longer-lived cache with explicit invalidation is a later optimization, not a
  Phase 4 need.
- **Letting the engine depend on calc (eager rule eval inside the engine):**
  rejected. It would couple the concurrency/durability layer to the calculation
  layer and create a dependency cycle; the `CellResolverFactory` seam keeps the
  engine calc-free.

## Consequences

- Rules compile once per version and evaluate with no per-cell string work;
  repeated references within a query are memoized; self-referential rules fail
  with a precise error instead of a stack overflow.
- The deferred surface (cross-cube `with` mapping, string formulas, text
  functions) is recorded here and in `docs/ROADMAP.md` (Phase 4) and is
  forward-compatible: it parses today and is rejected at compile time, so adding
  it later is an evaluator change, not a grammar change.
- Realized in M4 as `epiphany-calc` (`rules` lexer/parser/AST, `compile`,
  `compiled` model, `eval` with `CalcEngine`/`CalcError`/`CellResolver` bridge,
  `provenance::explain`) and the API `CalcFactory`/`PinnedRegistry`. The
  provenance trace value is asserted to agree with the evaluator, and the whole
  path is gated by `epiphany-api/tests/m4_acceptance.rs`.
