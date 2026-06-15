# ADR-0021: Model-editing API (create cubes, build dimensions, edit attributes)

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Epiphany maintainers
- **Phase:** Post-roadmap web UI overhaul (W-API)

## Context

The server can hold many cubes, each with dimensions, elements, consolidation
hierarchies, and attributes. Until now that structure could only be created two
ways: at boot (open a cube directory, or materialize the demo) and through
model-as-code flows (`ctx.ensureElements(...)`). There is no REST surface that
lets an operator create a cube or shape a dimension directly, so the web client
cannot deliver on "manage all parts of the model": it can read structure and
write cell data, but it cannot author the structure itself.

Three engine facts constrain what is safe to expose:

1. **The cube set is built once at boot.** `Engine.cubes` is an
   `Arc<BTreeMap<String, CubeState>>` (ADR-0001): immutable after `from_stores`,
   which is what makes cube lookups on the hot read path lock-free. There is no
   runtime path to register a new cube.
2. **Schema growth is append-only and repacks cells.** `Cube::extend_schema`
   adds elements and consolidation edges to *existing* dimensions and repacks the
   stored cells to the new per-dimension width (ADR-0006). It rejects unknown
   dimensions. There is no operation to add a new dimension to a populated cube
   (that changes the cube's rank and would rebuild every coordinate), and none to
   rename or delete elements or dimensions.
3. **Element and edge adds are already transactional and durable.**
   `Engine.define_elements` validates on a clone, commits atomically with
   optimistic concurrency (`base: Option<Version>`), and checkpoints the snapshot
   before publishing. Attribute mutation exists on `core::Dimension`
   (`add_attribute`, `set_attribute`) but is not reachable through the engine
   handle.

## Decision

Add a model-editing API in two tiers, matched to what the engine can do safely.
Destructive and rank-changing operations are explicit non-goals for this ADR.

### Tier 1 - additive editing of an existing cube (no concurrency change)

Expose, as authenticated, `Write`-gated, audited REST endpoints over a cube:

- **Add elements and consolidations.** `POST /api/v1/cubes/{cube}/elements` with
  `{ elements: [{dimension, name, kind}], edges: [{dimension, parent, child,
  weight}] }`, wrapping `Engine.define_elements`. Append-only and idempotent;
  rejects unknown dimensions, kind conflicts, non-consolidated parents, and
  cycles, with the cube unchanged on rejection.
- **Define an attribute.** `PUT
  /api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}` with `{ kind }` where
  kind is `text | numeric | alias`. New engine method `define_attribute` wrapping
  `Dimension::add_attribute` through the writer/commit path.
- **Set attribute values.** `PUT
  /api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}/values` with `{ values:
  [{element, value}] }`. New engine method `set_attribute_values` wrapping
  `Dimension::set_attribute`; enforces type match and alias uniqueness; all or
  nothing.

These run through the existing per-cube `writer` mutex and `published` ArcSwap,
so they inherit snapshot isolation, optimistic concurrency, durability
(immediate checkpoint), and the model-as-code round-trip with no change to
ADR-0001.

### Tier 2 - create a new cube (extends ADR-0001)

Add `POST /api/v1/cubes` with `{ name, dimensions: [{ name, elements?, edges? }]
}` to create a brand-new cube, dimensions and initial members declared up front
(a dimension cannot be added later, per constraint 2). New engine method
`create_cube`.

Registering a cube at runtime requires mutating the top-level cube set, which is
immutable today. The change:

- `Engine.cubes` becomes `ArcSwap<BTreeMap<String, Arc<CubeState>>>`. Hot reads
  do `self.cubes.load().get(cube)` - one extra lock-free atomic load, no
  blocking, snapshot isolation preserved. `CubeState` is shared by `Arc`, so a
  new map reuses every existing cube's state.
- A coarse `topology: Mutex<()>` serializes create/registration so two
  concurrent creates cannot lose a cube. Per-cube commits are unaffected and
  never take this lock.
- The engine learns its on-disk root (`cubes_dir`) so `create_cube` can
  `Store::create(<cubes_dir>/<name>, cube)`, matching the boot layout
  (`<data_dir>/cubes/<name>/snapshot.model`), so a created cube reloads on
  restart. Engines built without a root (tests) reject `create_cube` rather than
  persisting nowhere.
- Creating an existing name is rejected (`Conflict`); the cube name must be a
  valid identifier and non-reserved.

The new cube is gated by `ObjectKind::Cube` admin/write authorization and
audited. Element security and global cube grants (ADR-0015/0016) apply
unchanged once the cube exists.

### Non-goals (explicit)

- Adding a dimension to an existing populated cube (rank change / full repack).
- Renaming or deleting elements, dimensions, or cubes; reparenting that removes
  edges. The model is append-only by design (ADR-0006); destructive editing
  needs a cube-rebuild design and its own ADR.
- Bulk element import beyond the existing guided CSV / flow paths.

## Alternatives considered

- **Per-cube mutable map entry (RwLock<BTreeMap>) instead of ArcSwap.** A plain
  `RwLock` would put a lock on the hot read path; readers would contend with the
  rare create. ArcSwap keeps reads lock-free and pays the cost only at create
  time, which fits the read-heavy workload.
- **Model-as-code only (no structural REST).** Keep authoring in flows and have
  the UI generate a flow. Rejected: it forces every structural edit through the
  JavaScript runtime and a CSV/flow round-trip, which is the opposite of the
  "spelled out and simple" goal, and gives no typed validation surface.
- **Full CRUD including delete/rename now.** Rejected for this ADR: it collides
  with the packed, append-only storage model and would be a large, separate
  cube-rebuild project. Shipping additive editing first delivers most of the
  value at a fraction of the risk.
- **A general "apply model diff" endpoint.** More flexible but harder to
  validate, authorize, and audit at a useful granularity. Discrete,
  intent-named endpoints map cleanly onto authorization and audit actions.

## Consequences

- The web client gains a Model section that can create a cube, add members,
  build consolidation hierarchies, and define and set attributes, all with typed
  validation and plain-language errors - the structural half of "manage all
  parts of the model".
- ADR-0001 is extended: the cube *set* is now mutable through a copy-on-write
  ArcSwap behind a coarse topology lock, while per-cube reads and commits keep
  their existing guarantees. This is the one hot-path change and is covered by a
  microbenchmark check that read latency is unchanged within noise.
- Durability and recovery: created cubes and all additive edits checkpoint
  immediately and reload on restart through the existing boot scan; an
  acceptance test creates a cube, adds structure, restarts, and asserts the
  structure persists.
- Validation: an acceptance suite covers create/add/attribute happy paths,
  every rejection (unknown dimension, kind conflict, cycle, non-consolidated
  parent, duplicate cube, alias collision, bad identifier), authorization
  (non-admin/non-writer denied and audited), and optimistic-concurrency
  conflicts. The OpenAPI document and its route-coverage test are updated.
- Non-goals are documented so the absence of delete/rename is a known boundary,
  not a gap; revisiting them is a future ADR.
