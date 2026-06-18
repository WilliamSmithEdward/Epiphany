# ADR-0031: Global dimension namespace (one list, no shared/local split)

- **Status:** Accepted (design lock; Phases 0 and 1 realized)
- **Date:** 2026-06-17
- **Deciders:** Epiphany maintainers
- **Phase:** Post-roadmap (model architecture)
- **Extends:** ADR-0024 (shared, independent dimensions). **Touches:** ADR-0021
  (model-editing), ADR-0015/0023 (element and object security), ADR-0020 (web IA).

## Context

ADR-0024 gave the server a dimension registry: a dimension can be registered
once and referenced by many cubes, and a grow fans out to every referencing
cube. It shipped as materialized references (the cube keeps owning its
`Vec<Dimension>`; the registry is the source of truth and the fan-out target),
which avoided the read-path rewrite.

That left two kinds of dimension visible to the user with different mental
models: a cube-embedded dimension (created as part of a cube, ADR-0021) and a
registry, or "shared", dimension (created in the library, then attached). The
web reinforced the split: the tree's top-level "Dimensions" section listed only
the registry (via `GET /dimensions`), so on a fresh install where every
dimension is cube-embedded it showed nothing, while the actual dimensions hid
under each cube. Buttons, badges, and copy said "shared dimension" throughout.

The user's requirement is that there be no such distinction: every dimension is
a first-class object of the application, all of them are listed together, and
any of them can be used to build any cube. The split is an implementation
artifact, not something a modeler should have to think about.

## Decision

Present a single **global dimension namespace**. There is one "Dimensions" list
for the whole application, and the words "shared" and "library" leave the user
surface: a dimension is just a dimension. Technically this is a presentation and
identity-exposure layer over the ADR-0024 registry, not a new storage model.

1. **One list = the union.** The global Dimensions list is the union of the
   registry dimensions and every cube's dimensions. A cube dimension that is
   registry-backed is shown once, as its global dimension (with the cubes that
   reference it); a cube dimension that is not yet in the registry is shown as a
   distinct global dimension carrying its originating cube as provenance.
2. **Identity is exposed.** A cube's dimension in the cube-detail response
   carries `id: Option<DimensionId>`: `Some(id)` when it is registry-backed,
   `None` when it is cube-embedded only. This is the single backend change and is
   purely additive (older clients ignore the field). It lets the web de-duplicate
   the union and route a click to the right editor.
3. **Non-merging stays the default (from ADR-0024).** Two cubes that each embed a
   same-named dimension are two distinct global dimensions; nothing is merged by
   name. "Global" means application-level and listed together, not deduplicated.
   Merging remains an explicit, opt-in operator action and is out of scope here.
4. **New work is global by default.** Creating a dimension creates a global
   dimension (a registry entry, exactly today's `register_dimension`). Cube
   creation references global dimensions. Existing cubes are not migrated
   (new-cube-only); their embedded dimensions are surfaced in the global list and
   stay cube-owned and append-only, as before.
5. **Security and the read path are unchanged in this phase.** Element ACLs stay
   keyed by `(cube, dimension, element)` and the cube read path keeps resolving
   identity from the cube's own dimensions. The ADR-0024 Phase 3 re-key to
   `(DimensionId, element)` (so a deny on a global dimension's element applies in
   every referencing cube), with a **fail-closed** default for that global
   element scope, remains the documented follow-up and is not part of Phase 0.

### Phasing

- **Phase 0 (this delivery): unify the surface, additively.** Expose
  `DimensionDto.id`; the web lists the union as one global namespace, routes a
  click to the registry editor (for registry-backed dimensions) or the cube model
  editor (for embedded-only ones), and drops every "shared"/"library" label in
  favor of "dimension"/"global dimension". No storage, security, or read-path
  change. This fixes the empty-section bug and removes the distinction the user
  objected to.
- **Phase 1 (done): promote an embedded dimension into the registry.** A
  one-click "Make global" (tree action `promote-dimension` ->
  `POST /cubes/{cube}/dimensions/{dim}/promote`) registers the cube's embedded
  dimension and attaches the originating cube to it (back-reference recorded, no
  duplicate, append-only and idempotent), so an existing dimension becomes
  referenceable by future cubes without a data migration. The cube keeps its own
  data unchanged; only the dimension's identity becomes global. Promoting an
  already-global dimension is a 409.
- **Phase 2+ (deferred, from ADR-0024):** `DimensionId`-keyed, fail-closed
  element security; optional pure-reference read path; eager cross-cube repack
  behind the benchmark gate.

## Alternatives considered

- **Auto-merge same-named dimensions into one global dimension.** Gives a
  literally single "Region" across cubes. Rejected for now: it silently rewrites
  existing cubes onto a shared identity (a real data migration with surprising
  cross-cube effects, exactly what ADR-0024 made opt-in), and contradicts the
  approved non-merging default. The union view delivers "one list" without it.
- **Migrate every embedded dimension into the registry on load.** Would make all
  dimensions truly registry-backed. Rejected as Phase 0: it is the
  blast-radiused, non-additive change ADR-0024 staged deliberately, and is not
  needed to remove the user-facing distinction. Offered instead as opt-in promote
  (Phase 1).
- **Leave the split, just relabel.** Cheapest. Rejected: the section would still
  be empty on a fresh install and the two editors would still behave differently,
  so the distinction would persist in practice.

## Consequences

- The Dimensions section is never empty when any cube exists, and a modeler sees
  one global list instead of a per-cube scattering plus an empty library.
- The change is additive and reversible: one optional DTO field plus web
  composition and copy. No format bump, no migration, no security change, so it
  carries none of the ADR-0024 Phase 1 read-path risk.
- The split is not fully gone under the hood: an embedded-only dimension is not
  yet referenceable by another cube until promoted (Phase 1). The UI states this
  plainly (provenance shown) rather than implying a capability that is not there.
- Validation: backend tests assert cube-detail carries `Some(id)` for a
  registry-backed dimension and `None` for an embedded-only one, and that
  promoting an embedded dimension makes cube-detail report its id, lists it in
  the library, lets another cube reference it, and 409s on a second promote; that
  promote rejects an unknown cube/dimension (404), a caller with no Dimension
  grant (403), a Cube:Read-only caller (403), and an element-restricted caller
  (403); the web union, routing, and promote flow are verified against the demo
  model; `cargo fmt`, `clippy`, the Rust suite, and the web typecheck/lint gates
  stay green.

## Known limitations and follow-ups (post-merge adversarial review)

A multi-lens review after this work landed confirmed the points below. The
genuine defects (promote element-security bypass, promote requiring only
Cube:Read, the wrong-dimension tab, and the swallow-all-errors union loader) were
fixed in place. The rest are inherent to the v1 materialized-reference model
(ADR-0024) and are recorded here rather than papered over:

- **Element security on global dimensions** is per-cube `(cube, dim, element)`,
  so a registry dimension referenced by N cubes has no single masking context:
  `GET /dimensions/{id}` returns the full element list to any global
  `Dimension:Read` holder, even members an element ACL hides from them on a
  referencing cube. Promotion now denies an element-restricted caller (so it
  cannot create new exposure), but the standing gap is resolved only by the
  deferred ADR-0024 Phase 3 re-key to `(DimensionId, element)` with a fail-closed
  default (a union of the referencing cubes' masks, or a `Dimension:Admin` gate on
  enumeration). Tracked there.
- **Attributes are not carried to referencing cubes.** `to_dimension_def` and the
  grow fan-out move only members and hierarchy, so a cube that references (or is
  created from) a promoted dimension does not receive its attribute defs/values,
  and a cube-local attribute edit on a backed dimension is not divergence-guarded.
  The promote copy says "members and hierarchy" accordingly. Carrying attributes
  end to end (def + fan-out + reconcile) is an ADR-0024 follow-up.
- **The global namespace allows duplicate names** (non-merging by design), and
  backing is resolved by `(cube, name)`. This is safe because a cube cannot hold
  two dimensions of the same name (so it cannot reference two same-named registry
  dimensions); two distinct cubes each owning a "Region" are simply two global
  entries. A future id-on-cube-dimension link would remove the name dependency.
- **Cube model-as-code export re-localizes a promoted dimension.** A cube file
  embeds its dimensions inline with no `reference = <id>` marker, so exporting a
  single cube and re-importing it into another deployment loses the registry
  linkage (within one data directory the registry index reconciles references on
  load, so a restart is fine). Recording per-dimension backing in the cube's
  model-as-code is the follow-up.
- **Unsaved-edit guard coverage.** Only the rules pane reports dirty state to the
  tab/navigation discard-guard; the Flows/Views/Schedules editors do not, so a tab
  switch can drop in-progress input there (the New-Cube wizard is a modal whose
  focus trap already blocks a background tab click, so it is not exposed). Wiring
  `onDirtyChange` through the remaining editing workspaces is a follow-up; it
  predates this work (the guard shipped rules-only with the tree IA).
- **Explorer Dimensions fan-out.** Listing the union fetches each readable cube's
  full structure via `getCube` just to read dimension names; a lightweight
  names+backing endpoint would avoid the N heavy reads. The per-request backing
  cost was reduced here to a single registry pass.
