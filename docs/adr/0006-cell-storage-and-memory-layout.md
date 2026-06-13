# ADR-0006: Cell storage and memory layout

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Epiphany maintainers
- **Phase:** 1

## Context
The cell store is the dominant memory cost. A coordinate is one element ordinal per dimension. A naive key (a boxed slice per cell) means a heap allocation and a pointer per cell, which is too heavy at the target scale (budgets in ROADMAP section 8). The default hasher (SipHash) is also slow on the integer keys that dominate the hot path.

## Decision (accepted)
Pack a coordinate into a single `u64` key when the cube's coordinate space fits in 64 bits. Each dimension gets a bit field sized to its element count (ceil log2), laid out at a fixed offset. Cubes whose total width exceeds 64 bits fall back to a boxed-slice key. The choice is made per cube at the *store* level (an `enum { Narrow(map of u64), Wide(map of boxed slice) }`), not per key, so a narrow entry carries no discriminant and no alignment padding. Hash cell keys with a small, dependency-free FxHash (fixed seed, deterministic). Cell values are exact scaled integers (8 bytes, ADR-0008).

This keeps a narrow cell at a `u64` key (8 bytes) plus an 8-byte value, with no per-cell heap allocation. Measured at 20.3 bytes per cell including hash-table overhead (a 110k-cell store; see the `tests/memory.rs` allocator probe), within the section 8 budget of about 24 bytes.

The 64-bit ceiling is wide in practice: eight dimensions of 256 elements, or four of 65k, fit. (An earlier draft packed into `u128`, but a `u128` field forces 16-byte alignment, padding each entry past budget; the `u64` key plus the store-level split is what meets it.)

## Alternatives considered
- **A boxed slice per cell:** simple and unbounded in width, but a heap allocation and pointer per cell, with poor cache behavior.
- **A columnar layout (parallel arrays):** excellent for scans, but worse for the point writes that planning workloads generate. Revisit for bulk scans later.
- **The default SipHash:** strong against collision attacks we do not face on internal integer keys, and slower than FxHash on the hot path.

## Consequences
- Common cubes pay no per-cell heap allocation and use a compact key; measured at 20.3 bytes/cell, within budget.
- A documented 64-bit ceiling on the packed path; wider cubes use the slower boxed-slice fallback (a rare case). A `u128` narrow tier could be added between the two if a real model needs 65 to 128 bits; benchmark-gated.
- The fast hasher is deterministic but not collision-resistant, which is acceptable because keys are internal coordinates, not untrusted input.
- Bytes-per-cell is gated by a deterministic allocator probe (`tests/memory.rs`); throughput and latency are tracked by `benches/cube_ops.rs` (bulk-load measured at about 13M cells/sec/core against the ~1M budget).
