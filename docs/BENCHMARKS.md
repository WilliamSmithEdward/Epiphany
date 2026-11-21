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

| Query | What it measures | Milestone 6 (2026-11-18) | Totals together and kept (2026-11-19) | Asked again |
| --- | --- | --- | --- | --- |
| Write 200,000 cells in one batch | Name lookup, checks and storing, with no journal | 202 ms | 206 ms | |
| 10,000 leaf cells | 100 by 100 leaves at one leaf of the other two dimensions | 2.1 ms (0.2 µs a cell) | 3.0 ms | 1.7 ms |
| 400 cells, each totalling two dimensions | 20 by 20 leaves at the grand total of the other two | 84 ms (210 µs a cell) | 5.1 ms (13 µs a cell) | 0.1 ms |
| 113 cells, each totalling three dimensions | Every element of one dimension at the grand total of the rest | 102 ms (0.9 ms a cell) | 8.8 ms (78 µs a cell) | 0.04 ms |
| The 10 largest of 100 totals | `TopCount` over totals, then the cells | 72 ms | 4.3 ms | 0.2 ms |

At milestone 6 every consolidated cell passed over the stored cells by itself. Now the cells
of a query's axes, and those a set function such as `TopCount` reads to choose its tuples,
are added up in one pass, and a consolidated cell once worked out is kept until the next
commit, so the same query asked again reads what was kept.

Keeping totals costs the leaf cells a check of whether each is one. That took the grid of
10,000 leaf cells from 2.1 ms to 3.0 ms at first; with the check made in one pass over the
cell's elements, and leaves no longer announced before they are read, it is 2.5 ms.

A grid of 10,000 cells of a calculated member, a share worked out from two leaf cells with
an `IIF`, took 6.7 ms (0.7 µs a cell) while the names in the member's expression were
looked up at every cell. With the names settled once for the query it takes 2.4 ms (0.24 µs
a cell), about the cost of the three cells each one reads (2026-11-21).

The same holds for the expressions of the axes, the slicer and the named sets: a `Filter`
over 10,000 tuples that compares two cells of each took 10.0 ms and takes 3.3 ms
(2026-11-21).

A calculated member whose cells are totals had each of them added up by itself, as every
total was at milestone 6: 400 cells of a share of the grand total took 75 ms. The totals a
member's expression names are now announced with the rest of the grid and added up in the
same pass, and the 400 cells take 3 ms. Finding what to announce costs the grid of 10,000
leaf cells of a calculated member a little: 2.7 ms where it was 2.4 ms (2026-11-21).

`NON EMPTY` over a cross join used to write the cross join out and then drop its empty
tuples, so the cross join of three dimensions here, 1,442,897 tuples, was refused for its
length. Its tuples that hold something are now found by looking through the stored cells:
the 200,000 cells give some 36,000 rows in 69 ms. Looking through the cells costs a pass
over all of them, so it is done only where the cross join is long beside them: the cross
join of two dimensions, 12,769 tuples, is still written out, in 6 ms (2026-11-21).

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
| 113 totals of the calculated element, one at a time (2026-11-19) | 14 ms | 36 ms |
| The same 113 totals read together (2026-11-19) | 6 ms | 9 ms |

Both give the same values. Without feeders a consolidation that a leaf rule can reach visits
every leaf beneath it; with them it visits only the stored and fed cells. What is fed is
worked out once after each change to the data, which is the extra time on the first read.

Read one at a time, each total of a fed cube passes over every stored and fed cell. Read
together, as the cells of a query or of one `/cells/read` are, each of those cells that counts
towards one of the totals is worked out once and shared out among them; a rule is not asked
about a cell that counts towards none. In this benchmark every cell counts, so it shows no
gain from that.

## Notes

- Leaf reads were 191 ns before they stopped building per-read lists; that change is kept.
- A consolidated read by itself costs about 32 ns per stored cell for the grand total. Reading
  one total at a time was the cost of every grid until totals were added up together and kept.
