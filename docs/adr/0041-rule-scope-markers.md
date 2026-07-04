# ADR-0041: Rule scope markers (N: / C:)

- **Status:** Accepted
- **Date:** 2026-07-02
- **Deciders:** Maintainer
- **Phase:** Post-roadmap (calc fidelity, continues ADR-0007)

## Context

A rule area (ADR-0007) selects the cells a formula targets, one selector per
constrained dimension; a dimension the author omits is *unconstrained*. Today an
unconstrained dimension matches **leaves only** (`compiled.rs`,
`DimPredicate::Any` tests `element.kind.is_leaf()`), so a rule that leaves a
dimension free computes leaf values and lets the ordinary consolidation roll
those leaves up. That is exactly right for an **additive** measure: the leaf
values are computed by the rule, and the total is their weighted sum.

It is **wrong** for a **non-additive** measure. Consider a ratio

```
['Measure':'Ratio'] = value['Measure':'Sales'] / value['Measure':'Cost'];
```

with `Region` unconstrained. At each leaf region the rule fires and computes
`Sales/Cost`. But at `[Region:Total, Ratio]` the rule does **not** fire (Total is
not a leaf, so `Any` does not match it), so the engine falls through to
consolidation and **sums the per-region ratios** — `0.5 + 0.4 = 0.9` where the
correct ratio of totals is `(Sales_N+Sales_S)/(Cost_N+Cost_S)`. Summing a ratio
is meaningless; the displayed total is simply wrong. The same defect hits every
non-additive calculation (margins, rates, prices, weighted averages) at every
consolidated cell. This is a real OLAP fidelity gap, not a cosmetic one.

Incumbent in-memory OLAP engines solve this with **scope markers** on the rule:
`N:` (numeric — the leaf/base cells) and `C:` (consolidated — the rolled-up
cells). A calculation is written to apply to the numeric cells, the consolidated
cells, or (as two statements) both, and a `C:` calculation *recomputes* at the
consolidated coordinate instead of aggregating the components.

Forces:

- **Backward compatibility is non-negotiable.** Every existing rule has no marker
  and must keep its exact present meaning (leaf-only). A model full of additive
  rules must behave identically after this change.
- **Non-additive totals must be expressible.** An author must be able to say
  "recompute this formula at the total," which is impossible today short of
  enumerating every consolidated member by name in the area (unmaintainable, and
  it does not generalize to members added later).
- **Precedence must not change.** Rule precedence is first-matching-area in
  source order (ADR-0007). The marker changes *which coordinates* an area
  matches, never *how ties are broken*.
- **The pipeline is hand-written (no parser library).** Lexer, recursive-descent
  parser, AST, compile, eval, and a canonical `Display` that round-trips. A new
  syntax must thread through all of them and survive `Display -> parse`.
- **Determinism and exact numerics** (ADR-0008, ADR-0009) are unchanged: the
  marker only redirects *where* a formula fires; the arithmetic and the
  consolidation algebra are untouched.

## Decision

Add an **optional, backward-compatible scope marker** as a prefix on a rule's
area:

```
C:['Measure':'Ratio'] = value['Measure':'Sales'] / value['Measure':'Cost'];   # consolidated cells
N:['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];  # leaf/base cells (explicit)
 ['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];   # no marker = leaf-only (today's default)
```

Concretely:

1. **Syntax.** An area may be prefixed by `N:` or `C:` (a bare letter `N`/`C`
   followed by `:`, then the `[` of the area). The marker is case-insensitive
   like every other keyword. No prefix is equivalent to `N:` in meaning but is
   preserved as "unmarked" so existing sources round-trip byte-for-byte through
   the canonical printer (the printer emits `N:` **only** when the author wrote
   it; an unmarked rule prints unmarked).

2. **Meaning — the marker redefines what an *unconstrained* dimension matches.**
   For a rule whose scope is:
   - **Leaf** (no marker, or `N:`): an unconstrained dimension matches **leaf**
     members only. This is today's `DimPredicate::Any` behavior, unchanged.
   - **Consolidated** (`C:`): an unconstrained dimension matches **consolidated**
     members only. A `C:` rule therefore fires *at* consolidated coordinates and
     its formula is evaluated there, so a ratio is recomputed at the total
     instead of the components being summed.

   An **explicitly named** selector (an element name, `{leaves}`, `{consolidated}`,
   `{children of ...}`, an attribute predicate, etc.) is unaffected by the marker:
   it already resolves to a fixed member set and matches those members whether
   they are leaves or consolidations (this is how an explicit consolidation
   override works today). The marker governs only the *implicit* (`Any`)
   dimensions.

3. **A non-additive measure that needs both leaves and totals is written twice** —
   one `N:` statement and one `C:` statement — exactly as incumbent engines do.
   This keeps each statement's scope unambiguous and precedence obvious, and it
   composes with the existing first-match rule ordering with no special cases.

4. **Precedence is unchanged.** `matching_rule` still returns the first area (in
   source order) that matches the queried coordinate; the marker only affects
   whether a given area matches a given coordinate. A `C:` and an `N:` rule for
   the same measure never both match the same coordinate (a member is either a
   leaf or a consolidation), so their relative order is irrelevant; where a `C:`
   rule and an explicit-member rule could both match a consolidated cell,
   source order breaks the tie as always.

5. **Evaluation.** When a `C:`-scoped rule matches a consolidated coordinate, the
   evaluator runs its formula at that coordinate (the cell refs read consolidated
   values through the same resolver), *instead of* calling `consolidate_with`. No
   change to the leaf path, to feeders (ADR-0005), or to the consolidation
   algebra. The compiled area carries a `scope` field; `CompiledArea::matches`
   consults it for `Any` dimensions.

## Alternatives considered

- **A per-dimension `{consolidated}` family selector instead of a rule-level
  marker.** The language already has `{consolidated}`, so one could write
  `['Region':{consolidated}, 'Measure':'Ratio']`. Rejected as the *primary*
  mechanism: it forces the author to name every otherwise-free dimension with an
  explicit `{consolidated}` (a cross-join of scopes across N free dimensions is
  `2^N` statements to cover), and it says nothing about the common "just the
  total, all free dims consolidated" intent. The rule-level marker expresses that
  intent in one token and matches the mental model (and syntax) users bring from
  incumbent tools. `{consolidated}` remains available for the mixed cases.
- **Infer non-additivity and auto-recompute at totals.** Detect that a formula is
  a ratio/product and silently recompute at consolidations. Rejected: it is
  guesswork (is `a - b` additive? usually, but not for a subtraction of two
  independently-consolidated ratios), it would silently change the meaning of
  existing rules, and "silently reinterpret the author's formula" is precisely
  the kind of magic this codebase avoids. The author states scope explicitly.
- **A new keyword block (`CONSOLIDATED { ... }`) wrapping several rules.**
  Heavier syntax, a new nesting construct in the grammar, and it separates the
  scope from the area it governs. The one-token prefix keeps scope adjacent to
  its area and needs no block structure.
- **Change the default so `Any` matches every element (leaf and consolidated).**
  Rejected outright: it breaks every existing additive rule (a total would be
  computed by the rule *and* aggregated, double-counting or overriding), i.e. it
  is not backward compatible. The leaf-only default is load-bearing and stays.

## Consequences

- Non-additive measures can finally be correct at consolidated cells: a `C:` ratio
  recomputes at the total rather than summing the component ratios. This closes a
  genuine correctness gap for margins, rates, and weighted averages.
- **Every existing rule is unchanged.** No marker still means leaf-only, and the
  canonical printer emits no marker for an unmarked rule, so existing model files
  round-trip byte-for-byte and existing behavior is bit-identical.
- New surface, all additive: a `Scope` enum on the AST `Area` and the compiled
  `CompiledArea`, two new tokens' worth of lexer/parser handling (a leading
  `N:`/`C:`), and `Display` support. No new dependency, no change to the numeric
  or determinism contracts, no change to precedence.
- Precedence and evaluation stay simple: the marker is read entirely as "what
  does an unconstrained dimension match," so it lives in `CompiledArea::matches`
  and touches neither the memo, the cycle guard, nor the consolidation math.
- Validated by tests in `epiphany-calc`: the canonical `Display` round-trips a
  marked rule (`Display -> parse -> Display` is stable, and an unmarked rule stays
  unmarked); a `C:` ratio rule computes `Sales_total/Cost_total` at the Total
  rather than summing the per-leaf ratios; an unmarked (and an `N:`) rule remains
  leaf-only and its total still aggregates; and precedence with mixed markers is
  first-match in source order.
- Revisitable: an all-scope marker (apply to leaves *and* consolidations from one
  statement) or a per-area-dimension scope override could be added later if the
  two-statement idiom proves too verbose; neither is needed for the correctness
  fix and both would build on this `Scope` field.
