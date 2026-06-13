# ADR-0003: Model-as-code serialization format

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Epiphany maintainers
- **Phase:** 1

## Context
A committed differentiator: every object (dimensions, cubes, rules, flows, views)
has a canonical, human-readable, **Git-friendly** text form that is the source of
truth. Requirements: lossless round-trip, stable/canonical ordering (clean
diffs), reviewable in PRs, and never *required* of casual users (the UI edits it
for them — dead-simple mandate).

## Decision (recommended, to finalize in Phase 1)
A **directory of text files per model**: structured objects (dimensions, cubes,
views, security) in **TOML** (or JSON) with canonically-sorted keys; rules and
flows as their own source files (rule DSL; `.ts` for flows). One object per file
where it aids diffing.

## Alternatives considered
- **TOML** — human-friendly, comments, great for config-shaped objects; arrays of
  tables can get verbose for large element lists.
- **YAML** — compact and familiar; whitespace fragility and footguns.
- **JSON** — ubiquitous, easy round-trip; no comments, noisier diffs.
- **A single TS-native definition** — powerful, but pushes everyone toward code
  (conflicts with the dead-simple mandate).

## Consequences
- Canonical ordering is mandatory (determinism) — serialization sorts keys and
  elements deterministically. Validated by a **round-trip identity** property
  test (text → model → text) in Phase 1.
- Large element lists may need a compact representation; revisit if diffs bloat.
