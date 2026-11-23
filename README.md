# Epiphany

An in-memory OLAP server written in Rust, with an Excel add-in that gives worksheets the
PAfE functions (`DBRW`, `DBS`, `SUBNM` and the rest).

- **Cubes** of numeric and text cells over dimensions with consolidations, attributes,
  aliases, subsets and saved views.
- **Rules and feeders**, written in the same expression language as queries.
- **MDX** queries, answered as JSON.
- **Writeback** in atomic batches, **spreading** to consolidations, and **transactions** with
  locks and rollback that span requests.
- **A model defined in JSON files**, updated through the API in one step and versioned.
- **Users** with roles, for the whole server, cube by cube and element by element.
- **One HTTP API**, described by [openapi.json](openapi.json), which is generated from the
  routes and checked by a test.

The server runs as an ordinary program: no administrator rights, no installer, no service.

## Try it

You need [Rust](https://rustup.rs). On Windows, double-click `start.cmd`: it builds the
server and the GUI, starts both on the sample model and opens the GUI. Anywhere, build them,

```
cargo build --release -p epiphany-server -p epiphany-gui
```

start the server on the sample, and the GUI beside it, each in a terminal of its own,

```
target/release/epiphany-server serve sample
target/release/epiphany-gui --listen 127.0.0.1:8090
```

and open <http://127.0.0.1:8090>: explore the cubes, write values back, make dashboards,
change the model and manage users. [docs/GUI.md](docs/GUI.md) says what the GUI does.

## Run the GUI

```
epiphany-gui [--listen <address>] [--server <url>] [--web <folder>]
```

The GUI is a program of its own: it serves the application, built into it, and passes the
browser's API calls on to a server. It listens on `0.0.0.0:80` and passes calls to
`http://127.0.0.1:8080` unless told otherwise; `--web` serves the application's files
from a folder instead, read afresh on every request, while they are worked on. What it
sends goes gzipped to a browser that takes it.

## Run a server

```
epiphany-server serve <directory> [address]
```

The directory holds the model in `<directory>/model` (see below), keeps its data in
`<directory>/data`, its users in `<directory>/users.json`, and the views and subsets each
keeps for themself in `<directory>/views.json` and `<directory>/subsets.json`. The address
defaults to `127.0.0.1:8080`. Every commit is written to disk before it is acknowledged, and the data is
there again after a restart.

A directory without users asks for no sign-in and listens only on this machine. To add a
user, with the password read from standard input:

```
epiphany-server user <directory> <name> [role] [cube=role ...]
```

A role is `read`, `write` or `admin`; a cube can be given another in its place, `none`
included, as in `"Payroll=none"`. The server has no TLS of its own: to use it across a
network, put it behind a proxy that provides it.

An element can be given a role of its own too, in Administration or with
`PUT /admin/users/{name}`: `"elements": { "Region": { "Europe": "none" } }`. It holds
beneath the element as well, unless a nearer element has its own, and an element beneath
several takes the highest of theirs. `none` hides the element: it is not in the user's
dimensions, queries or subsets, and its cells are refused as cells of an element there is
not, though the totals above it still count them, the same for everyone. `read` lets its
cells be read and not written, and a spread is refused where it would change a leaf the
user may not write. An administrator is bound by none.

An administrator may view the server as another user has it, to see what they see: a
session of theirs that only reads, and sees none of their sandboxes, until it is signed out
of. Each such view is recorded in `<directory>/audit.jsonl` before it is given, and
`GET /admin/audit` lists them.

## The model

A model is a folder of JSON files, one per dimension and one per cube:

```
model/
├── dimensions/
│   ├── account.json
│   └── period.json
└── cubes/
    └── general-ledger.json
```

```json
{
  "name": "Period",
  "elements": [
    { "name": "Q1", "type": "consolidated",
      "children": [{ "name": "Jan" }, { "name": "Feb" }, { "name": "Mar" }] },
    { "name": "Jan", "type": "numeric", "attributes": { "Code": "M01" } },
    { "name": "Feb", "type": "numeric" },
    { "name": "Mar", "type": "numeric" }
  ],
  "attributes": { "Code": "alias" },
  "subsets": { "Months": { "mdx": "TM1FilterByLevel([Period].Members, 0)" } }
}
```

```json
{
  "name": "General Ledger",
  "dimensions": ["Account", "Entity", "Period", "Version"],
  "rules": ["[Account].[Margin %] = IIF([Account].[Revenue] = 0, 0, [Account].[Margin] / [Account].[Revenue])"],
  "views": { "By quarter": { "mdx": "SELECT [Period].Levels(1).Members ON COLUMNS FROM [General Ledger]" } }
}
```

[sample/model](sample/model) is a complete example. Names are matched ignoring case and spaces, and
an element answers to its aliases as well as its name. A dimension may also name its levels,
from the leaves up, and say that it is the cube's calendar:

```json
{ "name": "Period", "type": "time", "levels": ["Month", "Quarter", "Year"], "elements": [ ... ] }
```

A query can then say `[Period].Levels('Quarter').Members`, and `Sum(Ytd(), ...)` for a figure
to date.

A subset in MDX may read cells, of any cube, through `LookupCube`: the region with the year's
largest sales, say.

```json
"subsets": { "Leader": { "mdx": "TopCount([Region].[World].Children, 1, LookupCube('Sales', [Period].[Year]))" } }
```

Such a subset is worked out whenever it is used, as the cells then stand: in a query as the
query reads them, a transaction's pending values included, and by `GET /dimensions/{name}`
as they are stored. A picklist cannot offer one.

A string element may give its cells a picklist: values listed, or the elements of a dimension
or of one of its subsets, in order.

```json
{ "name": "Owner", "type": "string", "picklist": ["Ada", "Grace", "Lin"] }
{ "name": "Region", "type": "string", "picklist": { "dimension": "Entity", "subset": "Regions" } }
```

A write of other text to such a cell is refused (`not_offered`), saying what is offered.
Emptying a cell is always allowed, and text stored before the picklist was given is kept.

A cube may check the values in its cells. A check names the cells it checks as a rule's
area does, and an expression, written as a rule's is, that holds where a value is acceptable:

```json
"validations": [
  { "cells": "N: [Account].[Salaries]", "valid": "[Account].[Salaries] >= 0",
    "message": "Salaries are never negative.", "refuse": true },
  { "cells": "[Account].[Travel]", "valid": "[Account].[Travel] <= [Account].[Travel Budget]",
    "message": "Travel is over its budget." }
]
```

A value a refusing check does not hold for is refused as it is written (`invalid_value`, with
the check's message), and its whole batch with it. A value that breaks a check that only warns
is kept, and `POST /cells/explain` lists, for each cell, the checks its value breaks.

## The API

Everything is JSON over HTTP. The contract is [openapi.json](openapi.json), also served at
`/openapi.json`.

| Route | Purpose |
| --- | --- |
| `POST /sessions`, `DELETE /sessions` | Sign in and out |
| `GET /model`, `PUT /model` | The whole model, and replacing it against its version |
| `GET /model/outline` | The model without its dimensions' elements: its cubes and dashboards, and each dimension's name, type and size |
| `GET`, `PUT`, `DELETE /dimensions/{name}` | One dimension, with its hierarchy and subsets |
| `GET`, `PUT`, `DELETE /cubes/{name}` | One cube, with its rules, feeders and views |
| `POST /cells/read` | A batch of cells, each with its value or its own error |
| `POST /cells/write` | A batch of leaf cells: all of them or none |
| `POST /cells/detail` | A leaf cell's supporting detail, the lines its value is the sum of, written with the sum |
| `POST /cells/spread` | A value shared out among the leaves of a consolidation, leaving any cells held as they are |
| `POST /cells/explain` | How each cell comes by its value: stored, a rule, a total, fed or not |
| `POST /cells/history` | The changes saved to a cell, the newest first: when, by whom, and from what to what |
| `GET /cubes/{cube}/cells` | The cells a cube stores, a page at a time, in a form `/cells/write` takes back |
| `GET`, `POST /cubes/{cube}/comments`, `DELETE /cubes/{cube}/comments/{id}` | Comments pinned to cells, signed by their authors |
| `POST /mdx` | An MDX query, optionally stood at other members in place of those its WHERE names (`context`) |
| `GET /cubes/{cube}/views/{view}` | A saved view |
| `GET /cubes/{cube}/private-views`, `PUT`, `DELETE /cubes/{cube}/private-views/{name}` | The signed-in user's own views of a cube, which nobody else sees |
| `GET /dimensions/{dimension}/private-subsets`, `PUT`, `DELETE /dimensions/{dimension}/private-subsets/{name}` | The signed-in user's own subsets of a dimension: lists of members, which nobody else sees |
| `GET /cubes/{cube}/feeders/check` | Calculated cells that no feeder feeds |
| `GET /admin/models`, `GET /admin/models/{name}` | The earlier versions of the model the server keeps; one is restored by `PUT /model` |
| `GET /admin/changes` | The changes saved to cells across cubes, the newest first, by cube, user and time |
| `POST /admin/view-as`, `GET /admin/audit` | A view of the server as another user has it, reading only, and the audit that records each |
| `POST /transactions`, then `/transactions/{id}/commit`, `rollback`, `locks`, `point` and `undo` | Transactions, and taking one back to a point it came to |
| `GET`, `POST /sandboxes`, `DELETE /sandboxes/{name}` | The signed-in user's sandboxes: transactions kept under a name |

A read, a write or a query can carry a transaction id; without one it stands alone.

A leaf cell may keep supporting detail: lines, each a label and an amount, that its value is
the sum of, such as a travel budget's flights and hotels. The detail is written with the sum,
as one write, and waits, commits and rolls back with it. A value written to the cell plainly
takes the detail away, for its lines no longer add up to it. `POST /cells/explain` gives a
cell's detail with the rest of what it says of the cell.

A user may keep views of their own beside a cube's, which everyone sees: queries saved for
them alone, with the same sizes, number format and highlights a cube's view may have. Anyone
who may read a cube may keep views of it; the query is run as it is saved, and must select
from that cube. A user may keep subsets of their own too: lists of a dimension's members,
named or by alias and kept by name, which leave out a member the dimension no longer has.
Their queries, and their own views, may take them as `TM1SubsetToSet([Period], 'Spring',
'private')`; nobody else's can, and neither can the model's views, subsets and rules. A
user's views, and their subsets, take at most 4 MiB each, and go when the user does. A
server without users keeps none: its views and subsets are everybody's.

A sandbox is a transaction its owner keeps under a name, for values to be tried out in
before anyone else sees them. It holds no locks, so it holds up nobody, never expires, and
outlives the server; a change to the model carries its values to wherever their elements
are. Its values are read, written, spread and queried with its transaction id, as any
transaction's. Committing it publishes them, checked and locked as any commit is, into the
cubes its owner may write to, and leaves it open and empty; rolling it back empties it.

```
curl -s http://127.0.0.1:8080/mdx -H "content-type: application/json" -d "{\"mdx\": \"SELECT {[Period].[Q1].Children} ON COLUMNS, {[Account].[Revenue]} ON ROWS FROM [General Ledger]\"}"
```

`NON EMPTY` over a long set or cross join does not work through every combination: the rows
that hold something are found from the stored cells, so a view of several large dimensions
down its rows takes the time its data needs and not the time its size would. A cube with
rules needs feeders for this, as it does for its totals to be quick.

## Limits

Each is there so that one request cannot take the server from the others. A request past a
limit is refused with an ordinary error that says which.

| Limit | Value |
| --- | --- |
| Cells a query returns | 1,000,000 |
| Tuples in a set while a query is worked out | 1,000,000 |
| Expressions inside one another | 32 deep |
| Levels in a hierarchy | 100 |
| Steps to work out one query, or one rule at one cell | 100,000,000 (about three seconds) |
| Characters in a text an expression builds | 1,000,000 |
| A request body | 64 MiB to write cells or change the model; 4 MiB to ask for anything; 16 KiB to sign in |
| Sign-ins waiting or being checked | 64, four checked at a time; one more is told to come back in a second |

[docs/PLAN.md](docs/PLAN.md) gives the reasons.

## Sizing

Measured on one Windows machine with a build for release, a cube of four dimensions:

| | |
| --- | --- |
| Memory a stored cell takes | 29 to 39 bytes at ten million cells, where the cube's element ids fit 64 bits together; 70 to 105 bytes where they do not |
| Memory at its height | starting, about 1.7 times what the cells take; writing millions of cells in large requests, about twice, the requests taking room of their own |
| Snapshot on disk | about 58 bytes a cell |
| Start, 1.5 million cells | 2 seconds |
| Write by name, through the server | 50,000 cells in about 35 ms |
| Read by name, through the server | 5,000 cells in about 4 ms |
| Memory a dimension element takes | about 500 bytes |
| Start, a dimension of a million elements | 3 seconds |
| Ten million cells | about 360 MB held, 740 MB at the most; a 586 MB snapshot; a start of 6 seconds |

Every 64 MiB of log, about a million cells written, the log is replaced by a snapshot,
written while reads carry on; writes wait for it, at ten million cells some 3.5 seconds.
[docs/BENCHMARKS.md](docs/BENCHMARKS.md)
has the queries and how to repeat every figure.

## Excel

[adapter](adapter) is an Excel-DNA add-in: one `.xll` file that needs nothing installed
beside Excel. Formulas name their cube as PAfE's do, `"production:General Ledger"`, read and
write in batches, write back a value typed over a `DBRW`, and choose a `SUBNM`'s element from
a list. [adapter/README.md](adapter/README.md) says how to build and use it, and
[docs/PAFE-COMPATIBILITY.md](docs/PAFE-COMPATIBILITY.md) what each function returns.

The compatibility table was written from the documented behaviour of the TM1 worksheet
functions, not from a PAfE installation; its unverified rows still need checking against one.

## Develop

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
dotnet test adapter/Epiphany.Excel.Tests
```

The adapter's tests need `cargo build -p epiphany-server` first and the .NET SDK; they start
a real server and need no Excel. Three scripts in [adapter](adapter) check the add-in inside
Excel itself.

The GUI's main journeys are taken in a browser, each page checked for accessibility as they
go; they need Node and Chrome or Edge, and install nothing:

```
cargo build -p epiphany-server -p epiphany-gui
node crates/epiphany-gui/tests/browser/journeys.mjs
```

[docs/GUI.md](docs/GUI.md) says what they check.

| Path | What is there |
| --- | --- |
| `crates/epiphany-core` | The engine: model, cells, rules, MDX, transactions. No HTTP, no disk |
| `crates/epiphany-server` | The program: routes, sign-in, files on disk |
| `crates/epiphany-gui` | The browser application, and the program that serves it and passes its calls on; its journeys in a browser in `tests/browser` |
| `adapter` | The Excel add-in and its tests |
| `sample` | A complete model to try, which `start.cmd` serves with the GUI; the journeys run on a copy |
| `docs/PLAN.md` | How it is built, what was decided and why, and what is not done |
| `docs/GUI.md` | The GUI: what was decided, what it learnt from other tools, and how it is checked |
| `docs/BENCHMARKS.md` | Timings, and how to repeat them |
