# ADR-0036: Dimension structural editing and a cube-agnostic dimension editor

Status: Accepted
Date: 2026-06-18

## Context

A dimension has, until now, been append-only: the engine can add leaves,
consolidations, and consolidation edges, but it cannot reorder members, move a
member to a different parent, change a member's kind, or delete a member. The web
dimension editor reflects that: it lives inside a cube's model page (labelled "for
<cube>"), exposes a Flat/Hierarchy toggle, and edits structure through forms and
drop-downs.

User direction from UAT (2026-06-18):

- The dimension editor should be a **standalone, single-dimension** surface with no
  association to any one cube.
- It should be **drag-and-drop** and **table-driven** (no drop-downs): drop a member
  **before**, **after**, or **as a child of** another, where dropping as a child
  converts a numeric or string member into a consolidation. Each row also offers
  right-click **add before / add after / add as child**.
- It should **always show the hierarchy** (no flat view). Flat / hierarchy / leaves
  belong to the **set editor**, a separate surface for configurations of a
  dimension (subsets), not to the dimension editor.
- Member kinds read as **Numeric / String / Consolidation**.

The hard constraint: a cube stores cells keyed by element **index** (a `Vec<u32>`
coordinate), and the element list is ordered, so an element's position is its
index. Reordering, inserting at a position, or deleting a member therefore changes
indices and would orphan or misplace stored cells unless they are remapped.

## Decision

**1. One source of truth: element order is the index; structural edits remap
cells transactionally.** Rather than thread a parallel "display order" through
every reader (pivot, MDX, member lists, Excel), the element `Vec` stays the single
ordering authority. Each structural edit that changes indices computes the
old-index to new-index permutation (and the set of removed indices) and applies a
**transactional cell-coordinate remap** to the owning cube(s): every stored numeric
and string cell has its coordinate component for that dimension remapped, cells at
a removed index are dropped, and edges referencing a removed element are removed.
The edit is staged on a clone, validated, and committed atomically (the existing
commit path), so a rejected edit changes nothing. Readers are unchanged: they keep
iterating the element `Vec`, which is now in the edited order. The remap is
`O(cells)` per edit, which is fine for an interactive editing operation (not a hot
read path).

**2. New engine structural operations**, each gated by `Dimension:Write` and
element security, audited, and transactional:

- `reorder_elements(dim, new_order)`: permute the element list to `new_order` (a
  permutation of the existing members), remapping cells. Drag-and-drop "place
  before/after" is a reorder.
- `reparent_element(dim, child, new_parent | none)`: change which consolidation a
  member rolls up to (an edge change; no index change, so no cell remap). Setting
  a member as a child of a numeric/string member first **converts** that target to
  a consolidation (decision 3). `none` detaches the member to a root.
- `set_element_kind(dim, element, kind)`: convert a member's kind. A
  numeric/string member becomes a **consolidation** automatically when it gains a
  child; converting a member that holds stored leaf values to a consolidation
  drops those values (a consolidation is computed, not stored) and is surfaced in
  the confirm step. A consolidation converts back to numeric/string only when it
  has no children. numeric and string convert between each other (the member's
  cells are re-typed; an incompatible existing value is cleared, surfaced in the
  confirm step).
- `delete_element(dim, element)`: remove a member, its edges, and its cells, then
  reindex (the only delete; append-only is replaced by full structural editing).
  Deleting a consolidation that still has children is rejected (detach or delete
  the children first), so a delete never silently orphans a subtree.

A new `insert_element_at(dim, spec, position)` covers right-click "add
before/after/as-child" by appending then reordering (or inserting directly),
remapping cells for the index shift.

**3. Shared dimensions fan out.** For a registry-backed dimension (ADR-0024/0031)
referenced by several cubes, a structural edit applies to the registry generation
and **fans the same remap out to every referencing cube** (the `grow_dimension`
fan-out path, extended to permutations/removals), so all materialized copies and
their cells stay consistent. The edit holds the `dim_topology` lock before any
per-cube `writer`, preserving the ADR-0024 lock order. A cube-embedded
(non-registry) dimension edits only that cube.

**4. Persistence and model-as-code.** The element order, kinds, and edges already
round-trip through the model-as-code text (ADR-0003); the new edits change those
in place, and the cube snapshot's remapped cells persist through the normal
checkpoint. No new on-disk format; a structural edit is just a new committed
version.

**5. A cube-agnostic standalone dimension editor (web).** Selecting any dimension
(registry or cube-embedded) opens a dedicated `DimensionEditor` for that one
dimension, with no cube labelling or cube tabs. It is **hierarchy-only** and
**table-driven**: rows are draggable (drop indicator shows before / after /
as-child); a drop as-child converts the target to a consolidation; each row has a
right-click menu with add before / add after / add as child, convert kind, and
delete. Kinds read Numeric / String / Consolidation. Edits call the new ops and
reflect the new committed version. A destructive edit (delete, or a convert that
drops values) confirms first. A cube-embedded dimension that is edited still edits
its cube's copy; promoting to the registry is unchanged.

**6. The set editor is separate.** Flat / hierarchy / **leaves** view modes move
out of the dimension editor and into the **set editor** (subset configuration),
which is where slicing a dimension into a named set belongs. The dimension editor
no longer offers a flat toggle.

## Consequences

- Dimensions become fully editable structurally, matching how modelers expect to
  shape a hierarchy, while stored cells stay correct because every index-changing
  edit remaps them atomically.
- The blast radius is real: new engine ops + cell remap, persistence, the
  shared-dimension fan-out, the security/audit gating, a new web editor, and the
  set-editor split. Done in gated phases.
- A structural edit is `O(cells)` for the affected cube(s); acceptable as an
  editing action, and far cheaper than re-threading a display order through every
  reader.
- Converting a member to a consolidation or deleting it can drop stored data; the
  editor confirms before such edits, and the ops are transactional so a rejected
  edit changes nothing.

## Drag-and-drop interaction model (research-grounded follow-up)

A study of hierarchical drag-and-drop editing (Nielsen Norman Group drag-and-drop
guidance; the W3C WAI-ARIA Authoring Practices tree and treegrid patterns + WCAG
2.2 SC 2.5.7 Dragging Movements; the dnd-kit and react-beautiful-dnd keyboard
models; and the Oracle shared-member / alternate-hierarchy model) settles the
editor's gestures:

- **Three drop zones per row**: the top third inserts the dragged member BEFORE the
  target (an insertion-line indicator), the bottom third inserts it AFTER (insertion
  line), and the middle third nests it AS A CHILD of the target (a container
  highlight, not a line). The active hit area extends slightly past the visible
  zone, and hovering a collapsed consolidation expands it so its children become
  drop targets.
- **Within vs into**: before/after among siblings is a reorder; the middle zone is a
  nest. Distinct indicators (line vs highlight) keep the two unambiguous.
- **Into is additive (share), not move**: dropping a member into a consolidation
  ADDS that parent edge and keeps the member's existing parents (the Oracle
  shared-member model for alternate hierarchies), matching decision-2 `AddChild`.
  A true move is "remove from the old parent, then add to the new".
- **Out of a consolidation removes ONE edge, not the member**: dragging a member out
  (to the root area) or the row action "Remove from this consolidation" removes just
  that parent edge, leaving the member, its data, and its other parents intact; if
  it was the member's last parent it becomes a root. This is distinct from Delete
  (which removes the member) and from "Detach to top level" (which removes all of a
  member's parent edges at once). It needs a `RemoveChild { parent, child }`
  structural op.
- **Delete is intent-aware (remove from a parent vs delete from the dimension)**:
  Delete on a member that is a child of one or more consolidations does not delete
  immediately; it offers a choice. "Remove from consolidations" opens a popup listing
  every consolidation the member belongs to, each with a checkbox, and unlinks only
  the checked edges (one `RemoveChild` each), keeping the member and all its data.
  "Delete from dimension" is the destructive `delete` op: it removes the member, every
  consolidation membership, and all data stored on it, behind an explicit "all data
  will be lost" confirm. A root member, having no parents to choose between, takes the
  "delete from dimension" path directly. This makes the common intent (stop a member
  rolling up into a consolidation) non-destructive and the rare intent (erase the
  member) deliberate.
- **Keyboard parity is mandatory (WCAG 2.2 SC 2.5.7, Level AA)**: every drag gesture
  has a non-drag equivalent in the row's context/overflow menu (move before / after,
  add as child, remove from this consolidation, detach, convert, delete). The tree
  is a single tab stop with roving tabindex; Right expands/into, Left collapses/to
  parent; a draggable row carries `aria-roledescription` and `aria-describedby`
  instructions; keyboard pick-up/drop is Space then arrows then Escape-to-cancel.
- **Safeguards**: a drop that would create a cycle is rejected with feedback (the
  engine already rejects it); destructive edits (delete, a convert that clears
  values) confirm; each edit is a normal committed version, so revert is by editing
  back (undo is deferred).

Status: shipped. The additive `AddChild` (into), before/after reorder (within),
`RemoveChild` (out of one consolidation, by drag-out or the row menu), the
intent-aware Delete (remove-from-consolidations popup vs delete-from-dimension), and
the keyboard/indicator polish are all implemented.

## Deferred

Undo/redo of structural edits (each edit is a normal version, so external revert
is by editing back); bulk reorder by a sort key; cross-dimension moves; a
structural-edit "dry-run" preview of how many cells a delete would drop (the
confirm step states the rule, not the exact count); and a two-phase
validate-every-referencing-cube-before-publish for a shared-dimension edit (today
the registry generation publishes first and per-cube application is best-effort
with no rollback, matching the `grow_dimension` posture).
