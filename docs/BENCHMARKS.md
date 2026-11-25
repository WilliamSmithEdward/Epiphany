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
rather than 882 MB; started again, the server holds 279 MB rather than 792 MB, with 464
MB at the most on the way, and starts in the same 5.5 s.

The passes over all the stored cells now go through each cell in turn with its elements
unpacked into the same place, rather than handing out each cell's place: the stored-cell
total takes 3.6 ms, the 400 totals of two dimensions 1.8 ms, the ten largest of a hundred
3.1 ms, the 113 totals of three dimensions 6.1 ms and the 113 over feeders one at a time
27.6 ms, against 4.0, 1.8, 2.9, 5.3 and 28.5 ms with coordinates kept whole, measured one
after the other (2026-11-22).

Their height while written seemed to be 1.2 GB, against 1.5 GB before; followed batch by
batch, it was 740 MB until the end, and the rest was one page of the cube's cells asked
for afterwards. Listing them gathered the place of every cell to set the page in order,
56 bytes a cell once coordinates were packed. Packed coordinates keep the first
dimension in their highest bits, so that as numbers they are in the order of their
elements, and the page is now set in order from the numbers, 8 bytes a cell: the same
ten million cells peak at 742 MB (2026-11-22).

A write of 500,000 cells to a server that held none took it from 24 MB to 362 MB on the
way. Part of that was a second copy of every cell by name, made at the commit for the log
only to be written out as one line. The log is now written from names borrowed from the
model, to the same text, and the same write reaches 255 MB and takes 0.75 s rather than
0.91 s (2026-11-22). Most of what is left is the request itself, read whole into cells by
name before any is stored.

Read into cell writes, half a million cells took 266 bytes a cell, two and a half million
small texts, each element's name looked up again for every cell it was in. A request's
cells are now read into a batch that keeps each name once and each cell as the numbers of
its names and its value, and each name is looked up once. The write of 500,000 cells
reaches 157 MB and takes 0.48 s; 50,000 cells take 32.9 ms through the server rather
than 55.7 ms (2026-11-22).

Cells read by name are taken in the same way, and the same names looked up once for a
request. Read where nothing is stored, 50,000 cells took 12 ms to take in and 11 ms to
look up; they take 5.8 ms and 3.3 ms. Through the server 5,000 cells are answered in 3.8
ms rather than 6.2 ms, and 50,000 in 31.5 ms rather than 53.8 ms (2026-11-22).

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

Without feeders, such a consolidation kept every leaf it visited with the cells that rules
work out, for whatever read them next: a million for the grand total, though only one leaf
in a hundred is calculated. It now looks up a leaf that no rule covers as it is stored, and
keeps only those a rule works out. Totals of that kind read together, which each walked the
leaves beneath them, now walk every combination of those leaves once, each leaf shared out
among the totals it counts towards, where that is no more than the totals would visit one
by one. The grid is every consolidation of the first two dimensions by every other, at the
calculated element and at the total of the last: totals nested in one another. Rerun the
same day, before and after (2026-11-22):

| Read, without feeders | Before | After |
| --- | --- | --- |
| Total of the calculated element | 4.1 ms | 4.0 ms |
| Grand total | 308 ms | 11 ms |
| 113 totals of the calculated element, one at a time | 12.8 ms | 12.4 ms |
| The same 113 totals read together | 5.2 ms | 5.1 ms |
| A grid of 338 totals, one at a time | 1,450 ms | 94 ms |
| The same grid read together | 1,040 ms | 37 ms |

With feeders, whose reading did not change, the grid takes 107 ms one at a time and 12 ms
together.

A leaf that no rule covers was still worked out as rules work cells out, and remembered for
the rest of the request, though it is only what is stored: each calculated cell here reads
one so, and kept it. It is now looked up as it is stored, and only what rules decide is
remembered. Rerun the same day, before and after (2026-11-22):

| Read | Without feeders, before | After | With feeders, before | After |
| --- | --- | --- | --- | --- |
| Total of the calculated element | 4.2 ms | 2.8 ms | 10.0 ms | 8.1 ms |
| Grand total | 11.4 ms | 9.7 ms | 4.4 ms | 3.0 ms |
| 113 totals, one at a time | 12.4 ms | 8.0 ms | 28.7 ms | 24.8 ms |
| The same 113 read together | 5.2 ms | 3.9 ms | 11.8 ms | 9.4 ms |
| A grid of 338 totals, one at a time | 96 ms | 78 ms | 107 ms | 96 ms |
| The same grid read together | 37 ms | 35 ms | 12.2 ms | 10.1 ms |

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

Compiling an element wrote out the words a refusal of it would need, whether or not there
was one; checked its children for a name given twice by indexing their names afresh, a
text made for each; and gathered a consolidation's leaves in a sorted map one at a time, a
million of them for a total over a million leaves. The words are now written for a refusal
only; the children are looked up by the key of their names, made where the last was, and a
name given twice is found by its element; and the leaves are gathered in a list put in
order once, which costs little where they come in order already, as children's mostly do.
Rerun the same day, before and after (2026-11-23):

| Dimension, compiled | Before | After |
| --- | --- | --- |
| 100,000 leaves in groups of 100 | 55 ms | 37 ms |
| 1,000,000 leaves in groups of 100 | 829 ms | 611 ms |
| 1,000,000 leaves under the total alone | 3.0 s | 0.64 s |

Every compiled element held a picklist and the dimension it offers, which few elements have:
an element took 160 bytes in place. Picklists are now kept beside a dimension's elements, by
element, and an element takes 128 bytes, 32 MB less for a dimension of a million. Compiling
is as quick, if anything quicker: best of three, alternated, 36, 594 and 592 ms before and 34,
579 and 592 ms after for the three dimensions above (2026-11-23).

A leaf's leaves are itself, with weight one, yet each leaf kept them in a list of its own, a
small allocation for every leaf of a model. That one leaf is now kept in place, and only a
consolidation's leaves in a list. Best of three, alternated, the three dimensions compile in
31, 530 and 532 ms rather than 33, 577 and 581, and a million leaves allocate a million
times less; reading cells is unchanged, a consolidation by combinations 38.9 µs against 39.6
(2026-11-23).

Each element kept its parents in a list of its own too, though nearly every element has one.
One parent is now kept in place, and only none or several in a list, each made once at its
length, the parents counted first. Best of six, alternated, on a machine busier than for the
runs above: 29, 507 and 711 ms rather than 30, 534 and 808 (2026-11-23).

The server on the model of a million leaves in groups of a hundred, started again and again,
says it is listening 0.72 to 0.75 s after it is started, and holds 480 MB. The 3.0 s above
was a first start by the script that had just written the model and still held it as text:
started from another process once the writer has gone, the same first start takes 0.75 s.
Timed by the server's own line rather than by asking it whether it answers (2026-11-23).

Saving a small dashboard on that model took 1.24 s and took the server from 476 MB to 1,285
MB at the most. Timed inside the server, the save copied the whole definition to change it
(89 ms), compiled every element again (520 ms), copied the old definition and the new once
more to see whether any cell had moved (275 ms), wrote the model out (240 ms) and let the old
one go (115 ms). Each dimension's definition is now kept once for every copy of the model it
is unchanged in, and the first change to it in a copy copies it; a dimension whose definition
is as it was keeps its compiled elements and their names, its subsets and picklists worked
out again; and the old model and the new are compared where they are, a dimension they share
without a look inside it. `bench_model` compiles a dashboard more in 0.23, 2.5 and 2.7 ms
for the three dimensions above, where compiling them anew takes 26, 547 and 533 ms. Measured
from outside, three saves each (2026-11-23):

| Saving a dashboard, a million leaves | Before | After |
| --- | --- | --- |
| Time to answer | 1.23 to 1.26 s | 0.24 to 0.26 s |
| Memory at the most | 1,285 MB | 483 MB, as at the start |

Reading through a dimension's shared elements costs nothing that shows: bench_cells, bench_mdx
and bench_rules, built from the commit before and after and run alternately, twice or three
times each, are within a few per cent of each other on every line (2026-11-23).

What is left of a save is writing the model out again, every dimension of it, and keeping the
model it replaces: 131 MB on disk for a million leaves, laid out, at every save.

A dimension of more than 100,000 elements is now written close rather than laid out, as a
snapshot is: no one edits one by hand, and the million leaves take 65 MB rather than 131. The
same saves, three each, measured from outside (2026-11-23):

| Saving a dashboard, a million leaves | Laid out | Close |
| --- | --- | --- |
| Time to answer | 0.24 to 0.27 s | 0.13 to 0.14 s |
| The model on disk, and kept again at every save | 131 MB | 65 MB |

`GET /dimensions/{name}`, which the model's editor and the Excel add-in ask for, sends that
dimension as 94 MB of JSON. It took 0.66 s and took the server from 476 MB to 916 MB: each
element's name, its parents' and its children's were copied out of the model, then written.
The answer is now written straight from the model, the names borrowed, as it is sent, and is
the same byte for byte for every dimension of the sample model and one more with what they
lack. Measured from outside, twice each (2026-11-23):

| A dimension of a million leaves, whole | Before | After |
| --- | --- | --- |
| Time to answer | 0.65 to 0.67 s | 0.32 s |
| Memory at the most | 916 MB | 597 MB |

The Excel add-in read that answer with the serializer the .NET Framework ships, which made
the whole text into dictionaries and lists before the add-in made its copy of the dimension
from them. It now reads the answer with a JSON reader of its own as the text comes, making
each element as it is read. Its copy gives the same answer to every question the dimension
functions ask, about every element of the sample model's dimensions and every 97th of the
million, as the copy made the old way. Measured with a test against a release server on the
model of a million leaves, not kept, three times after (2026-11-23):

| The add-in, a dimension of a million leaves | Before | After |
| --- | --- | --- |
| Time to the first answer, fetching included | 11.4 s | 2.9 s |
| Reading the answer, once fetched | 10.4 to 11.4 s | 2.4 to 2.6 s |
| Memory at the most | 2,162 MB | 576 to 591 MB |
| The copy, held | 418 MB | 418 MB |

The copy's list of names and its index of names were grown a name at a time, made again
larger and larger. They are now made as large as they will be at once. Measured the same
way, the old and the new in turn, twice each (2026-11-23):

| The add-in, a dimension of a million leaves | Grown | Made at once |
| --- | --- | --- |
| Memory at the most | 579 to 580 MB | 524 to 540 MB |
| The copy, held | 417 MB | 411 MB |
| Time to the first answer, fetching included | 2.35 to 2.87 s | 2.24 to 2.35 s |

The model's editor, opening that dimension, read its 65 MB definition as it came (258 ms),
wrote it out again as the original to go back to (95 ms), and read that again as the copy
to work on (240 ms). It now keeps the text it came as for the original and reads it once;
what the copy is compared with to tell whether it changed is written out the first time it
is asked, at the first change. A release server and GUI, the dimension opened from the
model's page (2026-11-23):

| The model's editor, a dimension of a million leaves | Before | After |
| --- | --- | --- |
| Opened and drawn | 1,830 to 1,889 ms | 1,537 to 1,591 ms |
| The page's heap, once open | 375 MB | 193 to 202 MB |

What it works out from the dimension then made a set for each element as it worked out the
levels, each by default; looked every leaf up again on the way down to their depths; gave
each leaf two empty lists of children; and indexed the elements by name from a pair made for
each. A profile of the page put a third of the time in those. One set now serves, the way
down passes a leaf by its element, a leaf's children are one empty list for all, and the
index is set one element at a time; the levels, depths, roots and parents of the million are
the same as before. Measured the same way (2026-11-23):

| The model's editor, a dimension of a million leaves | Before | After |
| --- | --- | --- |
| Opened and drawn | 1,537 to 1,591 ms | 1,333 to 1,398 ms |
| The page's heap, once open | 193 to 202 MB | 167 to 170 MB |

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

The history of cells is kept for good, a line for each commit, and was read from its start
for every question asked of it. On a history of two million commits, 287 MB, against a
release server, the best of three, read from its end instead (2026-11-23):

| Asked | From its start | From its end |
| --- | ---: | ---: |
| The newest 200 changes | 1,209 ms | 1.1 ms |
| The next 200 | 1,254 ms | 1.0 ms |
| 200 from a month back | 1,232 ms | 20 ms |
| One cell's newest 100 | 1,073 ms | 1.4 ms |
| One user's newest 200 | 1,275 ms | 1.2 ms |

The answers to twelve questions, of the newest, of parts far back and of the very first
commits, by time, by user and by cell, are the same from both, byte for byte. A part far
back passes over the lines newer than it by the time each begins with, without reading
them as JSON.

## Starting

```
cargo run --release -p epiphany-server --example bench_start [cells]
```

A directory whose cube of four dimensions of a hundred leaves holds the cells at its first
coordinates, written in commits of 250,000; opened three times once the log has become a
snapshot, so that each opening reads the model and the snapshot alone, as a server starting
does (2026-11-24):

| Cells | Written | Snapshot | Opened |
| --- | --- | --- | --- |
| 1,000,000 | 1.12 s | 60 MB | 0.33 s |
| 5,000,000 | 8.30 s | 302 MB | 1.68 s |

The snapshot is read as it comes, through a buffer, and never held whole. Held whole and
read from memory, the million open in 0.25 s rather than 0.33, but the file is then held
beside the cells as they are stored, 60 MB a million: the peak memory of a start was
brought down from 446 MB to 182 MB for a million and a half cells, and a quarter of the
time is not worth giving that back. A buffer of a megabyte in place of eight kilobytes
opens them in 0.32 s. Neither change is made.

### A wide model

```
cargo run --release -p epiphany-server --example bench_wide [dimensions]
```

Ten thousand dimensions of twenty leaves and 500 cubes of five of them, each a file of its
own: opened while its files are new, opened again, then a dashboard saved, and opened once
more (2026-11-24):

| | One file at a time | Many at once | Only what changed written |
| --- | --- | --- | --- |
| Opened first | 0.58 s | 0.37 s | 0.41 s |
| Opened again | 0.50 s | 0.30 s | 0.39 s |
| A dashboard saved | 13.30 s | 9.95 s | 2.15 s |
| Opened after the save | 0.69 s | 0.38 s | 0.47 s |

Files are read and written by twice as many threads as the machine runs at once. On this
machine a disk scanned for malware as files are first read makes the difference greatest:
a model of 10,000 dimensions and 10,800 files (71 MB), written by another program, took 78 s
to read the first time one file at a time, 7.6 s with 8 threads, 3.9 s with 32, and no
less with 64 or 128; read again, 0.87 s one at a time and 0.26 s with 8. A server starting on
it waited 83 s before it answered, and waits 4 s now. Writing gains less: a save's files,
written, flushed and renamed, took 13.9 s one at a time, 8.8 s with 4 threads and no less
with more.

A save now writes only what changed, here the dashboard and the model's version, and
hard-links each other file into the new model, the model kept and the new one sharing it: the
earlier models of a save then hold 153 bytes of their own each rather than 93 MB. Links are
made one after another: many at once, the 10,000 dimensions' took 3.6 to 4.3 s against 2.4 s.
A link takes about 0.3 ms however many the file already has, to the 1,023 Windows allows, past
which the file is written again and linked from then on (2026-11-24).

### Explore on a dimension of a million leaves

A cube of a dimension of a million leaves, in 10,000 groups of a hundred under one total, and
a measure of two, opened in Explore, a release server on this machine, measured in the page
(2026-11-24):

| | Whole | In parts, children listed | In parts, children counted |
| --- | --- | --- | --- |
| The grid shown | 1,804 ms | 83 ms | 81 ms |
| The page's memory | 299 MB | 4 MB | 3 MB |
| Fetched | 109 MB (6 MB sent) | 341 KB | under 1 KB |
| The total opened, its 10,000 groups shown | as quick | 907 ms, 98 MB | 438 ms, 12 MB |
| An item found by name and opened to | as quick | 151 MB | 16 MB |

Whole, the dimension came at once, to be looked through in the page. In parts it comes as it
is shown, by `POST /dimensions/{name}/elements`: the top elements, what the layout names, a
member's children as it is opened, and a search the server makes. Each group sent once
listed its hundred children, a million between them, though none was opened; an element now
lists its children only where the page is to go down from it, and counts them otherwise.

## Notes

- Leaf reads were 191 ns before they stopped building per-read lists; that change is kept.
- A consolidated read by itself costs about 32 ns per stored cell for the grand total. Reading
  one total at a time was the cost of every grid until totals were added up together and kept.
