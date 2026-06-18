# ADR-0032: Scalable member table for the dimension and set editors

- **Status:** Accepted (design lock; v1 realized)
- **Date:** 2026-06-17
- **Deciders:** Epiphany maintainers
- **Extends:** ADR-0020 (web design system + IA). **Touches:** ADR-0024/0031
  (dimensions), ADR-0021 (model editing).

## Context

The dimension editor renders members as a flat `<ul>` (DimensionsWorkspace
`model-members`) and the set/subset builder renders the available members as a
fully-expanded tree (`ElementTree` in `MemberSetPicker`). Both render every
member to the DOM, neither scales past a few hundred members, and neither shows
a member's attributes. The user needs the editors to handle hundreds to
thousands of members and to show attributes as toggleable columns against each
member row. A cited UI/UX research pass (15 sources across NN/g, Smashing,
web.dev, LogRocket, W3C ARIA APG, and the OLAP vendors IBM Planning Analytics,
Microsoft Analysis Services, Oracle Smart View, Anaplan) produced the design
below; recommendations are kept where >= 3 independent publishers agree, with
single-source patterns flagged.

## Decision

Introduce one shared **`MemberTable`** component, used by both the dimension
editor and the set builder, plus a small in-house **`useVirtualRows`** windowing
hook. No table/grid library is added (dependency-light constraint, ADR-0020).

1. **A real table, as a CSS grid.** Render the table over `display:grid` with
   `grid-template-columns`, so a sticky header row, a pinned left "Member"
   identity column, and the attribute columns all cooperate with virtualization.
   The first column is the human-readable member name, never an internal id
   (NN/g). This is the same CSS-grid approach the pivot `CellsetGrid` uses. v1
   uses **static table semantics (`role=table`/`row`/`columnheader`/`rowheader`/
   `cell`)** because cells are read-only here and the interactive controls
   (checkboxes, sort headers, twisties) are normal tab stops; sortable headers
   still carry `aria-sort`. The interactive `role=grid` with roving-tabindex cell
   navigation is deferred to when inline cell editing lands (declaring `grid`
   without fulfilling its keyboard contract is the trap a prior review flagged on
   the tab strip).
2. **In-house virtualization, gated.** `useVirtualRows` renders only the rows in
   the viewport (plus a small overscan) over a fixed row height, padding with top
   and bottom spacer rows so the scrollbar stays honest. Gated above a threshold
   (200 members) so small dimensions stay plain DOM (mirrors the existing
   `>1024 cells` parallel-aggregation gate). Accessibility under windowing uses
   the W3C APG mechanism: `aria-rowcount` on the grid is the filtered total and
   each rendered row carries its true `aria-rowindex`.
3. **Attributes as toggleable columns.** Attributes are OFF by default (member +
   kind only). A "Columns" menu (the vendored `Menu`, keyboard-accessible, no
   drag) toggles each attribute column and shows how many are hidden. Attribute
   values come from `DimensionDto.attributes[].values[member]`. An `alias`
   attribute can also re-label the member column ("Show by: Name | <alias>").
4. **Search and sort are the primary navigation.** An always-visible search box
   filters live, supports `*`/`?` wildcards (translated to a safe, escaped regex;
   a leading `*` is rejected per Smart View) and matches the displayed label
   (name or active alias). Any column header sorts (asc/desc) with a visible
   indicator and `aria-sort` on the active header; "model order" (the dimension's
   own element order) is a named option alongside name sort. A live "N of M
   members" count (`role=status`) gives result feedback. No infinite scroll
   (NN/g/LogRocket): member lookup is goal-directed, so filter + a windowed
   scroll region with honest scrollbar and out-of-region action bar is used.
5. **Flat and Hierarchy modes.** Flat (searchable, sortable) is the default; a
   "Flat / Hierarchy" toggle switches to an indented first column with disclosure
   twisties over the flattened, windowed tree (reusing `buildElementTree` and the
   existing expand-all/collapse-all/level logic). No per-row accordions (NN/g).
6. **Set builder keeps the two-pane shell.** `MemberSetPicker` keeps its
   Available -> Included dual list, transfer (Add/Replace), presets, and the
   ordered Included pane; only the Available pane becomes a `MemberTable` in
   select mode (checkbox per row + click, `aria-selected`, controlled selection
   Set). The dynamic-MDX mode (`SubsetEditor`) is unchanged.
7. **Terminology.** The UI says **dimension** (never "global/shared/library
   dimension"; "global" is the internal ADR-0031 term), **member** (code keeps
   `element`), **attribute**, **alias**, **set**, and **total**/rollup. Backend
   and TS type names (`ElementDto`, `SubsetDto`, `SharedDimension*`) stay as-is.

### v1 scope vs deferred

- **v1 (this delivery):** the CSS-grid `MemberTable` with sticky header + pinned
  member column, gated virtualization with APG row-index a11y, attribute column
  show/hide, wildcard + alias-aware search, sortable headers with `aria-sort` and
  model-order, "N of M" count, Flat/Hierarchy toggle, and controlled multi-select
  for the set builder (checkbox + click + Shift-range); wired into the dimension
  editor (browse) and the set builder (select). Read-only attribute cells.
- **Deferred (own follow-ups):** inline cell editing of attribute values (needs
  per-cell define-attribute wiring) and, with it, the interactive `role=grid` +
  APG two-dimensional cell-navigation matrix; relationship/family bulk operators
  (Descendants/Children/Ancestors/Siblings/Level) and Keep/Hide view filters;
  per-column typed filters; a backend paged/filtered member endpoint (v1 windows
  the full member payload client-side, fine for thousands).

## Alternatives considered

- **Add react-window / TanStack Virtual / a grid library.** Rejected: violates
  the dependency-light constraint (ADR-0020); the windowing math is ~40 lines and
  the APG a11y wiring is needed regardless of library.
- **Native `<table>` with `position:sticky`.** Rejected: a pinned first column +
  per-column widths + virtualization are awkward in `<table>`; CSS grid gives
  direct control and matches the existing pivot grid.
- **Keep the tree, just virtualize it.** Rejected: the user asked for a table
  with attribute columns; a tree cannot show columns. Hierarchy is kept as a mode
  of the table (flattened) instead.

## Consequences

- The dimension and set editors handle thousands of members at a constant DOM
  cost, with attributes visible on demand and fast search/sort. One shared
  component keeps the two surfaces consistent and is reusable later for the
  cube-dimension model view.
- Risks (carried from the research): virtualization removes off-screen rows from
  the DOM, so the APG `aria-rowcount`/`aria-rowindex` wiring is mandatory (a
  single-source but load-bearing pattern) or a future WCAG audit fails; an
  in-house virtualizer is fixed-row-height in v1 (variable heights deferred); the
  pinned column + horizontal scroll + windowing need careful z-index/background
  layering; wildcard search must escape regex metacharacters to avoid bad matches
  or ReDoS. The ADR-0031 element-security and attribute-propagation limitations
  still apply (the editor must not widen member exposure beyond what the API
  already returns).
- Validation: web typecheck/lint stay green; the member table is browser-verified
  against a dimension with hundreds of members (search, sort, attribute toggle,
  virtualized scroll, Flat/Hierarchy) with no console errors.
