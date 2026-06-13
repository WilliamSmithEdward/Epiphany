# ADR-0008: Numeric model & precision

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Epiphany maintainers
- **Phase:** 1

## Context
Cell values must be **deterministic** (same inputs → identical results) and
**correct for finance** (no `0.1 + 0.2` surprises), while respecting the
**memory budget** (≤ ~24 bytes/cell). Floating-point summation is also
*order-sensitive*, which interacts with the determinism mandate.

## Decision (recommended, to finalize in Phase 1)
**Scaled-integer (fixed-point) for stored and monetary values** — an `i64`
(8 bytes, same as `f64`) with a documented scale (e.g. 1e-4). Exact and
deterministic. **Floating point only** for derived ratios/analytics where a
**documented tolerance** is acceptable. Regardless of type, **aggregation uses a
deterministic order** so sums are reproducible.

## Alternatives considered
- **`f64` everywhere** — fast, 8 bytes, ecosystem-default; but inexact for money
  and order-sensitive (nondeterministic totals). Used by legacy engines.
- **Decimal128** — exact decimal; 16 bytes (fights the memory budget) and slower.
- **Scaled `i64`** — exact, 8 bytes, fast, deterministic; fixed scale must be
  chosen well and overflow handled.

## Consequences
- Choosing exactness over the legacy `f64` norm is a deliberate improvement.
- Need overflow/round policy and a clear scale; ratios that need fractions use
  the tolerance-bound float path. Validated by golden + property tests.
