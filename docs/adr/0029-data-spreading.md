# ADR-0029: Data spreading

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Maintainer
- **Phase:** Post-roadmap (ROADMAP section 13 deferred data-entry item)

## Context

Data entry today writes one leaf cell at a time. A planning user wants to type a
number at a higher level (a consolidated intersection, for example Region=Total)
and have it distributed across the underlying leaf cells by a rule: split it
evenly, split it in proportion to what is already there, repeat it to each, or
clear them. ROADMAP section 13 deferred spreading and named the starting set:
proportional, equal, repeat, and clear.

Constraints and forces:

- **Exactness (ADR-0008).** Values are scaled integers. A spread must distribute
  the entered value so the contributing leaves sum back to it exactly, with a
  deterministic remainder allocation (no float drift, no order dependence).
- **Determinism (ADR-0009).** The same request must produce the same leaf writes
  every time: ordered leaf enumeration, ordered remainder allocation.
- **Reuse the safe write path.** Spreading must go through the same element-
  security gate, sandbox routing, MVCC commit, and change-broadcast as a normal
  write; it only changes how a value becomes a set of leaf writes.
- **Predictability over cleverness.** Spreading into a weighted consolidation
  (where a child contributes with a weight other than +1, like a subtraction
  member) has no intuitive answer that preserves the total. v1 refuses it rather
  than produce a surprising result.

## Decision

1. **A pure spreading engine in `epiphany-core`** (`spread.rs`):
   `spread_leaves(cube, target, value, method, read_leaf) -> Result<Vec<(Vec<u32>,
   Fixed)>, SpreadError>`. It expands the target coordinate into leaf writes; it
   does no I/O, security, or commit. The current-value reader (`read_leaf`, used
   only by Proportional) is injected, mirroring the `CellResolver`/`SetEvaluator`
   seam, so the engine is unit-testable and the API supplies a resolver that
   already honors the active sandbox and element mask.

2. **Methods (the v1 set):** `Equal`, `Proportional`, `Repeat`, `Clear`.
   - `Repeat`: every contributing leaf gets `value`.
   - `Clear`: every contributing leaf is set to zero.
   - `Equal`: `value` split evenly; the integer remainder (in scaled units) is
     allocated one unit at a time to the leading leaves in coordinate order, so
     the leaves sum to `value` exactly.
   - `Proportional`: each leaf gets `value * w_i / sum(w)` where `w_i` is its
     current value (read through the injected reader); the floor remainder is
     allocated as in Equal so the total is exact. If `sum(w)` is zero,
     Proportional falls back to Equal (there is no basis to weigh by).
   - Arithmetic uses the scaled `i64` directly, with `i128` for the proportional
     product to avoid overflow. The remainder allocation is the only rounding and
     it is deterministic.

3. **Leaf enumeration.** Each target dimension expands to its contributing leaves
   via `Dimension::leaf_weights` (already sorted by leaf index). The leaf write
   set is the cartesian product across dimensions, enumerated in a fixed order. A
   leaf dimension expands to itself.

4. **Refuse weighted consolidations (fail-safe).** If any contributing leaf in
   any dimension has an accumulated weight other than +1, the spread is rejected
   (`SpreadError::WeightedConsolidation`). Equal/Proportional/Repeat all preserve
   the entered total only under unit-weight additive consolidation (the common
   case: a Total that is the plain sum of its children). Spreading into a
   weighted member is uncommon and has no total-preserving answer, so v1 declines
   rather than mislead. (Revisitable.)

5. **Bound the blast radius.** The cartesian product is capped
   (`MAX_SPREAD_LEAVES`); a target that would expand beyond it is rejected
   (`SpreadError::TooManyLeaves`) rather than producing a giant batch.

6. **Numeric only.** A target that addresses a string cell is rejected; spreading
   is for numeric leaves.

7. **REST: `POST /api/v1/cubes/{cube}/cells/spread`** (cube Write), body
   `{ target: {dim: member}, value: "<decimal>", method: equal|proportional|
   repeat|clear }`, honoring the `X-Epiphany-Sandbox` selector. The handler
   resolves the target, runs the engine (supplying a resolver-backed reader),
   gates the resulting leaf coordinates with `require_element_write_indices`
   (fail-closed: if any contributing leaf is not writable the whole spread is a
   403, nothing is written), then applies the writes through the existing
   `apply_batch` (base) or `sandbox_set_cells` (what-if) path and broadcasts a
   `CellsChanged` event. The response is the usual `{ applied, version }`.

8. **Web: a spreading affordance in the pivot grid.** A consolidated cell can be
   spread: the user picks a method and enters a value; the grid calls the spread
   endpoint and refreshes. Leaf cells keep the existing direct-edit path.

## Alternatives considered

- **Spread on the client.** The browser could expand the leaves and send a batch.
  Rejected: it duplicates the model (hierarchy, weights, exact arithmetic) in
  TypeScript, cannot read other users' values safely, and would drift from the
  server's rounding. The server owns the model and the exact numerics.
- **Allow weighted consolidations with a best-effort split.** Rejected for v1:
  there is no total-preserving distribution across mixed weights, and a silent
  surprise in a planning total is worse than a clear refusal.
- **Round with floats and fix up.** Rejected: violates ADR-0008. Scaled-integer
  division with deterministic remainder allocation is exact and reproducible.
- **A new engine write path.** Rejected: spreading reuses `apply_batch` /
  `sandbox_set_cells` and the element-security gate unchanged; it is purely a
  value-to-writes expansion in front of the existing path.

## Consequences

- Planning users can enter at a total and distribute down, the headline missing
  data-entry ergonomic, without ever writing to a consolidated cell directly.
- Spreads are exact (leaves sum to the entered value) and deterministic, proven
  by property tests over generated cubes (sum-preservation and run-to-run
  stability) plus method unit tests; integration tests cover the endpoint,
  sandbox routing, and element-security refusal.
- Element security and what-if isolation are preserved by construction: spreading
  produces leaf coordinates that pass through the same per-leaf write gate and
  the same sandbox routing as a normal batch.
- New surface: `SpreadMethod` + `spread_leaves` + `SpreadError` (core), a
  `POST .../cells/spread` endpoint, and a grid spreading control. No new
  dependency.
- v1 deliberately omits percent-change, relational, and holds spreading, and
  refuses weighted consolidations; each is a documented, revisitable follow-on.
