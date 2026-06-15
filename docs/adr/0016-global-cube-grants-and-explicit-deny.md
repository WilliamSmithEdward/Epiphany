# ADR-0016: Global cube grants and explicit deny

- **Status:** Accepted (model locked; realized in m8.2 as a Phase 7 security extension)
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 7 extension (object security), built after Phase 8

## Context

[ADR-0015](0015-object-and-element-security.md) locked an additive,
most-permissive-wins authorization model with **no explicit-deny** and **no
scope broader than a single named object**, and deferred both on purpose: "an
explicit-deny facility, if ever needed, is a future ADR" and "most-permissive-
wins is the conservative generalization." Two operational needs are now unmet:

- **Broad baseline access.** Granting a user or group Read on every cube takes
  one grant per cube; there is no "all cubes" scope. At any real cube count this
  is unmanageable and a footgun: a newly created cube is invisible to a baseline
  reader until someone remembers to grant it.
- **Exceptions to a broad grant.** With only additive allows, you cannot say
  "Read everything except this one cube" or "deny this group here." A narrower
  grant can only add access, never carve out an exception.

The closed-by-default posture (ADR-0015 decision 2a, amended in m8.1) made the
*absence* of a grant safe, but it added neither a broad scope nor a way to
express exceptions. This ADR adds both, for **cube access**, without disturbing
the element-security model (deny-the-rollup masks) or the owner/visibility model
for subsets, views, and sandboxes.

Four forces shape the decision:

- **No regression / strict superset.** With no global grants and no denies
  defined, cube access must resolve exactly as it does after m8.1: closed-by-
  default (or open by opt-in), plus per-cube allows and admin bypass. The new
  tiers are purely additive.
- **Predictable precedence.** An operator must reason about overlapping grants
  without surprise. The rule has to be a small, total function of `(admin?,
  specific allow/deny, global allow/deny, default posture)`.
- **Determinism ([ADR-0009](0009-determinism-strategy.md)).** Resolution stays a
  pure function of sorted store state and the principal: no wall clock, no RNG.
- **Backward-compatible artifact ([ADR-0003](0003-model-as-code-serialization.md),
  ADR-0015).** Existing `security.model` files must load and re-serialize
  unchanged; new grants are additive `serde(default)` rows and the format tag is
  unchanged. The bounded change layers onto the existing cube-allow path
  (`object_acls` + `set_object_access` + the per-cube REST), which keeps working.

## Decision

**1. Two scopes for cube access: global and specific.** A cube grant carries a
scope: **global** (every cube) or a **specific** named cube. Specific-cube
allows continue to live in the existing per-object ACL (`object_acls` keyed by
the cube), unchanged. Global allows and all denies (global or specific) are new
state. No other object kind and no element gains a global scope in this
increment; the broad-baseline and exception needs are cube-access needs and are
solved there.

**2. Explicit deny.** A grant has an **effect**: `allow` (carries a level Read,
Write, or Admin) or `deny` (carries no level; it is a full denial of cube
access). Deny exists at both scopes: deny a subject on one cube, or deny a
subject across all cubes.

**3. Resolution: most-specific tier wins; deny wins within a tier; admin bypass
is absolute; the default posture is the floor.** For a non-admin `username` and a
`cube`, effective access is the first that applies:

1. **Specific tier.** If the subject is denied on this cube -> `None`; else the
   specific-cube allow, if any.
2. **Global tier.** If the subject is denied across all cubes -> `None`; else the
   global allow, if any.
3. **Default posture.** `Write` if the deployment opted cubes open
   (`EPIPHANY_DEFAULT_CUBE_ACCESS=open`), else `None`.

An admin always resolves to `Admin` first; a deny never applies to an admin (see
decision 4). "The subject is denied/allowed" combines the principal's direct
user entry and every matching group entry in sorted order: within a tier an
allow is the `max` over matching entries, and a deny on any matching entry wins
over an allow at that same tier. The function is total and independent of
insertion order.

Worked outcomes (the cases that motivated this):

- Global allow Read + deny on cube X -> Read everywhere, `None` on X.
- Global allow Read + specific allow Write on cube Y -> Read everywhere, Write
  on Y.
- Global deny on group G + specific allow Read on cube Z for G -> `None`
  everywhere, Read on Z. Specificity wins across tiers, so a per-cube allow
  overrides a global deny.

**4. Admin bypass is absolute; a deny never locks out an admin.** A deny applies
only to non-admins. Admins are the parties who manage grants; honoring a deny
against an admin would create an unrecoverable lockout with no one left who can
clear it. This is the deny-versus-admin-bypass question ADR-0015 flagged; we
resolve it in favor of an always-recoverable system. Deleting a cube and
managing its grants stay admin-only regardless of any grant, including a global
Admin allow.

**5. A global Admin allow is permitted and is effectively a cube-administrator
role.** A global allow may carry any level, including Admin. A non-admin with a
global Admin allow can manage grants on and delete any cube; this is a coherent
delegated-administrator role and is the operator's explicit choice, never a
default. It does not confer user or group administration, which is gated
separately.

**6. Serialized as additive model-as-code.** A new `[[cube_grant]]` array in the
security artifact carries global allows and all denies:

```toml
[[cube_grant]]
# cube omitted = all cubes (global scope)
subject_kind = "group"   # "user" | "group"
subject = "analysts"
effect = "allow"         # "allow" | "deny" (default "allow")
level = "read"           # required for allow; absent for deny

[[cube_grant]]
cube = "Salaries"
subject_kind = "group"
subject = "analysts"
effect = "deny"
```

Specific-cube allows are **not** duplicated here; they stay in `[[object_acl]]`.
Loading is total and tolerant: an unrecognized subject kind, level, or effect is
skipped, never fatal, consistent with ADR-0015 decision 6. Existing files have
no `[[cube_grant]]` rows, so they load and re-serialize byte-identically.

**7. One resolution point, unchanged callers.** The new tiers fold into
`SecurityStore::cube_access`, the single method the API already calls to gate
cube reads, writes, rules, flows, and cells. No handler changes its gating call;
rules, flows, and cells inherit the new behavior because they already inherit
`cube_access`. New admin REST sets and lists cube grants (global and deny); the
existing per-cube allow endpoint is unchanged.

## Alternatives considered

- **A full rewrite to one unified grant table (scope x effect for every object
  kind).** Rejected for this increment: it would migrate `object_acls` and
  `element_acls`, touch every resolution path, and risk regressions, to solve a
  need that is specifically about cube baseline access. Layering global+deny onto
  `cube_access` is the bounded change; a unified table can be a later ADR if
  other kinds need it.
- **Least-permissive / intersection resolution** (every matching grant must
  allow). Rejected: it is not a superset of today's additive model and would
  tighten working paths, and it makes "broad allow with a few exceptions"
  awkward (you would grant the exceptions, not deny them). Most-specific-wins
  with explicit deny expresses operator intent directly.
- **Deny overrides admin.** Rejected: it enables an unrecoverable lockout.
  Recoverability (an admin can always fix access) outweighs the ability to deny
  an admin, which an operator achieves by not making them an admin.
- **Allow-wins within a tier** (an allow overrides a deny at the same scope).
  Rejected: deny-wins is the conventional, safer reading of an explicit deny and
  matches operator expectation ("I denied this group here" should hold even if a
  member also has an allow at the same scope). Cross-tier, specificity still lets
  a per-cube allow override a global deny.
- **A global scope for elements too.** Deferred: element security is a different
  hot-path mechanism (deny-the-rollup masks) and the broad-baseline need has not
  arisen there; adding it now would broaden scope without a driver.

## Consequences

- New `SecurityStore` state (a global allow list, a global deny set, a per-cube
  deny map), a rewritten `cube_access` with a documented three-tier resolution,
  set and list methods, a `[[cube_grant]]` serialization row with tolerant load,
  admin REST and a web admin surface, and audit emission on grant changes
  ([ADR-0010](0010-audit-logging.md)).
- Strict superset: with no global grants or denies, `cube_access` is identical to
  m8.1; every existing test stays green and existing artifacts are byte-stable.
- Determinism and performance hold: resolution is sorted-map lookups and set
  membership over the principal's groups, no wall clock or RNG, O(groups) per
  check, and the new state is empty (zero cost) when unused.
- The precedence rule (most-specific-wins, deny-wins-in-tier, admin-absolute) is
  the load-bearing, contestable decision; it is pinned by a unit resolution
  matrix and an end-to-end acceptance test (broad read + per-cube deny + per-cube
  write + group-based + admin-over-deny) and re-checked by an adversarial review
  before the m8.2 tag.
- Out of scope, each needing its own ADR if pursued: global and deny grants for
  non-cube objects and elements, time-bounded or conditional grants, and a
  unified grant table.
