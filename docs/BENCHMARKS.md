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

Where a cube's element ids fit 64 bits together, a stored cell's coordinate is now packed
into one number, each dimension given the bits its largest stored element needs: a number
and its place take 16 bytes rather than 48. Looking a cell up is quicker, there being less
to hash and compare; a pass over the cells unpacks each, which costs it a little. Compared
on the same machine, one after the other (2026-11-22):

| | Coordinates kept whole | Packed |
| --- | --- | --- |
| Write leaf | 115 ns | 51 ns |
| Read leaf | 60 ns | 32 ns |
| Read consolidation by combinations | 80 µs | 38 µs |
| Read consolidation by stored cells | 4.0 ms | 4.3 ms |
| 400 cells, each totalling two dimensions | 1.9 ms | 2.2 ms |
| 113 cells, each totalling three dimensions | 5.2 ms | 6.5 ms |
| The 10 largest of 100 totals | 2.9 ms | 4.0 ms |
| Non-empty rows of 1,442,897, found from the cells | 42.8 ms | 43.7 ms |
| 113 totals over feeders, one at a time | 27.5 ms | 33.3 ms |
| 113 totals over feeders, read together | 8.7 ms | 8.8 ms |

Handing out each cell's place as one of two kinds of coordinate had made a pass half as
long again; it is now one plain value, and a pass over the cells is taken as a pass of its
own rather than one cell asked for after another. Ten million cells take 282 to 375 MB
rather than 882 MB, and 1.2 GB at the most while they are written rather than 1.5 GB;
started again, the server holds 279 MB rather than 792 MB, with 464 MB at the most on the
way, and starts in the same 5.5 s.

Reading a cell made a list of where the cell is, and reading the cell of one member made
a tuple to hold that member; both are now done without. The grid of 10,000 leaf cells
goes from 2.4 ms to 2.2 ms, that of a calculated member from 2.7 ms to 2.4 ms, and the
`Filter` over 10,000 tuples from 3.3 ms to 2.6 ms (2026-11-21).

Laying a grid out made three lists for each of its cells, one for the slicer's cell and
one more as each axis's tuple was put in. One is enough: the grid of 10,000 leaf cells
goes from 2.2 ms to 1.5 ms, and of a calculated member from 2.4 ms to 2.0 ms (2026-11-21).

`Filter`, the aggregates and the announcing of cells went through a set copying the cell
for every tuple. They now move one copy along the set, putting each tuple in the place of
the one before: the `Filter` over 10,000 tuples goes from 2.6 ms to 1.9 ms, a grid of 400
cells that are each a `Sum` over 100 members from 7.3 ms to 4.8 ms, and the 1,000 largest
of 10,000 tuples by `TopCount` from 2.8 ms to 2.2 ms (2026-11-21).

`Order` by value alone (`BASC`, `BDESC`) went the way of the order within hierarchies,
which works a tuple's value out once for each of its members and compares paths. It has
a way of its own now, one value to a tuple: 10,000 tuples go from 16.5 ms to 10.7 ms, of
which reading the 10,000 cells of the answer, each a total of a hundred leaves, is about
five. The order within hierarchies is unchanged at some 31 ms for 12,769 tuples;
remembering each tuple's value there was tried and made it slower (2026-11-21).

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

Inside a transaction that had written anything, totals were neither added up in one pass
nor kept, since what others had kept was of the committed cells: the grid of 400 totals
took 59 ms in a transaction with one write pending, against 1.2 ms outside. A reader in a
transaction now keeps totals of its own for its request and adds them up in one pass over
the stored cells with the pending writes laid over them: 2.4 ms (2026-11-21).

## Model

```
cargo run --release --example bench_model
```

A model with one dimension of many leaves, under groups and one total, read from JSON
and compiled, as is done at every start and every model update (2026-11-21):

| Dimension | JSON | Read | Compiled |
| --- | --- | --- | --- |
| 100,000 leaves in groups of 100 | 5 MB | 15 ms | 56 ms |
| 1,000,000 leaves in groups of 100 | 55 MB | 147 ms | 1.0 s |
| 1,000,000 leaves under the total alone | 55 MB | 218 ms | 1.5 s |

A server on the model of a million leaves in groups of a hundred, with no cells, answers
3.0 s after it is started and holds 520 MB: about 500 bytes an element. It keeps the
model twice, as written, which `GET /model` returns and an update edits, and compiled,
with each element's parents, children and leaves and an index of names and aliases; by
reckoning rather than measurement, the two are of a size. A dimension of 100,000 leaves
takes 60 MB (2026-11-21).

## Memory

Measured from outside, on a server built for release and given a cube of four dimensions
of a hundred leaves each: a million different cells, written by name in four requests,
took the process from 6 MB to 106 MB, 105 bytes a cell, and 58 MB of log on disk
(2026-11-21). A cell is 48 bytes where it is stored, its coordinate of up to eight
dimensions and its value; the rest is the room a table keeps free so as to stay quick,
which is at its most just after the table has grown, as it had here.

Ten million cells, written in requests of half a million, took the server to 865 MB, 90
bytes a cell, and to 1,510 MB at its height. Started again, it answered in 13.9 s,
holding 792 MB with 1,044 MB at the most on the way, from a snapshot of 586 MB. The
requests that brought the log past 64 MiB at 7.5 and 9 million cells took 3.4 and 3.5 s,
nearly all of it the snapshot (2026-11-21).

That snapshot held the database for writing, so reads waited for it too. With five million
cells loaded and six writes of 250,000 cells taking the log past its limit, a client
reading one cell over and over for twenty seconds waited at most 2,097 ms. The snapshot is
now written after the request, in a task of its own reading the database, and writers
take a turn that the snapshot holds, so that none queues for the lock ahead of the
readers. The longest read is now 263 ms, as long as an ordinary write of 250,000 cells
holds the database; without the turn it was 1,224 ms. The write that arrives during the
snapshot waits for it: 1.9 s, against 0.5 s for the others (2026-11-21).

Each cell of a snapshot was then turned into a cell write before it was written, copying
the cube's name and every element's. Written from the model's names instead, five million
cells take 0.44 s to write out rather than 1.02 s, serialization alone. In the run above
the write that came during the snapshot no longer waited longer than the others: 0.8 s,
as each of them took in that run, which was slower throughout (2026-11-22).

At a million and a half cells in the same table it is 70 bytes a cell. With the
coordinate kept apart from the cell, as it was until 2026-11-21, the same two loads came
to 89 and 74 bytes a cell: the cell kept whole costs more just after the table grows and
less as it fills.

Loading past a million cells found what no test had: the snapshot that replaces the log
once it passes 64 MiB, and at every start, was written a few bytes at a time, each piece
a call to the operating system. The request that brought the log to its limit took 130
seconds, with every other request waiting on it, and a server holding a million cells
took 159 seconds to start. Written through a buffer the request takes 1.3 seconds and the
start 1.4 seconds. The snapshot is also no longer laid out for reading: laid out, a
million cells took 131 MB, about 131 bytes a cell; written close, a million and a half
take 87 MB, about 58 bytes a cell, and a server holding them starts in 2 seconds
(2026-11-21).

A snapshot named every cell before writing any, and the server's memory, 106 MB for its
cells, rose to 460 MB while it did, some 300 bytes a cell. Each cell is now named as it
is written: the peak through the same load is 177 MB. Measured as the process's largest
working set over a load of 1.5 million cells in requests of 25,000 (2026-11-21).

Starting was the same the other way: the whole snapshot was read into a list of cells by
name before any was stored, and a server that holds 104 MB once started reached 446 MB
on the way. The snapshot is now read and stored ten thousand cells at a time, and the
start peaks at 182 MB, in the same 1.7 seconds (2026-11-21).

Most of a start was then spent in two things: making every cell of the snapshot into a
cell write, its cube's name and each element's name copied, before looking those names
up; and the table of cells growing again and again as they came. The engine now reads
the cells itself, looking each name up as it is read, and the snapshot says ahead of
them how many cells each cube holds, so that room is made for them at once. Five million
cells start in 1.9 s rather than 4.9 s, and ten million in 5.6 s rather than 13.9 s,
with 976 MB at the most on the way rather than 1,044 MB (2026-11-22). A snapshot from
before, without the counts, loads as it did.

The cells a cube stores are listed a page of 10,000 at a time, in a fixed order. Each
page put in order every cell up to its end, so the later pages of a large cube were the
slowest: through a server holding 1.5 million cells the first page took 97 ms and the
last 244 ms, and all 150 pages 34.6 s. Each page now orders only its own cells: the last
takes 107 ms and all of them 24.0 s. These are times seen by a PowerShell client, which
takes most of the 97 ms itself to read a page's JSON (2026-11-21).

A model update on the same server, adding an element to a dimension, moves every one of
the 1.5 million cells to the new model and writes a snapshot: 1.1 s, and no more memory
at its height than loading the cells took. One that adds a view moves no cells and takes
14 ms (2026-11-21).

## Notes

- Leaf reads were 191 ns before they stopped building per-read lists; that change is kept.
- A consolidated read by itself costs about 32 ns per stored cell for the grand total. Reading
  one total at a time was the cost of every grid until totals were added up together and kept.
