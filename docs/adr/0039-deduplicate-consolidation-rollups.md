# ADR-0039: Deduplicate consolidation rollups

Status: Accepted
Date: 2026-06-20

## Context

`Dimension::leaf_weights(element)` (`crates/epiphany-core/src/dimension.rs`) expands
a query element into the leaf descendants that contribute to its rollup, each with a
net weight. It is the single seam through which EVERY consolidated read flows: the
cell read (`Cube::get`), the rule-aware reads (`Cube::consolidate_with` /
`consolidate_fed`), data spreading (`crates/epiphany-core/src/spread.rs`),
element-security masking (`crates/epiphany-core/src/element_mask.rs`), feeder potency
(`crates/epiphany-calc/src/feeders.rs`), and provenance
(`crates/epiphany-calc/src/provenance.rs`).

It previously walked every consolidation edge and SUMMED a leaf's weight over every
path that reached it (`*acc.entry(leaf).or_insert(0) += weight`). When a descendant
leaf is reachable from the query element by more than one path -- a multi-parent
"diamond" -- that leaf was counted once PER PATH. For example, with edges
`Total -> East`, `Total -> Coastal`, and `Coastal -> East`, East reaches Total by two
paths and accumulated weight 2, so Total double-counted East's value.

A standard per-path weighted sum is the textbook rollup, but multi-parent diamonds
are now easy to create accidentally: Copy-to references, additive multi-parent
add-child (ADR-0036), and top-level pinning (ADR-0038) all let a member sit under
several consolidations that themselves roll up together. In those cases the per-path
sum is almost always a silent error -- a grand total that quietly counts a region
twice -- rather than an intended multiplier.

## Decision

**A descendant leaf is counted ONCE per consolidation.** `leaf_weights` now records
each distinct contributing leaf exactly once, regardless of how many paths reach it.

**The weight is the most-direct (fewest-hops) path's net weight.** When a leaf is
reachable by several paths, the path that reaches it in the fewest edges wins. Ties
(the same depth via different edges) are broken deterministically by
edge-declaration order. For plain weight-1 edges this always yields weight 1.

**Implemented as a breadth-first traversal.** A queue of `(node, path_weight)` starts
at `(element, 1)`; a `visited` set guards each node. Dequeuing a node that is already
visited is skipped, so the FIRST time a node is dequeued -- which BFS guarantees is
along a shortest path, edge order breaking ties -- is the one that counts. A leaf
records `(node, weight)` once; a string leaf contributes nothing (text, not a number,
unchanged); a consolidation enqueues each child edge in edge order with
`weight * edge.weight` (saturating). The post-processing is unchanged: sort by leaf
index (deterministic, ADR-0009) and EXCLUDE leaves whose recorded weight is zero
(the zero-weight-excluding contract is preserved). The return type
(`Result<Vec<(u32, i64)>, ModelError>`) and the out-of-range-element error are
unchanged.

The visited set also makes the traversal cycle-safe by construction, regardless of
edge structure. The backend rejects cycles when an edge is added, so this is a
belt-and-braces property rather than a relied-upon one; termination no longer depends
on the acyclicity invariant.

**This replaces the prior standard per-path weighted sum.** A weighted single edge
with no diamond is unchanged (East under Total with weight 3 still yields weight 3);
only a leaf reached by multiple paths changes -- from the sum of its path weights to
its most-direct path weight, counted once.

## Consequences

- Every consolidated read deduplicates, because they all flow through
  `leaf_weights`. The headline effect is the cell read in
  `crates/epiphany-core/src/cube.rs`: the per-dimension weight maps that drive the
  rollup no longer contain a per-path-inflated weight, so a diamond's shared leaf is
  added once.
- Spreading (`spread.rs`) already distributes to each DISTINCT leaf once (it collects
  the leaf set and requires weight 1); the set of leaves is unchanged, so spreading
  is unaffected except that it can no longer observe a spurious non-unit weight from a
  weight-1 diamond.
- Element-security masking (`element_mask.rs`), feeder potency (`feeders.rs`), and
  provenance (`provenance.rs`) all consume only the SET of contributing leaves
  (discarding the weight). The set of distinct leaves is identical before and after,
  so which leaves are checked / fed / traced is unchanged; multiplicity never affected
  them.
- A model that deliberately relied on a multi-path sum as a multiplier would change.
  We judge this not to exist in practice and to be indistinguishable from the
  accidental double-count this fixes; a genuine multiplier is expressed with an edge
  weight, not by routing a leaf through two consolidations.
- This is independent of the ADR-0038 display-root double-count: that is a reader
  summing the display roots (a member appearing both as a pinned root and inside its
  consolidation), which does not go through a single `leaf_weights` call and is a
  documented, accepted display affordance. ADR-0039 is purely about a single
  consolidation's own rollup.

## Alternatives considered

- **Keep the per-path weighted sum (status quo).** Rejected: with Copy-to,
  multi-parent add-child, and pinning all making diamonds easy, the sum is almost
  always an unintended double-count, and there is no UI affordance that communicates
  "this leaf is multiplied because it has two parents".
- **Count once with the LAST (deepest) or the MAX-weight path.** Rejected as less
  predictable; the most-direct path is the one a modeler reasons about ("East is
  directly under Total") and BFS makes it cheap and deterministic.
- **Forbid diamonds at edge-add time.** Rejected: alternate rollups are a wanted
  feature (a member legitimately rolls up under more than one consolidation, e.g.
  East under both Total and Coastal); the fix is to count it once, not to ban the
  structure.
