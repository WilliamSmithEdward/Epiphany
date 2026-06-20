# ADR-0038: Explicit top-level membership for dimension members

Status: Accepted
Date: 2026-06-19

## Context

A dimension member is, until now, a "top-level" member (a display root) IFF it has
no parent: the web layer computes the roots as
`dim.elements.filter(el => !childSet.has(el.name))` (see
`web/src/model/tree.ts`: `buildElementTree` and `buildForest`). There is no core
helper for roots; the core exposes only the elements and the consolidation edges,
and each reader (the web forest builder is the canonical one) derives the roots
from edge emptiness. MDX `.Children` / `.Descendants` walk explicit parent members
and never need a roots concept.

Modelers want a member to appear at the top level (as a root, e.g. a grand-total
peer or a frequently-watched leaf) EVEN WHILE it also rolls up under one or more
consolidations. Today the only way to make a member a root is to detach it from
every parent, which destroys the rollup. We want an explicit, additive "pin to top
level" that leaves the rollup untouched.

## Decision

**1. A per-element `pinned_to_top` flag.** The flag lives inside the `Element`
struct (`crates/epiphany-core/src/dimension.rs`), not in a separate index set.
Because the element `Vec` is the single ordering authority (ADR-0036), a
per-element flag travels with its element through every structural edit
(reorder permutes it with the element, delete drops the deleted element's flag, a
newly inserted element defaults to NOT pinned) with no extra remapping. A new
element is never pinned by default, so an existing model has zero pinned members.

**2. Display roots = {no parent} UNION {pinned}.** A reader's roots become the
union of members with no incoming edge and members pinned to top. The core still
computes no roots of its own; it stores and exposes the flag, and the web phase
(deferred) unions the pinned set into `buildForest` / `buildElementTree`. A pinned
member that also has a parent is therefore BOTH a display root AND a child of its
consolidation.

**3. Rollup edges and values are UNCHANGED.** Pinning adds no edge and removes
none; `leaf_weights`, the cell store, and consolidation are untouched. The
documented, accepted consequence: a grand total that sums the display-roots can
include a pinned member both at the top (as its own root) and again inside the
consolidation it rolls up under -- a double-count that is intended (the pin is a
display affordance, not a re-parenting). Callers that must avoid the double-count
sum a consolidation member, not the display-roots.

**4. Two structural ops, both idempotent.**
- `PinToTop { element }`: set the flag. Pinning an already-pinned member, or a
  member that has no parent (already a root), succeeds as a no-op.
- `UnpinFromTop { element }`: clear the flag. The member reverts to a display root
  only if it has no parent. Unpinning an unpinned member succeeds as a no-op.

These are exposed as: `Dimension::pin_to_top` / `unpin_from_top` /
`is_pinned_to_top` / `pinned_to_top` (the index list of pinned members); the
index-stable cube ops `Cube::pin_element_to_top` / `unpin_element_from_top`; the
persist `DimensionEdit::PinToTop` / `UnpinFromTop` variants dispatched by
`Store::edit_dimension` to `Store::pin_element_to_top` / `unpin_element_from_top`;
and the registry mirror `SharedDimension::edited`.

**5. Persistence: serde-defaulted text field + a name-addressed WAL record.**
In the model-as-code text (`crates/epiphany-core/src/text.rs`) the flag serializes
on the element document as `top_level = true`, with `#[serde(default,
skip_serializing_if = ...)]`, so ABSENT means not pinned. An existing snapshot
(which never carries the field) loads with zero pinned members and re-serializes
byte-identically, so the change is backward-compatible. The flag round-trips
through the snapshot AND, because a pin is index-stable, through the WAL: a new
`SetPin { dimension, element, pinned }` record (ADR-0002 framing) is addressed by
member NAME rather than index, so it replays safely onto the snapshot regardless of
element order -- exactly like a cell write, and unlike a reindexing edit (reorder /
delete / insert) which must checkpoint. The pin/unpin Store methods apply to the
in-memory cube and append the record; recovery replays it.

## Alternatives considered

- **A separate `pinned: HashSet<u32>` index set on the dimension.** Rejected: it
  duplicates the structural-edit remapping that the element `Vec` already does, so
  reorder/insert/delete would each have to remap the set by hand (the exact bug
  the per-element flag avoids).
- **Checkpoint the pin like the other structural edits (rewrite snapshot + clear
  WAL) instead of a WAL record.** Workable, but a pin is index-stable and tiny;
  logging it as a name-addressed WAL record is cheaper, matches the cell-write
  durability path, and keeps an interactive pin off the full-snapshot path. A
  checkpoint still folds the pin in correctly when one happens for another reason.

## Consequences

- A member can be a display root and a consolidation child at once, which is the
  whole point; the accepted double-count when summing display-roots is documented
  above.
- Roots are still computed in the WEB layer, not the core (this phase only stores +
  exposes the flag). The web phase consumes `pinned_to_top` / `is_pinned_to_top`
  and unions them into the forest; the API phase exposes the flag on the dimension
  DTO and adds the pin/unpin edits to the dimension-edit endpoint.
- Back-compat is total: an old snapshot loads unchanged (no pins), and a model with
  no pins emits no `top_level` field, so its bytes are unchanged.

## Phasing

- **Phase 1 (this ADR, core + persist):** the per-element flag, the
  `pin_to_top`/`unpin_from_top`/`is_pinned_to_top`/`pinned_to_top` getters, the
  cube + persist ops + WAL record, and the text serialization with back-compat.
- **Phase 2 (API):** expose `pinned_to_top` on the dimension DTO and add
  `PinToTop`/`UnpinFromTop` to the dimension-edit request enum.
- **Phase 3 (web):** union the pinned set into `buildForest`/`buildElementTree` and
  add the pin/unpin gestures to the dimension editor.
