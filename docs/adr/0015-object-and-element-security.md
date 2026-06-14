# ADR-0015: Object and element security model

- **Status:** Accepted (model locked; realized across Phase 7 increments 7B-7I)
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 7 (object and element security)

## Context

Phase 7 makes the server multi-user and least-privilege: a non-admin must see
and edit only the cubes and elements they are permitted, an admin manages access
from the UI and REST, and security-relevant actions are audited
([ADR-0010](0010-audit-logging.md)). Authentication (users, groups, Argon2id)
and a `Principal { username, is_admin, groups }` already exist from Phase 2; what
is missing is per-object and per-element authorization. ADR-0010 records the
audit design; this ADR decides the authorization model it audits against.

Today authorization is open-coded and additive: each handler allows a request
when `is_admin`, or the caller owns the object, or the object is public (for
example `can_read`/`can_modify` on subsets and views, `require_admin` on
connections, `authorize_sandbox` on sandboxes). Rules and flows currently carry
no per-object check at all. Element-level access does not exist. Four forces
shape the decision:

- **No regression.** The single bootstrap admin must keep full access, and with
  no access entries defined every existing path must behave exactly as it does
  today. Generalizing the current additive checks must not tighten a working
  path.
- **Determinism ([ADR-0009](0009-determinism-strategy.md)).** Access decisions
  are observable (they gate output and produce audit records), so they must be
  reproducible: sorted iteration, no wall clock, no RNG.
- **Performance.** Element security sits on the cell read/write hot path, so it
  must cost nothing when no element rules exist and be O(1) per coordinate
  component otherwise, never a lock per cell.
- **Core purity.** `epiphany-core` and `epiphany-calc` must not depend on
  `epiphany-security`; security reaches the calculation engine only through an
  injected value, like the existing `SetEvaluator` and `CellResolver` seams.

## Decision

**1. A four-level lattice.** `AccessLevel` is a totally ordered enum
`None < Read < Write < Admin`, applied uniformly to every securable kind. Read =
see and read (cells, definitions, query results); Write = create, replace,
delete, and run mutating operations on the object (write cells, run a flow,
define rules/flows/subsets/views under a cube, commit a sandbox); Admin = delete
the object and manage its access grants. A securable object is named by a typed
`ObjectRef { kind, cube, name }` over `ObjectKind` (Cube, Dimension, Rule, Flow,
View, Subset, Connection, Sandbox, Job, Group, User). The `Job` kind is reserved
now (serialized, no enforcement) so the Phase 8 scheduler (ADR-0013) adds
enforcement, not the model. Connections keep their stricter rule: Admin (not
Write) is required to define or delete them, preserving today's behavior.

**2. Most-permissive-wins resolution with admin bypass.** A principal's
effective access to an object is the maximum of: Admin if `is_admin` (the bypass,
evaluated first); each matching direct user grant; each matching group grant (the
principal's groups, in sorted order); Write if the principal owns the object; and
Read if the object is public. The grant-based part (admin bypass plus user and
group grants) is resolved by a pure `SecurityStore` method; the owner and public
fallbacks are composed at the API boundary, where the object's owner and
visibility are known from the model snapshot, so `SecurityStore` stays free of
cube-model knowledge. Resolution is a union (max), never an intersection: it is a
strict generalization of today's `is_admin || owner || public` disjunctions, so
it cannot tighten a path that works today. There are **no explicit-deny entries
in v1**: absence of a grant is None and access only accumulates upward. This
locks "admin always wins, grants only add"; an explicit-deny facility, if ever
needed, is a future ADR. (Element security in decision 4 is the one scoped place
a deny-style mask appears.)

**2a. Cubes are open until restricted (no-regression default).** A cube object
carries no owner or visibility, so the grant-only rule of decision 2 would deny
every non-admin on a fresh cube, a regression from the pre-Phase-7 behavior where
any authenticated user could read and write any cube. The cube default is
therefore: a cube with no object grants is "unmanaged" and open to any
authenticated user at Write (read and write cells, define rules/flows/subsets/
views under it), but never Admin (deleting the cube and managing its grants stay
admin-only). The moment an admin adds any grant to that cube it becomes
"managed" and access is exactly the grants (plus admin bypass), so granting one
user Read restricts everyone else. Rules, flows, and cells are cube-scoped and
inherit the cube's level (Write on the cube governs its rules and flows; there is
no separate per-rule owner in the model). Subsets, views, and sandboxes keep
their own owner/visibility model (decision 2). This makes "least privilege"
opt-in per cube while keeping every existing path working with no grants defined.

**3. Access decisions re-resolve per request against the live store.** The check
re-reads the principal's current groups and admin flag from `SecurityStore` by
username each request, rather than trusting the session-captured `Principal`, so
a revoked grant or group change takes effect immediately without forcing
re-login; a user removed entirely is denied. The session remains the
authentication proof; the store remains the authorization source of truth.

**4. Element security: 403, suppress, or deny-the-rollup, chosen by call site.**
Element ACLs restrict specific `(cube, dimension, element)` members for a
principal; a member with no element ACL inherits the dimension/cube decision, so
the common case is free.

- A **directly addressed** denied coordinate (single-cell read, write, or
  explain) returns **403**. The client named that exact cell; emptying it would
  misrepresent the data and dropping a denied write would corrupt the model.
- A denied member on an **axis or member enumeration** (cellsets, subset members,
  preview) is **suppressed** (omitted), like zero-suppression, so a pivot stays
  usable without a storm of denials.
- A **consolidated cell that rolls up any denied leaf is itself denied**
  (deny-the-rollup): 403 when directly addressed, suppressed on an axis. This is
  the only policy that closes the subtraction-inference leak (read a total,
  subtract the visible children, recover a hidden child). Zeroing the denied
  contribution would silently corrupt every total; allowing the rollup leaks the
  aggregate. The contributing leaves are exactly `Dimension::leaf_weights` (a
  deterministic, zero-weight-excluding expansion), so the check is an
  intersection of the coordinate's leaf set with the per-dimension deny mask.

**5. Element security is injected into the calculation engine, never a core
dependency.** The consolidation deny-the-rollup check happens inside
`CalcEngine::compute`, below the API layer. The per-request element mask is
threaded through the existing `CellResolverFactory::resolver_with` seam into the
calc resolver as a plain injected value (a dense per-`(cube, dimension)` allow
bitset indexed by element index), so `epiphany-core` and `epiphany-calc` carry no
security dependency. The mask is built once per request under a single security
lock, then consulted with O(1) array indexing; when no element ACLs apply the
mask is absent and the check is skipped entirely.

**6. ACLs are control objects in the security artifact, serialized as
model-as-code.** Object and element grants are stored in the existing
`security.model` artifact (already the authorization home, already atomic
temp-then-rename TOML), as additive `serde(default)` arrays, so the format tag is
unchanged and existing files load untouched. Elements are referenced by name (not
index) so grants survive reindexing. In memory the store holds indexed maps for
O(log n) resolution and re-emits sorted for byte-stable output. A grant may
reference a cube-model object in a different artifact, so the policy is
**tolerate-dangling-on-load, validate-on-grant**: loading never fails on a grant
whose object no longer exists (load is total), the grant endpoint validates the
object exists against the current snapshot before writing, and a dangling grant
is simply never consulted.

**7. One enforcement point.** A single API helper `require_access(principal,
object, needed_level)` performs the resolution, emits an `AccessDenied` audit
record and returns 403 on failure, and is called before the engine mutation in
every handler, so a denied request never changes state or broadcasts a change.
This collapses the open-coded checks into one implementation and removes the
"new handler forgets the check" footgun.

## Alternatives considered

- **Intersection / least-permissive resolution, or explicit-deny entries.**
  Rejected for v1: it would not match the existing additive checks (risking a
  regression), and deny-precedence-versus-admin-bypass is a genuine design
  question better deferred to its own ADR than rushed into Phase 7. Most-
  permissive-wins is the conservative generalization.
- **Element security by zeroing denied contributions** rather than denying the
  rollup. Rejected: it silently corrupts every consolidation that touches a
  denied leaf, which is worse than an honest denial and violates the principle
  that a shown number is correct.
- **Element security by allowing the rollup** (deny only leaves). Rejected: it
  leaks denied leaf values by subtraction from visible totals.
- **A separate ACL artifact / a new security model format version.** Rejected:
  the additive `serde(default)` approach keeps existing files loading and avoids
  a format-tag bump that would reject them; the security artifact is already the
  authorization home.
- **A core or calc dependency on `epiphany-security`.** Rejected: it would invert
  the layering. Injecting an opaque element mask through the existing resolver
  seam keeps core and calc security-free.
- **Resolving access from the session-captured principal.** Rejected: it makes
  revocation wait for re-login. Re-resolving per request from the live store is
  cheap and correct.

## Consequences

- A new `AccessLevel`/`ObjectRef`/ACL model in `epiphany-security`, two resolution
  methods, an injected element mask reaching the calc engine, a single
  `require_access` enforcement helper, security-admin REST and a web admin UI, and
  audit emission (ADR-0010) at the one denial point plus the lifecycle handlers.
- No regression: with no grants defined, admin keeps full access and every
  non-admin path behaves as today via the owner and public fallbacks; gates are
  additive, so each increment stays green.
- Determinism and performance hold: resolution uses sorted maps and the sorted
  groups list with no wall clock or RNG; the element mask is built once per
  request and is absent (zero cost) when no element ACLs exist.
- The deny-the-rollup policy is the load-bearing, contestable decision; it is
  validated by the Phase 7 acceptance suite (including the subtraction-inference
  test) and re-checked by an adversarial review before the milestone tag.
- Explicit-deny, cross-object inheritance, and regulator-grade controls remain
  out of scope and would each need their own ADR.
