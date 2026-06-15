# ADR-0024: Shared, independent dimensions (a dimension registry)

- **Status:** Accepted (design lock; realized across phases 0-4)
- **Date:** 2026-06-15
- **Deciders:** Epiphany maintainers
- **Phase:** Post-roadmap (model architecture)
- **Amends:** ADR-0001 (concurrency), ADR-0003 (model-as-code), ADR-0006 (cell
  layout). **Extends:** ADR-0021 (model-editing). **Touches:** ADR-0014
  (sandboxes), ADR-0015/0016/0023 (security).

## Context

In standard OLAP modeling a dimension is a first-class, server-level object that
many cubes share: editing "Region" once updates every cube that uses it. Epiphany
does the opposite today - each `Cube` *owns* its dimensions by value
(`Cube.dimensions: Vec<Dimension>`, cube.rs), so dimensions are cube-local copies
created only as part of a cube (ADR-0021), and an edit in one cube cannot be seen
by another. The user requires independent dimensions, reusable by reference, where
an edit propagates to all referencing cubes.

A design analysis across all layers found that three things commonly assumed to
block this are already true in the codebase:

1. **Layout is already per-cube and recomputed per-cube.** `Cube` owns its own
   `Layout` and `relayout()` rebuilds packing independently when a dimension
   grows. Sharing element *identity* does not force a shared bit layout; each cube
   keeps packing its own view (and two cubes may pack the same dimension at
   different bit-widths).
2. **The cube set is already an `ArcSwap<BTreeMap<..>>` behind a coarse `topology`
   lock** (ADR-0021). A parallel `ArcSwap<DimensionRegistry>` behind a
   `dim_topology` lock is the same proven pattern, not a new invention.
3. **Model-as-code already references dimensions by name.** `CubeDoc.dimensions`
   is already `Vec<String>`; dimensions embed in the cube snapshot only because a
   snapshot is one cube today (the format's own forward-compat note anticipated a
   multi-cube model).

The genuinely hard part is the **cross-cube atomicity of a shared-dimension
grow** under the per-cube writer model (ADR-0001 gives one linearization point
*per cube*, not across cubes). This is a pre-release library, so the on-disk
format may change with a load-time migration and no long-term back-compat.

## Decision

Introduce a server-level **dimension registry**; cubes hold **references** to
shared dimensions. Element identity (and the consolidation hierarchy and
attribute definitions) is owned once, in the registry; each cube keeps its own
order of references, its own packed layout, and its own cells. Editing a shared
dimension propagates to every referencing cube. This is true shared-by-reference
(the user's requirement); a copy-by-value "template" is explicitly rejected as the
end state because copy semantics cannot make an edit propagate.

### Registry and references

- `Engine` gains `dimensions: Arc<ArcSwap<DimensionRegistry>>` and a coarse
  `dim_topology: Mutex<()>`, structurally parallel to `cubes`/`topology`.
- `DimensionRegistry: DimensionId -> Arc<SharedDimension>`, plus a reverse index
  `DimensionId -> set<cube>` of referencing cubes.
- `SharedDimension` = today's `Dimension` + a server-unique `id: DimensionId`
  (minted from `IdGen`, never positional) + `generation: u64` (bumped on every
  append). It owns the `(index, name, kind)` element identity and `index_by_name`
  - the single source of truth - plus the hierarchy edges and attribute defs.
- `Cube.dimensions: Vec<Dimension>` becomes `Vec<DimensionRef>` where
  `DimensionRef { id: DimensionId, attached_generation: u64 }` is `Copy`-cheap.
  The cube keeps its dimension *order* (rank/position unchanged), its own
  `Layout`, and its cells; it no longer owns element lists. Cloning a cube on the
  writer path no longer deep-clones dimensions (a memory win over today's
  clone-on-commit).
- Because the index space is owned by the one `SharedDimension`, element 5 =
  "North" is index 5 in *every* referencing cube. There is **no per-cube index
  space and no coordinate-translation layer** - the thing that would otherwise be
  hard simply does not exist.

### Per-cube layout (ADR-0006 unchanged in spirit)

`Layout::new` changes only its input source from `&[Dimension]` to the dimensions
resolved from a cube's refs against a pinned registry snapshot; it still computes
`bits_for(dim.len())` per slot. On a grow `g -> g+1`, the registry commits the new
`SharedDimension` once (the atomic event); each referencing cube then runs the
**existing** `relayout()`: if its bit-width for that slot grew it repacks its
cells, otherwise (the common same-width append) relayout is a no-op and the cube
only advances its `attached_generation`. Two cubes may pack the same dimension at
different widths - fine, because nothing shares byte layout; only identity is
shared. Append-only is preserved and now enforced at the registry.

### MVCC (ADR-0001 amended)

`Published` becomes `(version, registry_snapshot: Arc<DimensionRegistry>, model:
Arc<Model>)`: a cube's published snapshot pins **both** its cells and the exact
registry generation they were packed against. Reads stay lock-free (one atomic
load of the cube's `published`, which carries its pinned registry Arc) and
snapshot-isolated. A cube whose repack has not yet caught up to a newer global
registry generation keeps publishing against its older pinned generation until its
repack commits, so a reader never interprets a cube's cells against the wrong
generation.

> Correction folded in (from the adversarial review): this pinned-registry
> `Published` does **not** exist today (`Published` is `{version, model}` and
> `Cube` owns `Vec<Dimension>` by value). Threading a pinned registry through
> every read-path method - `Cube::get` / `consolidate_with` / `check_coord` /
> `leaf_weights`, `ReadSnapshot`, calc, MDX, the element mask - is the dominant,
> blast-radiused cost of Phase 1 and is budgeted as such, not assumed.

### Eventual, generation-pinned repack (the hard part)

A shared-dimension grow is the single atomic durable event; per-cube repacks are
idempotent, deterministic follow-on work that readers never observe torn (every
snapshot pins its generation). **Repack defaults to lazy** (repack-on-next-commit
/ on-load); **eager cross-cube fan-out repack is behind a benchmark gate**,
because a bit-width-crossing grow on a dimension shared by N populated cubes costs
O(cells) repack **plus a full snapshot rewrite per referencing cube** (the WAL has
no incremental relayout record and `extend_schema` checkpoints synchronously) -
the engine's most expensive op times N. The common same-width append stays a
no-op. Eager repack is not load-bearing until benchmarked against the per-cell
budget (ADR-0006).

### Locking, durability, migration, deletion (safety corrections)

- **Lock order:** a single global hierarchy `dim_topology` -> per-cube `writer`,
  always acquired in that order, to preclude the AB/BA deadlock the flow-driven
  growth path could create. The registry grow's CAS + durability + publish is one
  fail-closed critical section (durable registry write happens *before* any cube
  repack, so the registry is always the authority; a lagging cube repacks
  forward, never backward).
- **Migration v0 -> v1 is non-merging by default:** each `(cube, dimension)` in an
  existing v0 snapshot becomes its **own** `DimensionId`. There is **no silent
  structural-hash dedup**; merging two cubes onto one shared dimension is an
  **explicit, opt-in** operator action. v0 snapshots are accepted and
  auto-migrated on load, with a backup kept on first migrate; migration is
  idempotent.
- **A referenced dimension cannot be deleted** (the reverse index enforces it);
  delete/rename remain non-goals (ADR-0021), now at the registry.
- **Security re-keying is reconciled with ADR-0023:** element ACLs re-key from
  `(cube, dimension, element)` to `(DimensionId, element)` using the same
  load-time id map, so a deny on a shared dimension's element applies in every
  referencing cube; the positional `ElementMask` build is unchanged on the hot
  path.

### On-disk + model-as-code (ADR-0003 amended)

Split the single `snapshot.model` into:

- `<data_dir>/dimensions/<dim_id>.model` - one canonical dimension each (id, name,
  generation, `[[element]]`, `[[edge]]`, `[[attribute]]` defs).
- `<data_dir>/cubes/<name>/snapshot.model` - the cube: name + ordered
  `dimensions = [{ id, generation }]` references (not embedded defs) + cells +
  subsets/views/rules/flows/sandboxes/jobs.

`FORMAT_TAG` advances to `epiphany-model/v1`; boot loads the registry first, then
cubes. Attribute *values* are shared (set once, visible to all referencing cubes -
the intuitive "edit propagates" behavior).

## Phasing

- **Phase 0 - registry skeleton (no behavior change):** `DimensionId`,
  `SharedDimension`, `DimensionRegistry`, the reverse index, and the `Engine`
  fields, additive and unused by the live path, with unit tests
  (append/generation/idempotence). Ships first.
- **Phase 1 - references + split snapshot + non-merging migration (library UX):**
  `Cube` holds `Vec<DimensionRef>` and resolves through a pinned registry
  snapshot; `Published` bundles the registry; split on-disk format + v0->v1
  auto-migration; boot loads registry then cubes; REST + UI to create/edit a
  dimension independently and attach it (by reference) to cubes. Grow restricted
  to same-width appends or lazy repack. This phase carries the read-path blast
  radius.
- **Phase 2 - eager cross-cube repack + dimension WAL (benchmark-gated):** the
  generation-pinned eventual-repack protocol with deadlock-free lock ordering and
  repack-on-load recovery; benchmark a bit-width grow on an N-cube-shared
  dimension against the per-cell budget before it is load-bearing.
- **Phase 3 - DimensionId-keyed security + shared subsets:** re-key element ACLs
  to `DimensionId`; optionally promote subsets to registry-level shared objects.
- **Phase 4 - sandbox generation stamping + concurrency stress suite + flag
  removal + release.**

## Alternatives considered

- **Copy-from-library (reuse by value).** Cheap, no registry, no cross-cube
  atomicity. Rejected as the end state: an edit to the source does not propagate,
  which is exactly the requirement. Its good UX (a dimension library you edit
  independently, then attach) is kept - but "attach" is a *reference* from day
  one, on the registry, so there is never a copy-to-reference second migration.
- **Big-bang full Option A.** Ship references + eager cross-cube repack at once.
  Rejected: the eager N-cube repack is the engine's most expensive op times N and
  must be benchmark-gated; staging it (lazy first) de-risks the rollout.
- **Canonical shared bit layout across cubes.** Would let cubes share packed keys
  but forces one global dimension ordering/width. Rejected: it re-couples cubes
  and fights the existing per-cube `relayout`; sharing identity (not bytes) is
  simpler and already supported.

## Consequences

- Dimensions become server-level objects: create/edit once, reference from many
  cubes, edit propagates. The web gains a dimension library; cube creation can
  reference existing dimensions instead of declaring fresh ones.
- The cost is concentrated and honest: Phase 1's read-path threading is the
  dominant effort; Phase 2's eager repack is the dominant runtime cost and stays
  behind a benchmark gate. The common same-width append remains a no-op.
- ADR-0001/0003/0006 are amended (not superseded), mirroring how ADR-0021 amended
  ADR-0001 for the cube set; ADR-0021 is extended (library-reference mode);
  ADR-0014/0015/0016/0023 are touched (sandbox generation stamp, element-ACL
  re-key).
- Sharp edge to surface in the UI: editing a shared dimension changes every
  referencing cube, including its rollups and element security. This is the
  requested behavior; the UI must make "shared by N cubes" visible before an edit.
- Recovery gains a two-phase load (registry then cubes) and repack-on-load for a
  cube behind the registry generation; the registry is always the durable
  authority, so a lagging cube only ever repacks forward. New failure modes (torn
  dimension append, cube ahead of a loaded registry) get explicit tests.
