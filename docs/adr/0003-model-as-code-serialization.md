# ADR-0003: Model-as-code serialization format

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Epiphany maintainers
- **Phase:** 1

## Context
A committed differentiator: every object (dimensions, cubes, rules, flows, views) has a canonical, human-readable, Git-friendly text form that is the source of truth. The requirements are a lossless round-trip, stable and canonical ordering (clean diffs), reviewability in pull requests, and never being required of casual users (the UI edits it for them, per the dead-simple mandate).

## Decision (recommended, to finalize in Phase 1)
A directory of text files per model: structured objects (dimensions, cubes, views, security) in TOML (or JSON) with canonically-sorted keys, and rules and flows as their own source files (a rule DSL, and `.ts` for flows). One object per file where it aids diffing.

## Alternatives considered
- **TOML:** human-friendly, supports comments, and good for config-shaped objects. Arrays of tables can get verbose for large element lists.
- **YAML:** compact and familiar, but whitespace-fragile with several footguns.
- **JSON:** ubiquitous and easy to round-trip, but it has no comments and noisier diffs.
- **A single TypeScript-native definition:** powerful, but it pushes everyone toward code, which conflicts with the dead-simple mandate.

## Consequences
- Canonical ordering is mandatory for determinism: serialization sorts keys and elements deterministically. Validated by a round-trip identity property test (text to model to text) in Phase 1.
- Large element lists may need a compact representation. Revisit if diffs bloat.
