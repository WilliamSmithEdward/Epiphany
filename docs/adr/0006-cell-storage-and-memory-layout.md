# ADR-0006: Cell storage and memory layout

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Epiphany maintainers
- **Phase:** 1

## Context
The cell store is the dominant memory cost. A coordinate is one element ordinal per dimension. A naive key (a boxed slice per cell) means a heap allocation and a pointer per cell, which is too heavy at the target scale (budgets in ROADMAP section 8). The default hasher (SipHash) is also slow on the integer keys that dominate the hot path.

## Decision (accepted)
Pack a coordinate into a single `u128` key when the cube's coordinate space fits in 128 bits. Each dimension gets a bit field sized to its element count (ceil log2), laid out at a fixed offset. Cubes whose total width exceeds 128 bits fall back to a boxed-slice key. Hash cell keys with a small, dependency-free FxHash (fixed seed, deterministic). Cell values are exact scaled integers (8 bytes, ADR-0008).

This keeps a typical cell near the section 8 budget: a `u128` key (16 bytes) plus an 8-byte value, before hash-map overhead, with no per-cell heap allocation.

## Alternatives considered
- **A boxed slice per cell:** simple and unbounded in width, but a heap allocation and pointer per cell, with poor cache behavior.
- **A columnar layout (parallel arrays):** excellent for scans, but worse for the point writes that planning workloads generate. Revisit for bulk scans later.
- **The default SipHash:** strong against collision attacks we do not face on internal integer keys, and slower than FxHash on the hot path.

## Consequences
- Common cubes pay no per-cell heap allocation and use a compact key.
- A documented 128-bit ceiling on the packed path; wider cubes use the slower boxed-slice fallback (a rare case).
- The fast hasher is deterministic but not collision-resistant, which is acceptable because keys are internal coordinates, not untrusted input.
- Bytes-per-cell is tracked by benchmarks against the section 8 budget.
