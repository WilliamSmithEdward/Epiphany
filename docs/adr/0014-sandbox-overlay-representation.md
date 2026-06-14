# ADR-0014: Sandbox overlay representation

- **Status:** Accepted (overlay representation locked; realized across increments 6A-6I)
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 6 (sandboxes / what-if)

## Context

Phase 6 adds sandboxes: named, per-user, copy-on-write overlays over base data
in which a user enters what-if numbers, sees rules and consolidations recompute
over them without touching base data, then commits or discards. The done-when
(ROADMAP section 6) is exactly that round trip.

[ADR-0001](0001-concurrency-model.md) chose MVCC and copy-on-write and noted
that "sandboxes are a natural copy-on-write overlay over a base snapshot," but
it decided only the concurrency model. It did not decide the granularity of the
overlay, where the overlay attaches relative to the calculation engine, how the
per-query memo stays correct when the same cell has a base value and a what-if
value, or how overlay state is persisted. Those are the genuine, long-lived
decisions, so they get their own record here. This ADR realizes and refines
ADR-0001 rather than superseding it.

Three forces constrain the choice.

- **Rules and consolidations must recompute over what-if values.** A what-if
  edit to a stored leaf must flow into every rule that reads it and every
  consolidation that rolls it up. This is the load-bearing correctness
  requirement.
- **Determinism ([ADR-0009](0009-determinism-strategy.md)).** A sandboxed read
  must be reproducible: deterministic ordering, injected ids and clock, no wall
  clock in logic, and no aliasing between a base value and a what-if value.
- **Memory and isolation.** A sandbox is typically a handful of cells. It must
  not cost a full cube per user, and it must not be able to corrupt base
  durability.

The relevant existing seams, confirmed by reading the code: every cell access in
the calculation engine funnels through `CalcEngine::value`, which calls
`CalcEngine::compute`; `compute` reaches a stored leaf at exactly one terminal,
`Cube::get`, taken only for an all-leaf coordinate with no matching rule;
consolidation pulls each contributing leaf back through `CalcEngine::value` via
the `Cube::consolidate_with` closure; cross-cube and rule inputs route through
`CalcEngine::value` as well. Base writes commit through `Engine::apply_batch`
(clone, apply a validated batch, durably log, publish under the per-cube writer
lock). Durable model objects (subsets, views, rules, flows, connections) are
registered with an immediate snapshot rewrite (the define-then-checkpoint
pattern). The durability log records batch indices that are valid only against
the base snapshot.

## Decision

**0. Scope: a sandbox belongs to one cube.** A sandbox is a per-cube object,
like a subset, view, or rule set, and overlays leaf cells in that one cube. The
engine holds each cube in its own store published behind its own snapshot, and
the whole API surface is already per cube, so a per-cube sandbox fits the
existing persistence and routing with no server-level registry. Same-cube rules
and consolidations recompute over the overlay, which is exactly the done-when.
Cross-cube what-if (a what-if leaf in cube A flowing into a rule that another
cube B reads while B is queried without A's sandbox) is out of scope for this
phase and is a documented deferral; it would require a server-level, cross-store
sandbox and is a separate increment under its own scope.

**1. Cell-granular sparse delta, not a cube clone.** A sandbox is a sparse map
of overridden stored leaf values for its cube (numeric and string kept separate,
mirroring the cube's own split), deterministically ordered. An empty sandbox
costs almost nothing; cost is O(overridden cells). This is copy-on-write at cell
granularity, which is the truest realization of ADR-0001 for this feature.

**2. The overlay attaches beneath the calculation engine, at the stored-leaf
terminal.** `CalcEngine` carries an optional overlay and a scope id. The overlay
is consulted inside `compute`, at the all-leaf, no-matching-rule branch, in place
of `Cube::get`: if the overlay holds a value for that leaf, it is returned;
otherwise the stored value is read. Because rules read through `value`,
consolidations pull leaves through `value`, and cross-cube references route
through `value`, this single interception makes overlaid leaves visible
everywhere, so rules recompute and rollups re-sum over what-if values with no
change to `compute`, the expression evaluator, or `Cube`. An overlay placed
above the engine would be invisible to rule inputs, so beneath-the-engine is
required, not stylistic.

**3. The overlay replaces stored leaves only.** A rule-derived leaf (a leaf with
a matching rule) is computed, not stored, and the overlay does not mask it: the
matching-rule branch runs before the stored-leaf terminal. The contract is
crisp: what-if overrides stored leaf inputs; everything computed (rules,
consolidations) recomputes on top. What-if writes are leaf-only, reusing the
existing non-leaf write rejection.

**4. Memo is partitioned by scope.** The per-query memo key gains a scope id:
`(scope_id, cube_ordinal, coordinate)`, where `scope_id` is 0 for base reads and
the sandbox's stable id for sandboxed reads. In addition, each query uses one
`CalcEngine` (hence one memo) per scope, so a base read and a sandbox read never
share a memo. The scope id in the key is defense in depth: even if an engine
were ever shared across scopes, a base value and a what-if value for the same
coordinate cannot alias. Cycle detection is unaffected because the key still
uniquely identifies a cell within a scope.

**5. Durable in the model snapshot, never the base write log.** A sandbox (its
metadata and its cell deltas together) is a per-cube model object, registered and
persisted through the define-then-checkpoint path exactly like a subset or view,
so it serializes into the base snapshot and recovers with the model. What it must
never enter is the base write log: log records are incremental batch indices
valid only against the base snapshot, so admitting sandbox writes there would
corrupt the base-version replay contract and the guarantee that the published
version is always durable base truth. The snapshot, by contrast, is a full
serialization and carries sandbox state safely. A sandbox write therefore commits
as a define (a full checkpoint), not a base-log append, so it never touches base
cells or the base log; discard removes the sandbox from the model and
checkpoints. A separate per-sandbox delta file with its own incremental log is a
later scaling optimization (it avoids a full snapshot per what-if write and makes
discard a file truncate), not required for the DoD.

**6. Commit and discard.** Commit applies the sandbox's delta to its cube through
`Engine::apply_batch` against the base version observed when the sandbox last
synced, using the existing optimistic version check; if the cube's base moved
under the sandbox the commit conflicts and base is left unchanged, and on success
the delta is cleared. Discard removes the sandbox and checkpoints. Both leave
base untouched until commit succeeds.

**7. Selection is per-request and per-user.** A request selects a sandbox with a
header naming a sandbox within the cube the request addresses (sandboxes are per
cube, decision 0); absent the header, behavior is identical to today (fully
back-compatible). An extractor resolves the header and authorizes by owner (a
non-admin may use only sandboxes they own; admins may use any). The read,
view-execute, explain, and write paths become sandbox-aware purely by honoring
the selector; explain builds its provenance over the same overlay so what-if
provenance matches the what-if read.

## Alternatives considered

- **Whole-cube clone per sandbox behind its own snapshot handle.** The literal
  reading of copy-on-write, reusing the base publish machinery. Rejected: it
  pays the full cube cost (cells, string pool, dimensions) per user per cube even
  for an empty sandbox; it forces runtime structural changes to the engine's
  immutable cube map; and it fights the pinned calc registry, which assigns cube
  ordinals positionally, so rules compiled against base ordinals would not read
  the sandbox cube without rebuilding the registry, while cross-cube rules
  referencing an un-sandboxed cube must still reach base. Commit would also
  require diffing two cubes to recover the delta.
- **Overlay at the registry's cube accessor (return an overlay-aware cube).**
  Rejected: the stored-leaf and consolidation branches operate on a borrowed
  cube reference, so this needs a per-query wrapped or cloned cube whose reads
  reflect the overlay, which is strictly heavier than a value-level map and
  re-introduces a partial clone.
- **Overlay above the calculation engine (at the outer cell resolver).**
  Rejected on correctness: rule inputs and consolidation leaves do not pass
  through the outer resolver, so an overlay there would be invisible to rules and
  rollups, defeating the DoD.
- **Runtime-only sandboxes (no persistence).** Rejected: what-if would be lost
  across a restart, contradicting the per-user, persisted intent. Persisting in
  the model snapshot gives durability without endangering base recovery (the
  write log, not the snapshot, is the hazard).
- **Sandbox deltas in the base write log.** Rejected as unsafe: it would corrupt
  the base-version replay contract (see decision 5). The snapshot path is used
  instead.
- **Server-level, cross-store sandbox spanning cubes.** Deferred (decision 0):
  it needs a registry above the per-cube stores and a cross-store commit, beyond
  this phase. The per-cube sandbox satisfies the done-when; cross-cube what-if is
  a later increment.

## Consequences

- Rules and consolidations recompute over what-if values through one
  interception point; `Cube`, `compute`, and the expression evaluator are
  unchanged, and the no-overlay path is byte-identical to today, so base behavior
  and all existing tests are unaffected until a sandbox is selected.
- Memory cost is proportional to overridden cells, not to cube size or user
  count, so many concurrent per-user sandboxes are cheap.
- Determinism holds: a sandboxed read pins one base snapshot plus one immutable,
  ordered delta; ids are injected; the memo is scope-partitioned.
- Base durability is protected: sandbox writes use the snapshot (checkpoint)
  path and never enter the base write log, and commit goes through the same
  validated, version-checked base write path as any other write, so a stale-base
  commit is detected rather than silently overwriting.
- New surfaces: a `Sandbox` model type (persisted in the snapshot) and a sandbox
  overlay in the calc and engine layers; lifecycle and commit and discard
  endpoints; a per-request sandbox selector and owner authorization; and a web
  sandbox switcher with a visual distinction for uncommitted values.
- Documented deferrals (each its own later increment): cross-cube what-if
  (decision 0), a string what-if overlay (the overlay is numeric for this phase,
  matching numeric-only rules), a feeder-aware sparse consolidation over large
  override sets, and a separate per-sandbox delta file to avoid a full snapshot
  per what-if write.
- Validated by a deterministic Phase 6 acceptance suite: enter what-if numbers in
  a sandbox, assert rules and consolidations recompute over them while base is
  unaffected, then commit and assert base reflects them, or discard and assert
  base is unchanged.
