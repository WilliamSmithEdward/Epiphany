# ADR-0023: Modular per-object-kind permissions (roles for users and groups)

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Epiphany maintainers
- **Phase:** Post-roadmap (security model)
- **Supersedes:** the object-grant model of ADR-0015 and the cube-grant model of
  ADR-0016 (the element-security / `ElementMask` half of ADR-0015 is retained).

## Context

Today authorization is coarse for editing. The four-level lattice
(`None < Read < Write < Admin`), `ObjectKind` (Cube, Dimension, Rule, Flow, View,
Subset, Job, Connection, Sandbox, Group, User), per-subject `AccessList` grants
(user or group, max-wins), and global cube grants with deny (ADR-0016) all exist.
But every cube-scoped *edit* - rules, flows, dimensions, attributes, subsets,
views, jobs, and cell data - is gated at one level: **cube `Write`**. The
per-object `require_access(ObjectKind)` helper is wired up only for connections.

So two things a real deployment needs are not expressible:

1. **A modeler distinct from a data-entry user.** Anyone with `Write` on a cube
   can both enter numbers and rewrite that cube's rules, flows, and dimensions.
2. **Narrow roles per object type.** For example "a group that can create and run
   flows" but not change dimensions or write arbitrary cells.

The capability to express these is half-built (the kinds and the subject grants
exist); it is the enforcement granularity that is missing.

This is a pre-release library, so **no backward compatibility is required**: the
grant model and its on-disk format can be restructured cleanly.

## Decision

Make permissions **modular per object kind**, granted to users and groups, and
enforce them per kind at every endpoint. Keep the existing `Read/Write/Admin`
lattice (no separate verb matrix - simplicity was the explicit ask).

### The grant

A permission grant is `(subject, scope, kind, level)`:

- **subject**: a `User(name)` or a `Group(name)`.
- **scope**: `Global` (all cubes) or `Cube(name)`.
- **kind**: an `ObjectKind` - `Cube`, `Dimension`, `Rule`, `Flow`, `View`,
  `Subset`, `Job`, `Connection`, `Sandbox`. (`User`/`Group` are not grantable
  kinds; managing principals stays server-admin-only.)
- **level**: `Read`, `Write`, or `Admin`. Absence of a grant is `None`.

A **role is just a group with a bundle of grants** (e.g. a "Flow authors" group
holding `Flow:Write` at `Global` or on specific cubes). No separate role object is
introduced.

### Resolution (fail-closed)

`effective(principal, kind, cube)`:

1. Server admin (`is_admin`) -> `Admin` (absolute bypass; an admin is never
   locked out - recoverability beats deniability, as in ADR-0016).
2. Otherwise the **maximum** level over the principal's own grants and their
   groups' grants whose `kind` matches and whose scope is `Global` or
   `Cube(cube)`.
3. Plus the **cube-admin implication**: a `Cube:Admin` grant (at `Global` or
   `Cube(cube)`) confers `Write` on every cube-scoped kind within that cube, so
   "admin of a cube" means full control of its contents without enumerating every
   kind. A `Cube:Admin` at `Global` also confers cube lifecycle (create/delete
   any cube).
4. Otherwise `None`. **Fail-closed**: no grant means no access.

Resolution is re-computed live per request (a revoked grant takes effect without
re-login), as today.

### How each operation maps

- **Read anything in a cube** (structure, cells, rule/flow/job listings,
  previews, explain) -> `Cube:Read`. One read grant lets you look at the whole
  cube; element-level masking (ADR-0015, retained) still hides denied members.
- **Write cell data** -> `Cube:Write`.
- **Edit a model object of kind K** (create/update/delete a rule, flow, view,
  subset, job, or dimension members/attributes) -> `K:Write` (or `Cube:Admin`).
- **Run a flow** -> `Flow:Write` to launch it, **and the flow's effects are
  authorized as the runner** (a flow is never a privilege-escalation path). After
  the flow produces its staged outcome, before anything is applied: if it adds
  elements/edges it requires `Dimension:Write` on the cube; if it writes cells it
  requires cube write and that every target cell is element-writable by the
  runner. So `Flow:Write` lets a user author and launch flows, but a flow can only
  ever edit a cube or dimension the runner could edit by hand. The same check
  governs the guided CSV import. (The scheduler runs admin-defined jobs as a
  trusted system actor, ADR-0013; a manual job kick is gated by `Job:Write`.)
- **Create a cube** -> `Cube:Admin` at `Global`. **Delete a cube** -> `Cube:Admin`
  at `Global` or on that cube. (The delete operation and its hard-vs-archive
  semantics are designed separately; this ADR only fixes who is allowed.)
- **Manage a cube's grants** -> `Cube:Admin`.
- **Connections** -> `Connection:Write` / `Connection:Admin` (global).
- **Users, groups, server ACLs, audit** -> server admin only (unchanged).

Result: a data-entry user (`Cube:Write`) cannot alter the model; a flow author
(`Cube:Read` + `Flow:Write`) can build and run flows only; a modeler
(`Cube:Read` + `Dimension:Write` + `Rule:Write`) shapes structure and logic; a
cube manager (`Cube:Admin` at `Global`) owns cube lifecycle - all below server
admin.

### Storage

Grants serialize as a flat, additive list in the existing `security.model`
artifact: one `[[grant]]` per `(subject_kind, subject, scope, cube?, kind,
level)`. This replaces `object_acls` and the ADR-0016 cube-grant tables in one
clean format (pre-release, so no migration). Element ACLs (`element_acls`) are
unchanged. The deny capability of ADR-0016 is not carried forward as a separate
mechanism; fail-closed + explicit grants cover lockout (an explicit per-kind deny
can be added later under its own ADR if a real need appears).

## Alternatives considered

- **A finer Create/Read/Update/Delete/Execute verb matrix per kind.** More
  precise but more concepts and more grants to manage; rejected for "keep it
  simple" - `Write` already means full CRUD of a kind, and flow `run` folds into
  `Flow:Write`.
- **A one-off "cube manager" right** (the narrower thing first asked about).
  Subsumed by this scheme: it is just `Cube:Admin` at `Global`. Building the
  general scheme avoids a pile of special-case rights.
- **Keep cube `Write` as the editing gate and add only a cube-create role.**
  Rejected: it does not separate modeler from data-entry, which is the core need.
- **Carry ADR-0016 deny + tier precedence into every kind.** Rejected for
  simplicity; fail-closed plus admin bypass is enough for a pre-release, and deny
  can return later if warranted.

## Consequences

- The web Security admin UI gains a per-kind grant editor: pick a subject (user
  or group), a scope (all cubes or one cube), a kind, and a level - the "roles"
  surface. Common bundles (data entry, flow author, modeler, cube manager) can be
  offered as presets.
- Enforcement moves from one `require_cube_access(Write)` to a
  `require_kind_access(scope, kind, level)` at each editing endpoint; reads stay
  at `Cube:Read`; element security is unchanged.
- Acceptance: a suite proves the matrix - a `Cube:Write`-only user is denied rule
  and flow edits (403); a `Flow:Write` group member can create and run a flow but
  is denied dimension edits and raw cell writes outside a flow; a `Dimension:Write`
  user can add members but not edit rules; a `Cube:Admin` user can do all of a
  cube and manage its grants; a `Global Cube:Admin` user can create and delete
  cubes; a server admin bypasses everything; every denial is audited.
- This supersedes the object-grant and cube-grant models; the ADR index and the
  security sections of the docs are updated. Element-level security and the audit
  stream are retained as-is.
- Because the model is uniform across kinds, when dimensions become independent,
  shared objects (the in-flight shared-dimension design) they slot in as a kind
  with `Global`/`Cube` scope and need no new authorization concept.
