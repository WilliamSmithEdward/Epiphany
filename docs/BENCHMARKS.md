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

The one pass keeps its totals in a row, each at the place its number gives it, where there
are no more than four million places, and looks them up by number only past that. Adding
to a place is quicker than a lookup: the 400 totals of two dimensions go from 4.9 ms to
3.8 ms, the 113 of three from 7.8 ms to 7.0 ms, and the ten largest of a hundred from 3.6
ms to 2.9 ms. Most of what is left is the pass over the 200,000 stored cells themselves,
each kept apart from the next in memory (2026-11-21).

A stored cell's coordinate was a list kept apart from the cell, found by a pointer. For a
cube of up to eight dimensions it is now kept in place beside the value, so a pass over
the cells reads them one after the next. The 400 totals of two dimensions go from 3.8 ms
to 1.3 ms, the same grid of a calculated member from 2.4 ms to 1.5 ms, the 113 totals of
three dimensions from 7.0 ms to 5.9 ms, and the long `NON EMPTY` query from 48 ms to 45
ms. Writing 200,000 cells is unchanged at 198 ms: its time is not in storing them
(2026-11-21).

Reading a cell made a list of where the cell is, and reading the cell of one member made
a tuple to hold that member; both are now done without. The grid of 10,000 leaf cells
goes from 2.4 ms to 2.2 ms, that of a calculated member from 2.7 ms to 2.4 ms, and the
`Filter` over 10,000 tuples from 3.3 ms to 2.6 ms (2026-11-21).

Laying a grid out made three lists for each of its cells, one for the slicer's cell and
one more as each axis's tuple was put in. One is enough: the grid of 10,000 leaf cells
goes from 2.2 ms to 1.5 ms, and of a calculated member from 2.4 ms to 2.0 ms (2026-11-21).

Writing 200,000 cells in one batch took 198 ms, about a microsecond a cell, of which
storing the cell is an eighth. Three things on the way there were trimmed: the locks were
asked for again at every cell and are now asked for once for the batch and each cube; the
writes were named for the journal even where there is none; and names were lowered a
character at a time and looked up with the slow hash. The batch now takes 117 ms. A
server keeps a journal, so it gains from the first and third only: one request writing
50,000 cells, 3.8 MB of JSON, took 75 ms through a server on this machine and takes 66 ms,
its body read, its commit written to the journal and flushed (2026-11-21).

Every name in a batch was still lowered into a new text before it was looked up. The key
is now written over the last one, and the batch of 200,000 takes 89 ms; the request of
50,000 through a server, 56 to 60 ms (2026-11-21).

Cells read by name are found the same way. One request reading 5,000 cells through a
server, as many as the add-in asks for at a time, took 7.3 ms at the start of these
changes and takes 5.5 ms (2026-11-21).

It is not all gain. In the cube with a rule and feeders, the first total read after a
write, which includes working out what is fed, takes 10.7 ms where it took 8.8 ms with
the coordinate kept apart, and 113 such totals read together 11.6 ms where they took 10.0
ms. Where that goes was looked for and not found. In `bench_cells` the pass over the
stored cells for one grand total goes from 6.4 ms to 4.0 ms (2026-11-21).

`NON EMPTY` over a cross join used to write the cross join out and then drop its empty
tuples, so the cross join of three dimensions here, 1,442,897 tuples, was refused for its
length. Its tuples that hold something are now found by looking through the stored cells:
the 200,000 cells give 36,461 rows in 69 ms, and in 57 ms with each tuple known by one
number in place of a list. Of that, finding the rows takes 5 ms, adding up their totals
15 ms, and describing the 109,383 members of the answer 17 ms: a name, a unique name and
a dimension's name written out for each. They are at most 339 different members, three
dimensions of 113, and with each described once and copied where it stands again the
query takes 47 ms (2026-11-21). Looking through the cells costs a pass
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
