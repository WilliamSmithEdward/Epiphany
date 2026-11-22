# Epiphany: architecture and build order

Draft. Requirements are in [AGENTS.md](../AGENTS.md). Items marked **Decision** are
defaults chosen to keep the plan moving; each is cheap to change before its milestone starts.

## 1. Shape

Two Rust crates and one .NET project:

| Part | Role |
| --- | --- |
| `epiphany-core` | The engine as a plain library: model, cell store, consolidation, rules, MDX, transactions. No HTTP, no async, no disk access except through one persistence trait. |
| `epiphany-server` | The binary: HTTP routes, JSON, sessions, the OpenAPI document, wiring to disk. Contains no OLAP logic. |
| `adapter/` | The Excel-DNA add-in (C#). Translates worksheet functions into API calls. Contains no OLAP logic. |

Everything testable about the engine is testable without a server, and the server stays a
thin translation layer.

### Runs in user space

The server must run as an ordinary user: no UAC elevation on Windows, no root on Linux. In
practice that means:

- It listens on a port above 1024, by default `127.0.0.1:8080`.
- It is a plain executable started by the user. It does not install itself as a service or
  daemon, and nothing in it needs an installer.
- It reads and writes only inside the directories it is given (the model directory now, the
  data directory at milestone 5), never system locations or the registry.
- It uses no privileged operating-system features: no raw sockets, no drivers, no locked
  memory, no changes to firewall rules or other machine settings.
- The Excel add-in is loaded per user and needs no machine-wide registration.

## 2. Engine

### 2.1 Model

- The JSON files map one-to-one onto serde structs (the *definition*).
- A definition is validated, then compiled into an immutable *model snapshot*: element names
  interned to integer ids per dimension, parent and child arrays, levels, alias lookup, and
  for every consolidated element its flattened list of `(leaf, weight)` pairs.
- Validation rejects: duplicate names, aliases that collide with names or each other, unknown
  children, cycles, consolidations with no children, cubes naming unknown dimensions,
  dimensions with no elements (a cube over one would have no cell to stand at, and a query
  of it failed in the evaluator), and hierarchies more than 100 levels deep (working one
  out goes as deep as it does, and one of fifty thousand levels ran the thread out of
  stack). Both were found by a test of odd and extreme definitions.
- A cube is an ordered list of dimensions and nothing else. There are no measures: a cell is
  exactly one element per dimension, which is also how DBRW-style formulas address it.
- Element types are `numeric`, `string` and `consolidated`.
- Attributes are declared per dimension with a type (`string`, `numeric` or `alias`) and
  valued per element. An alias is an attribute whose values also resolve as element names.
- **Decision (names):** names and aliases are compared ignoring case and spaces, so
  `product revenue` finds `Product Revenue`. Two names that differ only in case or spacing
  are duplicates.
- **Decision (string children):** a consolidation may list string elements as children for
  navigation; they contribute nothing to its value.
- **Decision (cell type):** a cell is a string cell when its element in the cube's last
  dimension has type `string`, otherwise numeric. String cells never consolidate.

### 2.2 Cell store

- Only leaf cells are stored. Per cube: a hash map from the coordinate (one leaf id per
  dimension, packed) to `f64`, and a second map for strings. Zero and empty are not stored.
- A consolidated cell is computed on read from the flattened leaf weights, by whichever is
  smaller: walking the cross product of leaf sets with point lookups, or scanning the cube's
  populated cells and testing membership.
- Consolidated cells are kept once worked out, and all forgotten at the next commit or model
  update, whichever cube it touches: a rule may read any cube. A transaction with writes of
  its own neither uses nor adds to what is kept.
- Cells read together are added up together. Where the totals among them would each pass
  over the stored cells, one pass gives every stored cell to all the totals it counts
  towards. This serves `/cells/read` batches, the cells of an MDX query's axes, and the
  cells that `Sum`, `Avg`, `Min`, `Max`, `Count`, `TopCount`, `BottomCount`, `Filter`,
  `NonEmpty` and `Order` read at each tuple of their set, where the expression names its
  members outright.
- In a cube with rules the one pass takes the totals that are sums of stored cells and
  nothing else: those no rule covers and no leaf rule reaches beneath.
- In a fed cube the totals a leaf rule reaches are added up together too: each stored or
  fed leaf is worked out once and shared out among the totals it counts towards.
- Not done: the one pass for such totals in a cube that is not fed, where each total
  visits every leaf beneath it. What is kept is not adjusted by a write but dropped, so a workload that alternates writes
  and reads of totals gains only from the one pass.

### 2.3 Transactions and locks

- Committed state is a pair: model snapshot and cell data, each with a version number.
- A transaction holds a private write set layered over committed state. Reads inside it see
  its own writes. Commit applies the write set in one step and bumps the version; rollback
  discards the write set. Nothing is ever undone in place.
- Lock modes: intent-shared, intent-exclusive, shared, exclusive, on two levels (server, cube).
  A cell write takes intent-exclusive on the server and on the cube, and no two open
  transactions may have a pending write to the same cell. A client may also lock a cube or
  the server, shared or exclusive. Model updates need the server exclusively.
- A lock request succeeds or fails at once with a conflict response; the client retries.
  Nothing waits, so nothing can deadlock.
- Reads take no locks: they see committed values plus the transaction's own pending writes.
  A client that writes a value computed from what it read must lock the cube first.
- Transactions span REST calls by id, and expire after an idle timeout so an abandoned
  client cannot hold locks.
- A batch write without a transaction id runs as its own transaction: all cells or none.

### 2.4 Model updates

- An update submits a new definition (whole model, or one dimension or cube) together with
  the version it was based on. A stale base version is rejected.
- The new definition is validated and compiled off to the side, then swapped in under the
  exclusive server lock. Readers see the old snapshot or the new one, never a mixture.
- Stored cells are re-keyed to the new element ids, matching cubes, dimensions and elements
  by name. Cells whose element was removed or turned from leaf to consolidated, or whose
  cube changed its dimensions, are dropped, and the response reports how many. A rename is
  a removal and an addition, so it drops the cells.
- An update is refused while any transaction holds a lock.
- Done: whole-model replacement, with the version and every earlier model kept on disk.
- Done: `PUT` and `DELETE` of `/dimensions/{name}` and `/cubes/{name}`, and `GET /cubes/{name}`.
  Each changes that one part of the current definition and then goes the way of a
  whole-model update, so it is checked against the version, validated as a whole and
  swapped in at once. A `PUT` to a name the model does not have adds the part.

### 2.5 Persistence

- **Decision:** serving is purely in memory; durability is a snapshot file plus an
  append-only log of committed transactions, replayed on start. The log is written before a
  commit is acknowledged. A snapshot truncates the log.
- Done. The server is given one directory: `model/` holds the current model with its
  version in `model.json`; `data/` holds `snapshot.json`, `log.jsonl`, `comments.jsonl`,
  `history.jsonl`, each sandbox under `sandboxes/<transaction>.json` and every earlier model
  under `models/<version>/`.
- A sandbox is written whole to its own file each time it changes, beside its place and then
  renamed into it, before the change is acknowledged. It names its cells, so it is placed
  afresh in whatever the model is when the server starts. A file that cannot be read stops
  the server from opening the directory, as a damaged commit does: it is someone's work.
- The history of cells, `history.jsonl`, keeps one line for each commit that changed a
  value: when, who began its transaction, and each cell's value before and after. A
  snapshot does not replace it. It is written after the commit is safe in the log, without
  waiting on the disk again, and read through for one cell at a time, apart from the
  database, so that reading a long history holds up no one.
- The engine reaches disk only through the `Journal` trait; the server crate implements it
  with files. A commit is appended to the log and flushed to disk before it is applied in
  memory; if that fails the commit is refused and the transaction stays open.
- Cells are recorded by cube and element name, so a snapshot or log loads into whatever
  model is current, dropping cells that no longer fit. That makes hand-edited model files
  and interrupted model updates safe.
- A snapshot replaces the log at every start, after every model update, and once the log
  passes 64 MiB. The last is taken by the server after the request whose commit filled
  the log, in a task of its own holding the database for reading only: readers carry on
  while it is written, and only writers wait. Taken inside the commit, it held the database
  for writing and nobody could read for the seconds a snapshot of millions of cells takes.
  A writer first takes a turn, which the snapshot holds while it is written, so that it
  waits there rather than in the lock's queue, where it would hold up every reader behind
  it. The engine by itself still takes the snapshot in the commit, unless asked to leave it
  (`defer_snapshots`).
- A model update writes the new model beside the old one, moves the old one into the
  history, and moves the new one into place; opening the directory finishes or discards an
  update that was interrupted.
- A file's contents are safe once it is flushed, but that it stands under its name is for
  its folder to record, and on Unix the folder is flushed too: after the snapshot is moved
  into place and before the log is emptied, when the log is first made, and after a model
  is moved into place. Without it a loss of power could leave the old snapshot beside the
  emptied log. This is by reasoning, not by test: nothing here cuts the power.
- The last line of the log may be a commit that was being written when the server died:
  it is left out, with a warning. Any other line that cannot be read is damage to a
  commit that was acknowledged, and the directory is then not opened: going on would drop
  every commit after it, and the snapshot taken at the start would erase them for good.
  Lines carry no checksum, so damage to the last line cannot be told from a write cut
  short, and damage that still reads as a commit cannot be told at all.
- Cost to keep in view: every commit waits for the disk, so many small commits are slower
  than one batch.

### 2.6 Rules and feeders

- Done. **Decision:** rules are written in the query language, not a second language. A
  rule is `[N: | C: | S:] area = expression`, for example
  `N: [Account].[Margin] = [Account].[Revenue] - [Account].[Cost]`. The area is a member or
  a tuple; the expression is any MDX value expression, so rules have the same functions,
  member properties and hierarchy navigation as calculated members. A cube's rules are a
  list of such statements in its JSON.
- `N:` limits a rule to leaf cells, `C:` to consolidated cells, `S:` to text cells; without
  one it covers every numeric cell of its area. The first rule that covers a cell decides
  it, unless it answers `STET` (keep the stored or consolidated value) or `CONTINUE` (ask
  the next rule).
- `LookupCube("Cube", member, ...)` reads another cube. Dimensions the two cubes share
  carry across from the cell being calculated; the members given move the rest.
- Names in a rule are checked and resolved when the model is compiled, so a mistake makes
  the model invalid and evaluation does no lookups by name.
- Evaluation is on read, remembered for the length of one request, and a cell that depends
  on itself is reported instead of looping.
- A cell is worked out inside the cell that needs it, so a chain of cells that depend on
  one another uses a thread's stack in proportion to its length, and a thread that runs
  out of stack ends the process. A chain that has taken 512 KB is left off where it
  stands; the cell it was left at is worked out first, as the beginning of a chain of its
  own, and what waited for it is tried again and finds it known. So a balance carried
  forward through any number of periods is followed, in stages, and a circle too long
  to see within one stage is still found, because a cell is left off twice.
- A cell that a rule decides cannot be written.
- Feeders are a second list of statements, `source => target, ...`, for example
  `[Account].[Units] => [Account].[Revenue]`. Wherever a stored cell lies in the source
  area, the matching target cell is fed; a fed cell feeds onward through further feeders;
  a target may be in another cube through `LookupCube`; and a source or target that names a
  consolidation stands for the leaves beneath it.
- A cube that some feeder feeds adds up its consolidations from stored and fed cells only.
  A cube that no feeder feeds visits every leaf beneath a consolidation that a leaf rule
  can reach, which is always right and can be slow; past ten million cells it refuses and
  asks for feeders.
- `GET /cubes/{cube}/feeders/check` lists calculated leaf cells that hold a value but are
  not fed, which is how missing feeders are found.
- `POST /cells/explain` says how each cell comes by its value: its kind, what is stored,
  whether it can be written, the rule that decides it, whether it is fed, and on request the
  feeders that feed it with the cell each fed it from. The lab uses it to colour cells and
  for its right-click inspector.
- Once any feeder feeds a cube, every leaf rule of that cube needs feeders: its
  consolidations then count stored and fed cells only.
- Not done: feeders whose target depends on cell values.

### 2.7 MDX

- Hand-written lexer and recursive-descent parser, with no parser dependency.
- Three stages: parse to a syntax tree; bind names against the model snapshot (names,
  aliases, unique names); evaluate sets to axes of tuples, then fetch all cells in one batch.
- Done, at `POST /mdx`:
  - `SELECT ... ON COLUMNS [, ... ON ROWS [, ...]] FROM [cube] [WHERE tuple]`, with
    `NON EMPTY` and `[DIMENSION] PROPERTIES` on an axis. Any number of axes, named
    (`COLUMNS`, `ROWS`, `PAGES`, `CHAPTERS`, `SECTIONS`) or numbered (`ON 0`, `ON AXIS(1)`).
  - Sets: braces, `.Members`, `.AllMembers`, `.Children`, `.Siblings`, ranges
    (`first : last`), `CrossJoin` and `*`, `Union` and `+`, `Except`, `Intersect`,
    `Distinct`, `Head`, `Tail`, `Subset`, `.Item`, `TopCount`, `BottomCount`, `Generate`,
    `Filter`, `Order` (`ASC`, `DESC`, `BASC`, `BDESC`), `Descendants` (distance and
    `SELF`, `AFTER`, `SELF_AND_AFTER`, `LEAVES`), `Ancestors`, `Hierarchize` (and
    `POST`), `NonEmpty`, and `TM1SubsetAll`, `TM1FilterByLevel`, `TM1FilterByPattern`,
    `TM1Sort`.
  - `Extract(set, dimension, ...)` for what the tuples of a set hold of some dimensions,
    and `[Dimension].DefaultMember`, the first top element.
  - `TM1SubsetToSet(dimension, name [, "public"])`, `TM1SortByIndex(set [, ASC | DESC])`,
    `TM1Member(tuple, place)` and `TM1TupleSize(tuple)`, written from memory of TM1 and
    not checked against it; and `.Dimension` and `.Hierarchy` of a member, a level or a
    dimension, with `.Name` and `.UniqueName` of a dimension.
  - `NonEmptyCrossJoin(set, set, ...)`, and `AddCalculatedMembers` and
    `StripCalculatedMembers` for the calculated members of a set's dimension.
  - Drilling: `DrilldownMember(set, members [, RECURSIVE])`, `TM1DrilldownMember` (which
    also takes `ALL` for the members), `DrilldownLevel(set [, level])`,
    `DrillupMember(set, members)` and `ToggleDrillState(set, members)`, which closes a
    member that is followed by something beneath it and opens any other.
  - Members: `[Dimension].[Member]` by name or alias, `.Parent`, `.FirstChild`,
    `.LastChild`, `.FirstSibling`, `.LastSibling`, `.CurrentMember`, `Ancestor`, `.PrevMember`, `.NextMember`, `.Lag(n)`,
    `.Lead(n)`, and `StrToMember`, `StrToTuple`, `StrToSet` for a member, tuple or set
    given as text.
  - Levels: `[Dimension].Levels(n)`, `[Dimension].Levels('name')`, `[Dimension].[name]`
    and `member.Level`, with `.Members`, `.Ordinal` and `.Name`; `Ancestor(member, level)`,
    `Descendants(member, level [, flag])`, `Cousin(member, ancestor)`,
    `ParallelPeriod(level [, back [, member]])`, `PeriodsToDate(level [, member])`,
    `LastPeriods(n [, member])`, `OpeningPeriod(level [, member])`,
    `ClosingPeriod(level [, member])`, and `Ytd`, `Qtd`, `Mtd` and `Wtd`, each with or
    without a member.
  - Values: arithmetic, text joined with `+`, comparisons, `AND` / `OR` / `XOR` / `NOT`,
    `IS` (the same member, tuple or level, or `NULL`), `IIF`, `CASE` in both its forms
    (`CASE WHEN condition THEN ...` and `CASE value WHEN other THEN ...`, with or without
    `ELSE`), `NULL`, `IsAncestor`, `IsEmpty`, `IsLeaf`, `CoalesceEmpty`, `Sum`, `Aggregate`, `Avg`, `Min`, `Max`,
    `Count` and `.Count`, tuples as cell references, `.Name`, `.UniqueName`, `.Value`,
    and `.Properties("name")` for an attribute or for `MEMBER_NAME`,
    `MEMBER_UNIQUE_NAME`, `LEVEL_NUMBER`, `PARENT_UNIQUE_NAME`, `CHILDREN_CARDINALITY`.
  - `Rank(tuple, set [, expression])`, and `TopSum`, `BottomSum`, `TopPercent` and
    `BottomPercent` beside `TopCount` and `BottomCount`.
  - `Median`, `Var`, `VarP`, `Stdev` and `StdevP` beside `Sum`, `Avg`, `Min` and `Max`.
  - Numbers: `Abs`, `Round`, `Int`, `Fix`, `Sqr`, `Exp`, `Log`, `Log10`, `Sgn`, `Mod`,
    `Power`. Text: `Len`, `Left`, `Right`, `Mid`, `UCase`, `LCase`, `Trim`, `InStr`,
    `Replace`, `Val`, `Str`, `CStr`.
  - `WITH MEMBER` (calculated members, which may refer to one another, with
    `SOLVE_ORDER`) and `WITH SET`.
  - Numeric and text cells.
  - A query may run inside a transaction and then sees its pending writes.
- **Decisions:**
  - A dimension the query does not mention stands at its first top element.
  - `LEVEL_NUMBER` and the `level` of a member count up from the leaves, as ELLEV does,
    not down from the top as in other MDX servers.
  - A value that cannot be computed, such as a division by zero, is null.
  - A member with several parents uses its first for `.Parent`, `Ancestor` and `Order`.
  - `.Members` lists elements only, not calculated members; `.AllMembers` adds them.
  - A cell holding zero is an empty cell, here as everywhere in the engine, so `Avg`,
    `Min` and `Max` are taken over the cells that hold something.
  - Where a cell has several calculated members and no `SOLVE_ORDER` separates them, the
    one in the cube's earlier dimension is applied last.
  - A range runs over elements in the order they are declared in the dimension.
  - `.PrevMember`, `.NextMember`, `.Lag` and `.Lead` move among the elements of the
    member's level, in the order they are declared. Past either end there is no member,
    and a tuple with no member in one of its places has no cell.
  - A level is known by its number, counted up from the leaves as everywhere here, so
    `[Period].Levels(0)` is the months of a calendar and not its top. A dimension may name
    its levels, from the leaves up, in `levels`; a name that is also an element's means
    the element, since the top of a calendar is as likely as not called Year.
  - `Ytd`, `Qtd`, `Mtd` and `Wtd` run within the levels named Year, Quarter, Month and
    Week. Without a member they end at the current member of the cube's dimension of type
    `time`.
  - What follows holds as well for `NonEmptyCrossJoin(...)` and for `NonEmpty` of a cross
    join, with or without a second set; for a long set that is no cross join, which is one
    of a single set; and for either inside `Hierarchize`, which only puts in order and is
    applied to what is left. Two long `NON EMPTY` axes, each of a dimension of thousands of
    elements, then give the few rows and columns that hold something, where every
    combination of them would be more cells than a query may return.
  - `NON EMPTY` over a cross join of sets of elements does not write the cross join out
    where it is long (over 10,000 tuples, and over a quarter of the number of cells there
    are to look through). It looks at each cell that may hold something and notes the
    tuples it counts towards, so a cross join far past the limit on sets gives its few
    rows. The cells that may hold something are the stored ones, those written in the
    transaction, and in a cube with rules the fed ones: as with a consolidation in a fed
    cube, a cell a rule gives a value that nothing feeds is left out. A cube with rules
    and no feeders, and a query with a calculated member on an axis or in the slicer,
    have their cross joins written out as before.
  - A set holds at most 1,000,000 tuples while a query is worked out, as a query returns
    at most 1,000,000 cells. A cross join that would hold more is refused before any of
    it is built, so that one short query cannot ask for more memory than there is.
  - A query takes at most 100,000,000 steps, a step being one part of an expression
    worked out, and so does a rule at one cell; a text worked out holds at most 1,000,000
    characters. Calculated members that each use the one before twice double the work
    and the text at every member, and thirty of them would otherwise never finish. The
    limit is reached after about three seconds of one processor (release build, measured
    2026-11-21).
  - A word that is a flag, such as `DESC`, `ALL` or `SELF`, is the flag wherever it stands
    alone, even in a cube with a dimension of that name. The dimension is then written in
    full, as `[Desc].[Desc]`.
  - Calculated members refer to one another at most 32 deep, and take at most 512 KB of
    the thread's stack in doing so; past either, the member is reported as referring to
    itself. The second was found by a test that asks expressions put together at random:
    a member that reads its own cell through a nest of functions ran a build without
    optimisation out of stack before it was 32 deep.
  - Expressions sit at most 32 deep inside one another, in a query, a rule, a feeder or a
    subset alike. Reading and working out an expression go as deep as it does, and the
    limit is what keeps a body of nothing but brackets from running a thread out of stack.
  - Drilling puts a member's children after it, each in its parent's place in the tuple,
    and opens a member wherever it stands, whether or not its children already follow.
    `DrilldownLevel` without a level opens the members furthest beneath the top of their
    dimension, which in a ragged hierarchy are not all of one level number.
    `DrillupMember` leaves out what follows a member and stands beneath it.
  - `LastPeriods` gives fewer members where the level ends first. `OpeningPeriod` and
    `ClosingPeriod` need their level, go down by first or by last children, and give no
    member if that way passes the level by.
  - `Cousin` and `ParallelPeriod` take the same child at each step down, so in a ragged
    hierarchy there may be no such member; then, as past the end of a level, there is none.
  - `TopSum` and `TopPercent` take tuples from the greatest down until their values reach
    the sum, or the share of the set's whole; the tuple that reaches it is the last taken.
  - `Var` and `Stdev` are of a sample and are null for one value alone; `VarP` and
    `StdevP` are of a population. Like `Avg`, all of them pass over empty cells.
  - `Rank` with an expression is one more than the number of tuples with a greater
    value, so equal values share a rank; a tuple that is not in the set ranks zero.
  - `Round` rounds halves away from zero. A root or logarithm that does not exist is null.
- Not done: functions outside the lists above (for example the date functions), and
  formatting.

### 2.8 Spreading

- Done, at `POST /cells/spread`. A value written to a consolidated cell with a method
  becomes one batch of leaf writes, inside the caller's transaction or its own.
  - **Proportional** scales the leaves that hold a value so the cell comes to the value
    given. It writes only those leaves, so it is cheap however wide the cell. Where no
    leaf holds a value it spreads equally.
  - **Equal** gives every leaf the same share, taking weights into account, so the cell
    comes to the value given. Where the weights cancel out, as with one part revenue less
    one part cost, no such share exists and the spread is refused.
  - **Repeat** gives every leaf the value itself, so the cell comes to that value times
    its leaves.
- Leaves that a rule calculates are left alone, and a cell that a rule decides cannot be
  spread into. Equal and repeat refuse a cell with more than a million leaves.

### 2.9 Subsets and views

- Done. Both are part of the model definition, so they are validated, versioned, stored and
  updated exactly as dimensions and cubes are.
- A dimension has named subsets: either a list of elements, or an MDX set over the
  dimension. A cube has named views: each an MDX query against that cube.
- A subset is worked out once, when the model is compiled, and a mistake in one makes the
  model invalid. **Decision:** an MDX subset is evaluated over its dimension alone, so it
  can use the hierarchy, levels, names and other subsets, but not cell values. Subsets are
  worked out in name order, and one may refer to those before it.
- A view is checked when the model is compiled, by running it against an empty cube.
- `TM1SubsetToSet([Dimension], "name")` uses a subset in any query.
- `GET /dimensions/{name}` lists each subset's elements; `GET /cubes/{cube}/views/{view}`
  runs a view.
- A model update that changes only subsets and views leaves the stored cells untouched.
- A subset is saved by putting its dimension, and a view by putting its cube; neither has
  a route of its own.
- Not done: subsets that depend on cell values, and private (per-user) subsets and views.

## 3. API

- JSON over HTTP. The OpenAPI document is generated from the server's route definitions,
  committed to the repository, and a test fails when the committed copy differs. The code
  and the contract cannot drift.
- Resource groups:
  - `/model`: read (done) and versioned update
  - `/dimensions/{name}`: one dimension with each element's level, parents, children and
    attributes (done)
  - `/cells/read`, `/cells/write`, `/cells/spread`: batched. Each cell names its cube, so
    one batch can span cubes, as one worksheet calculation can. A read reports errors per
    cell; a write is all or nothing and reports the first bad cell (read and write done)
  - `/mdx`: run a query (done)
  - `/cubes/{cube}/views/{view}`: run a saved view (done); subsets come with
    `/dimensions/{name}`
  - `/transactions`, `/transactions/{id}/commit`, `/rollback`, `/locks`
  - `/sessions`: sign in and out
- `GET /cubes/{cube}/cells` lists the cells a cube stores, in the order of their elements
  in each dimension and a page at a time. A page is a list of writes, so what comes out of
  one server goes into another through `/cells/write` as it is. It lists committed cells
  only, not a transaction's pending ones.
- Any data call may carry a transaction id; without one it is its own transaction.
- One error shape everywhere: a stable code, a message, and the offending coordinate when
  there is one. The adapter maps codes to worksheet results.
- Checking a password is slow and takes some 19 MiB on purpose, and anybody may ask for
  it. Four are checked at once and 64 sign-ins in all may be waiting or being checked;
  one more is answered 429 with the code `busy` and `Retry-After: 1`. A crowd of
  sign-ins can then take neither the machine's memory nor all of its threads. The add-in
  asks again after a second, up to ten times, before it reports that signing in failed.
- A request body that writes cells or changes the model holds at most 64 MiB, room for a
  model of large dimensions or some hundreds of thousands of cells in one write; a larger
  one is refused with 413 before it is read. A request that only asks for something holds
  at most 4 MiB: bodies wait in memory, and those who may only read are not given the room
  that writing needs. A request to sign in, the one request anybody may send, holds at
  most 16 KiB. The add-in reads five thousand cells to a request, however many a
  workbook asks for in one calculation.
- Each request is handled on a thread of the pool kept for work that takes time, not on
  one of the few that take requests in, so that as many slow queries as the machine has
  processors do not make a quick request wait for one of them to finish. Writes still
  wait for the queries already reading, and queries for a write. A request is handed
  over only once it has been let in and its body has arrived whole: handed over sooner,
  it kept a thread for as long as its sender took, and six hundred requests to sign in
  that never finished arriving held every thread there was. Reading the body first costs
  nothing that shows in the timing below. The hand-over costs a
  small request about 25 µs: one cell read over a kept connection took 54 µs and takes
  78 µs (2026-11-21).
- A request whose handling fails in a way nobody foresaw is answered 500 with the code
  `internal`, and the server goes on serving the others. If it was a write that failed
  part of the way through, what is in memory is in no known state: the server then stops,
  to be started again from what is on disk, where only whole commits are.
- **Decision (authentication):** users with salted password hashes in a server-side file;
  sign-in returns a bearer token for the session. Per-cube permissions are left out of the
  first version.
- Done:
  - Users are in `users.json` in the server's directory, each with an Argon2id hash of the
    password. `epiphany-server user <directory> <name>` adds a user or changes a password,
    reading the password from standard input so that it is in no command line.
  - `POST /sessions` with a name and password gives a random token; every other route
    needs it as `Authorization: Bearer <token>` and answers 401 without a live one.
    `DELETE /sessions` signs out. The contract and the lab's test page need no session.
  - A session ends when unused for thirty minutes. Sessions are in memory, so they also
    end when the server stops.
  - A directory with no users asks for no sign-in, and then the server refuses to listen
    on an address other machines can reach.
  - A wrong name takes as long to refuse as a wrong password, so the two cannot be told
    apart.
  - Transaction ids are random 53-bit numbers, so that knowing one's own does not reveal
    anyone else's.
  - Each user has a role: `read` may read cells, the model and run queries; `write` may
    also write cells, spread and use transactions and locks; `admin` may also change the
    model. A request beyond the role is answered 403. A user recorded before there were
    roles is an admin.
  - A name given five wrong passwords running is refused for a minute whatever password
    comes with it, with 429 and how long to wait. Names that are not users are counted the
    same way, so the refusal does not tell which names exist.
  - A user may have another role on a cube in place of the one that holds elsewhere:
    `none`, `read` or `write`. Reads answer for each cell's own cube; a batch of writes
    is refused whole, with the position of the write that is not allowed; a query is
    refused for the cube it selects from and for any it looks cells up in; a lock follows
    its cube, and a lock on the server takes the role that holds everywhere.
  - **Decision:** what the model's rules, feeders and saved views read is the model's
    business, not the user's. A rule may read a cube that the user reading its result may
    not, as a view may; only what the user asks for directly is checked. The model and
    its dimensions are open to anyone who may read any cube, and changed only by a user
    whose role everywhere is `admin`.
- Not done: permissions by cell or by element, TLS (put the server behind a proxy that
  provides it before using it across a network), and tying a transaction to the session
  that began it. Transaction ids are fifty-three random bits, so the last adds little. Anyone who knows a name can keep its user out by giving wrong passwords;
  the pause is short so that this is a nuisance and no more.

## 4. PAfE adapter

- Before any adapter code, write a compatibility table from observed PAfE behaviour: for
  each function, its arguments, what it returns for a missing element, a string cell read
  as a number, a consolidated write, and a lost connection, and when it recalculates.
  The adapter is then built and tested against that table rather than from memory.
- Reads are batched: calls made during one calculation pass are collected and sent as a
  single `/cells/read` request, then the cells are filled in.
- Writes (DBS, DBSW, DBSS) are queued and sent as one atomic `/cells/write`.
- Lookups (DIMNM, DIMIX, ELLEV, ELPAR, ELCOMP, ATTRS, ATTRN, DBRA) are answered from a
  per-connection metadata cache, refreshed when the model version changes.
- SUBNM resolves against subsets and aliases through the same cache.
- Done (in `adapter/`, table in `docs/PAFE-COMPATIBILITY.md`):
  - An Excel-DNA add-in for the .NET Framework, which Windows already has, with no
    libraries of its own beyond Excel-DNA.
  - Every function of the compatibility list, plus `DIMSIZ`, `ELCOMPN` and `ELPARN`.
  - Reads and writes asked for within twenty milliseconds of each other go as one request
    each, the writes first. A write the server refuses gets `*KEY_ERR` and is left out; the
    rest are sent again together.
  - Dimension functions answer from a copy of the dimension fetched once per connection.
  - Connect, Disconnect and Refresh commands; a sign-in window that opens when the server
    refuses a request; the address and user name remembered, never the password.
  - Tests that run the connection against a real server, without Excel.
- Not done, or done differently:
  - The table is recalled, not observed: no PAfE installation was at hand. Its unverified
    rows still need checking against a real PAfE workbook.
  - Every response of the server carries the model's version in a header. When an answer
    comes from another version than the one before it, the add-in forgets its copies of
    the dimensions and has every formula ask again. A change is therefore noticed at the
    add-in's next request, whatever it is for, and not before. A dimension function
    answered from a copy already held makes no request, so a workbook of nothing but those
    notices only at Refresh.
  - `adapter/check-excel.ps1` enters a formula for each row of the table in a real Excel
    and all of them pass, as does Refresh. `adapter/check-excel-windows.ps1` drives the
    Connect window, a refused and an accepted sign-in, the window that chooses a `SUBNM`'s
    element, and Disconnect. Neither runs in CI,
    which has no Excel.
  - A value typed over a `DBRW` or `DBR` cell is written to the cube and the formula put
    back. The add-in notes the formulas of the selected cells before they can be typed
    over, and works out which cube cell a formula reads, from the formula, when it is. It
    first kept that by the address the formula stood at when calculated; a row inserted or
    a range sorted moved the formulas and not the notes, and a value could then have gone
    to another formula's cell.
  - Begin transaction, Commit and Roll back in the menu: the connection then makes every
    read, write and spread within one server transaction. A transaction the server has
    rolled back for want of use is noticed at the next request; reads go on without it,
    and a write meant for it is not made, since made then it would be final.
  - An Epiphany tab on the ribbon with a button for each command, and
    `adapter/install.ps1` to have Excel load the add-in at every start for one user.
    `adapter/check-excel-ribbon.ps1` presses the buttons in a visible Excel.
  - Formulas name their environment as PAfE's do (`production:Sales`); each environment is
    a server with its own session, added with Connect.
  - See "Not replicated" in the table for the PAfE features left out.

## 5. Build order

Each milestone ends with something that can be observed working.

| # | Milestone | Done when |
| --- | --- | --- |
| 0 | Repository, workspace, formatting, lint and test checks | An empty workspace passes all checks |
| 1 | Model definition, validation, compiled snapshot, attributes and aliases | The outline's example model loads; a suite of invalid models is rejected with the right error |
| 2 | Cell store, leaf read and write, consolidation | Randomised tests agree with a brute-force aggregator; first benchmarks recorded |
| 3 | Server skeleton, OpenAPI, metadata and batched cell endpoints | Cells can be written and read over HTTP; the contract test passes |
| 4 | Transactions, locks, atomic batches, versioned model updates | Concurrent tests show isolation, rollback, conflict responses and expiry |
| 5 | Snapshot and log | A killed server restarts with every acknowledged write |
| 6 | MDX | A corpus of queries returns the expected axes and cells |
| 7 | Subsets and views | Stored subsets and views round-trip and can be used inside MDX |
| 8 | Rules, then feeders | Rule results match hand-computed cases; feeders change speed, not values |
| 9 | Spreading | The cell comes to the written value for proportional and equal, and every leaf to it for repeat |
| 10 | Authentication and sessions | Unauthenticated calls are refused; tokens expire |
| 11 | PAfE adapter | Every row of the compatibility table passes in Excel |

The adapter's compatibility table and its DBRW/DBS path can start as soon as milestone 3 is
done; it does not need to wait for 4 to 10.

## 6. Testing and performance

- Consolidation, rules and spreading are each checked against a deliberately naive
  reference implementation on random models.
- MDX is checked against a corpus of query files with expected results.
- Benchmarks exist from milestone 2 and are rerun at every milestone; an optimisation is
  only kept if they show it paid for itself.
- Dependencies are kept to an async runtime, an HTTP framework, serde, an OpenAPI generator
  and a fast hasher, with versions chosen and pinned at milestone 0.
