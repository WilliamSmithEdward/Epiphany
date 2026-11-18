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
  children, cycles, consolidations with no children, cubes naming unknown dimensions.
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
- No caching in the first version. A consolidation cache keyed by data version is added only
  if benchmarks show the need.

### 2.3 Transactions and locks

- Committed state is a pair: model snapshot and cell data, each with a version number.
- A transaction holds a private write set layered over committed state. Reads inside it see
  its own writes. Commit applies the write set in one step and bumps the version; rollback
  discards the write set. Nothing is ever undone in place.
- Lock modes: intent-shared, intent-exclusive, shared, exclusive, on two levels (server, cube).
  Cell writes take intent-exclusive on the server and exclusive or intent-exclusive on the
  cube; model updates take exclusive on the server.
- Lock requests either succeed or fail after a bounded wait with a conflict response. There
  is no deadlock detector, because nothing waits forever.
- Transactions span REST calls by id, and expire after an idle timeout so an abandoned
  client cannot hold locks.
- A batch write without a transaction id runs as its own transaction: all cells or none.

### 2.4 Model updates

- An update submits a new definition (whole model, or one dimension or cube) together with
  the version it was based on. A stale base version is rejected.
- The new definition is validated and compiled off to the side, then swapped in under the
  exclusive server lock. Readers see the old snapshot or the new one, never a mixture.
- Stored cells are re-keyed to the new element ids. Cells whose element was removed, or
  turned from leaf to consolidated, are dropped, and the response reports how many.
- Every committed model version is kept on disk as a numbered copy; the newest is `model/`.

### 2.5 Persistence

- **Decision:** serving is purely in memory; durability is a snapshot file plus an
  append-only log of committed transactions, replayed on start. The log is written before a
  commit is acknowledged. A snapshot truncates the log.

### 2.6 Rules and feeders

- **Decision:** an original, small rules language in the familiar area-equals-expression
  form, for example `['Margin'] = N: ['Revenue'] - ['Cost'];`, with `N:` for leaf cells and
  `C:` for consolidated cells, arithmetic, `IF`, and cross-cube references.
- Rules compile to an expression tree per cube. Evaluation is on read, memoised per query,
  with circular-reference detection.
- Rules land first without feeders: correct, but consolidations over rule-calculated leaves
  must visit every candidate leaf. Feeders are then added as the optimisation they are:
  they mark which calculated leaves can be non-zero so consolidation and NonEmpty skip the
  rest. A check mode reports cells that have a value but no feeder.

### 2.7 MDX

- Hand-written lexer and recursive-descent parser, with no parser dependency.
- Three stages: parse to a syntax tree; bind names against the model snapshot (names,
  aliases, unique names); evaluate sets to axes of tuples, then fetch all cells in one batch.
- Functions arrive in dependency order: members and tuples, set braces and `.Members` /
  `.Children`, CrossJoin, Descendants and Ancestors, Hierarchize, Order, Filter, NonEmpty,
  then `WITH MEMBER` and `WITH SET`, then properties.

### 2.8 Spreading

- A write to a consolidated cell with a spread method (proportional, equal, repeat) becomes
  a set of leaf writes inside one transaction. Proportional spreading over all-zero leaves
  falls back to equal.

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
  - `/mdx`: run a query
  - `/subsets`, `/views`: stored selections and stored queries
  - `/transactions`, `/transactions/{id}/commit`, `/rollback`, `/locks`
  - `/sessions`: sign in and out
- Any data call may carry a transaction id; without one it is its own transaction.
- One error shape everywhere: a stable code, a message, and the offending coordinate when
  there is one. The adapter maps codes to worksheet results.
- **Decision (authentication):** users with salted password hashes in a server-side file;
  sign-in returns a bearer token for the session. Per-cube permissions are left out of the
  first version.

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
| 9 | Spreading | Leaf totals equal the written value for each method |
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
