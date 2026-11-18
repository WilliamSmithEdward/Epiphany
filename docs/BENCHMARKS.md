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

## MDX

```
cargo run --release --example bench_mdx
```

The same cube and data, queried through the database.

| Query | What it measures | Milestone 6 (2026-11-18) |
| --- | --- | --- |
| Write 200,000 cells in one batch | Name lookup, checks and storing, with no journal | 202 ms |
| 10,000 leaf cells | 100 by 100 leaves at one leaf of the other two dimensions | 2.1 ms (0.2 µs a cell) |
| 400 cells, each totalling two dimensions | 20 by 20 leaves at the grand total of the other two | 84 ms (210 µs a cell) |
| 113 cells, each totalling three dimensions | Every element of one dimension at the grand total of the rest | 102 ms (0.9 ms a cell) |
| The 10 largest of 100 totals | `TopCount` over totals, then the cells | 72 ms |

Parsing and set handling are negligible; the time is the consolidated reads, which are still
uncached. A grid of a few thousand consolidated cells would take seconds, so a consolidation
cache is the first optimisation to make when that matters.

## Rules and feeders

```
cargo run --release --example bench_rules
```

A random cube of 3 dimensions of 100 leaves each. In the last dimension one element is
calculated by a leaf rule as twice another, and 50,000 cells of that other element are stored.

| Read | Without feeders | With feeders |
| --- | --- | --- |
| Total of the calculated element (10,000 leaves beneath it) | 4.3 ms | 8.0 ms, including working out what is fed |
| Grand total (1,000,000 leaves beneath it) | 401 ms | 5.0 ms |

Both give the same values. Without feeders a consolidation that a leaf rule can reach visits
every leaf beneath it; with them it visits only the stored and fed cells. What is fed is
worked out once after each change to the data, which is the extra time on the first read.

## Notes

- Leaf reads were 191 ns before they stopped building per-read lists; that change is kept.
- Consolidated reads are uncached. The grand total costs about 32 ns per stored cell.
