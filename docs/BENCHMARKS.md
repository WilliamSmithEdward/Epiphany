# Benchmarks

Numbers are from one development machine and are only comparable with each other. Rerun at
every milestone and add a row group; keep an optimisation only if it shows here.

## Cells

```
cargo run --release --example bench_cells
```

A random cube of 4 dimensions, each with 100 leaves, 12 random consolidations and one
consolidation over every leaf; 200,000 leaf cells written at random coordinates.

| Operation | What it measures | Milestone 2 (2026-11-18) |
| --- | --- | --- |
| Write leaf | One leaf write into a growing store | 108 ns |
| Read leaf | One leaf read from the full store | 58 ns |
| Read consolidation by combinations | A cell whose leaf combinations (at most 10,000) are fewer than the stored cells, so each is looked up | 79 µs |
| Read consolidation by stored cells | The grand total (100,000,000 leaf combinations), so each of the 200,000 stored cells is weighed | 6.4 ms |

Notes:

- Leaf reads were 191 ns before they stopped building per-read lists; that change is kept.
- Consolidated reads are uncached. The grand total costs about 32 ns per stored cell.
