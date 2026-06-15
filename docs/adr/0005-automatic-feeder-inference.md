# ADR-0005: Automatic feeder inference, validation, and under/over-feed detection

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 4 (rules, feeds, and calculation engine)

## Context

Rule-derived leaf cells are sparse: a rule like `Margin = Sales - Cost`
produces a value only where its inputs are populated. A consolidation over a
rule-derived leaf measure must include those rule cells, but enumerating the
full dense leaf space to find them does not scale on large, sparse models. The
incumbent answer is the "feeder": a hand-written marker that says "this
rule-derived leaf may be populated, include it in the sparse scan." Feeders are
the single most bug-prone chore in incumbent products: forget one and a rollup
silently reads a wrong (too-low) total (under-feed); write too many and you
waste scan time and RAM (over-feed). Both failures are quiet.

Phase 4 commits to *automatic* feeder inference and validation as a
differentiator. Three things needed pinning: what is statically derivable, how
the result is validated, and how honest the scope is about what it cannot
analyze.

## Decision

**1. Two separate consolidation paths, with the dense path as the source of
truth.** `epiphany_core::Cube::consolidate_with` always enumerates the correct
contributing leaves and is what the evaluator uses for reads, so correctness
never depends on feeders. `Cube::consolidate_fed` is the sparse optimization: a
union scan over a supplied fed-coordinate set. Feeders exist only to make the
sparse path fast; they can never make a read *wrong*, because the read does not
use them. This separation is what makes "validate feeders" meaningful: we
compare the sparse fed set against the dense truth.

**2. Inference is a fixpoint over "potentially non-zero" leaves (amended m8.4).**
`infer_feeders` analyzes each compiled rule whose area selects at least one leaf
coordinate (a rule whose area selects no leaves is a pure consolidation override
and needs no feeder, so it is skipped; see decision 4). For each such rule it
walks the rule's *target* leaves and feeds a target whenever one of the rule's
same-cube inputs is *potentially non-zero* there: a stored leaf, a leaf already
fed by another rule, or a consolidated input that rolls up any such leaf (the
consolidated input is expanded to its contributing leaves via
`Dimension::leaf_weights`). This is computed to a **fixpoint** seeded by the
stored leaves: a newly fed target is itself potentially non-zero, so a later
round feeds a rule that reads it. This makes inference complete for the patterns
the original cut missed: chained rules (a rule reading another rule's derived
output), consolidated inputs, and multi-element target areas with a pinned input
(each target leaf is considered independently, so a pinned input no longer forces
a single-member target). A rule with a **base contribution** (a term that can be
non-zero when every cell it reads is zero: a non-zero constant or attribute,
including such a term inside a conditional branch, decided by a sound static
analysis of the formula) feeds its *whole* target area, since it is non-zero
there regardless of which inputs are populated; this closes the
constant/conditional under-feed. Cross-cube inputs remain non-localizable and a
cross-cube-only rule is still reported opaque (decision 3), unchanged. It remains a sound over-approximation: it never under-feeds an
analyzable rule, and at worst over-feeds a target whose inputs turn out to be
zero (a warning). One consolidated input's leaf expansion is bounded by a cap,
beyond which it is conservatively assumed potent, so inference cost stays bounded
on a pathological hierarchy. The fed set is a sorted `BTreeSet`, the target and
input expansions are walked in index order, and the fixpoint only ever adds
feeders over a finite leaf space, so the result is deterministic and terminates.

**3. Honest scope: un-analyzable rules are reported, never guessed.** A rule
with no same-cube input to localize the feed set (for example a pure constant,
or a rule driven only by a cross-cube scalar) is reported as an `OpaqueRule`
with a reason, not silently treated as fully fed or fully unfed. Cross-cube and
consolidated inputs do not localize the feed set and are ignored for
localization. This is the "derive for the analyzable majority, diagnose the
rest" scope the roadmap commits to.

**4. Validation is dense-truth comparison, on demand only.**
`validate_feeders` evaluates each rule-target leaf with the (always-correct)
dense evaluator and compares against the supplied fed set:
under-fed = a leaf with a non-zero rule value that is not fed (the hard error: a
silent wrong-zero in rollups); over-fed = a fed leaf whose rule value is zero
(a warning: wasted scan and RAM, with an estimated byte cost). The set of
candidate target leaves is exactly a rule's *leaf* coordinates, so a rule whose
area mixes leaf and consolidated members (for example `{descendants of Total}`)
still has its leaf targets validated; only a rule with no leaf targets at all is
treated as a pure override (amended m8.4: this replaces an earlier check that
skipped any rule naming a consolidated member, which could hide an under-feed in
such a rule's leaf targets). Candidate leaves and fed coordinates are walked in
sorted order, so the diagnostic lists are deterministic. Validation is an
explicit operation (the REST `/feeders/diagnostics` endpoint and the model test
path), never on the read hot path.

## Alternatives considered

- **Feeders required and hand-written (incumbent model):** rejected as the
  default. It reintroduces exactly the bug class the differentiator removes.
  Manual feeds remain expressible and are validated by the same dense-truth
  comparison, so the escape hatch exists without being mandatory.
- **A single sparse path used for both reads and rollups (no dense truth):**
  rejected. Correctness would then depend on feeders being complete, so a
  missing feeder would be a silent wrong answer rather than a diagnosable
  warning, and there would be nothing to validate against.
- **Best-effort inference for cross-cube/consolidated-driven rules:** rejected
  for M4. Guessing a feed set for a rule we cannot localize would risk silent
  under-feed. Reporting it as opaque is the honest behavior; manual feeders plus
  validation cover those rules.

## Consequences

- Reads are always correct regardless of feeders, because the evaluator uses the
  dense path; feeders are purely a performance and memory concern, and their
  correctness is checkable on demand against the dense truth.
- The model testing framework and the REST surface can assert "no under-feed and
  no over-feed" as a real property; the M4 acceptance suite does so on an
  analyzable-by-construction model, and the property holds across a restart.
- Realized in M4 as `epiphany-calc::feeders` (`FeederIndex`, `infer_feeders`,
  `validate_feeders`, `FeederDiagnostics`, `OpaqueRule`) plus
  `epiphany_core::Cube::{consolidate_with, consolidate_fed}`, surfaced at
  `GET /api/v1/cubes/{cube}/feeders/diagnostics` and in the web rules
  workspace. The sparse-vs-dense equality and the under/over-feed diagnostics
  are covered by unit tests in `feeders.rs` and gated end to end by
  `epiphany-api/tests/m4_acceptance.rs`.
