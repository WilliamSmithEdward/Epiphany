# ADR-0033: Fail-closed element security for global dimension reads

- **Status:** Accepted
- **Date:** 2026-06-17
- **Deciders:** Epiphany maintainers
- **Extends:** ADR-0015 (object and element security), ADR-0031 (global dimension
  namespace). **Realizes (differently):** the ADR-0024 Phase 3 intent of element
  security that applies across every cube referencing a shared dimension.

## Context

Element security (ADR-0015) is keyed `(cube, dimension, element)`: an ACL hides
specific members of a dimension *within a cube*, and `GET /cubes/{cube}` masks
those members out of the returned structure (deny-the-name, ADR-0015). Global
dimensions (ADR-0031) added `GET /api/v1/dimensions/{id}`, which returns a
registry dimension's full element list gated only on the global `Dimension:Read`
permission, with no element masking. The post-merge review of ADR-0031 confirmed
the leak: once a dimension is promoted or referenced by a cube, any holder of
`Dimension:Read` can enumerate every member name through the global endpoint,
including members an element ACL hides from them on a referencing cube. The
explorer's dimension node and the dimension editor both read this endpoint, so
the leak is reachable in the UI, not only via raw API.

ADR-0024 Phase 3 envisioned re-keying element ACLs to `(DimensionId, element)`
so a deny applies in every referencing cube. Under the shipped
materialized-reference model (ADR-0024 v1: cubes still own their dimensions; the
registry holds identity + the reverse index), that re-key is a storage-format
change with a migration, and it is not required to close the confidentiality
gap.

## Decision

Mask the global dimension read by the **union of the per-cube element masks of
its referencing cubes**, fail-closed, mirroring how `get_cube` already masks a
cube's structure. When `GET /dimensions/{id}` serves a non-admin caller:

- A member of the dimension is **suppressed** if, in **any** cube that references
  the dimension, an element ACL denies the caller that member
  (`has_element_acls(cube, dim) && !element_readable(principal, cube, dim,
  member)`). The union is the fail-closed choice: hidden in one referencing cube
  means hidden in the global view.
- Suppressed members, and every consolidation edge that touches a suppressed
  member, are dropped from the returned definition (same filtering as
  `cube_detail`).
- **Admins bypass** (consistent with ADR-0015). An **unreferenced** registry
  dimension has no per-cube ACL context, so its members are gated only by the
  global `Dimension:Read` grant (the registry's own trust level); nothing to mask.
- An unknown principal (which `require_kind_access` already rejects upstream) is
  treated as fully denied, for defense in depth.

Element ACLs stay `(cube, dimension, element)`-keyed; no storage change, no
migration. This delivers the user-visible guarantee ADR-0024 Phase 3 wanted (a
member denied on any referencing cube cannot be read through the global
dimension) without the re-key. The optional `(DimensionId, element)` storage,
which would let an admin author one deny that applies everywhere (an admin-UX
convenience, not a confidentiality requirement), remains a possible future
addition behind this same guarantee.

## Alternatives considered

- **Restrict global element enumeration to `Dimension:Admin`.** Simple, but
  over-broad: it would hide all member names from ordinary `Dimension:Read`
  modelers who have every right to see the members not denied to them. Rejected.
- **Full `(DimensionId, element)` ACL re-key + migration (ADR-0024 Phase 3).**
  Achieves the same guarantee plus author-once-deny. Rejected for now as a
  format/migration change not needed to close the leak; the union mask is
  additive and reversible.
- **Leave it (document only).** Rejected: it is a real confidentiality leak that
  the redesigned dimension editor surfaces in the UI.

## Consequences

- The global dimension endpoint no longer leaks element names a caller is denied
  on any referencing cube; the dimension editor and explorer node inherit the
  fix. The cost is a per-request pass over the referencing cubes that carry
  element ACLs (most carry none, so the common case is `has_element_acls` short
  circuiting to nothing); `get_dimension` is a cold structure read, not a hot
  cell path.
- Behavior matches `get_cube`: a member denied in a referencing cube is absent
  from both that cube's structure and the global dimension. A member denied in
  one referencing cube but allowed in another is, conservatively, hidden in the
  global view (the union); the per-cube view still shows it where allowed.
- Validation: an api test asserts a non-admin denied a member on a referencing
  cube cannot see that member via `GET /dimensions/{id}`, an admin still sees the
  full list, and an unreferenced dimension is unmasked; `cargo fmt`/`clippy`/
  `cargo test --workspace` stay green.
