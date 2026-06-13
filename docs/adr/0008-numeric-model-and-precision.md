# ADR-0008: Numeric model and precision

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Epiphany maintainers
- **Phase:** 1

## Context
Cell values must be deterministic (the same inputs produce identical results) and correct for finance (no "0.1 + 0.2" surprises), while respecting the memory budget (roughly 24 bytes per cell or less). Floating-point summation is also order-sensitive, which interacts with the determinism mandate.

## Decision (recommended, to finalize in Phase 1)
Use scaled-integer (fixed-point) values for stored and monetary cells: an `i64` (8 bytes, the same size as `f64`) with a documented scale (for example, four decimal places). This is exact and deterministic. Use floating point only for derived ratios and analytics where a documented tolerance is acceptable. Regardless of type, aggregation uses a deterministic order so that sums are reproducible.

## Alternatives considered
- **`f64` everywhere:** fast, 8 bytes, and the ecosystem default, but inexact for money and order-sensitive, so totals can be nondeterministic. This is what legacy engines used.
- **Decimal128:** exact decimal, but 16 bytes (which fights the memory budget) and slower.
- **Scaled `i64`:** exact, 8 bytes, fast, and deterministic. The fixed scale must be chosen well and overflow handled.

## Consequences
- Choosing exactness over the legacy `f64` norm is a deliberate improvement.
- We need an overflow and rounding policy and a clear scale. Ratios that need fractions use the tolerance-bound float path. Validated by golden and property tests.
