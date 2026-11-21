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
- **Users** with roles, for the whole server or cube by cube.
- **One HTTP API**, described by [openapi.json](openapi.json), which is generated from the
  routes and checked by a test.

The server runs as an ordinary program: no administrator rights, no installer, no service.

## Try it

You need [Rust](https://rustup.rs). On Windows, double-click `lab\start.cmd`; anywhere, run

```
cargo run --release -p epiphany-server -- lab lab
```

and open <http://127.0.0.1:8080/lab>. The lab is a page for trying the server by hand: drag
dimensions onto rows and columns, type into cells, write MDX, begin and commit transactions.
[lab/README.md](lab/README.md) says what everything on it does.

## Run a server

```
epiphany-server serve <directory> [address]
```

The directory holds the model in `<directory>/model` (see below), keeps its data in
`<directory>/data` and its users in `<directory>/users.json`. The address defaults to
`127.0.0.1:8080`. Every commit is written to disk before it is acknowledged, and the data is
there again after a restart.

A directory without users asks for no sign-in and listens only on this machine. To add a
user, with the password read from standard input:

```
epiphany-server user <directory> <name> [role] [cube=role ...]
```

A role is `read`, `write` or `admin`; a cube can be given another in its place, `none`
included, as in `"Payroll=none"`. The server has no TLS of its own: to use it across a
network, put it behind a proxy that provides it.

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

[lab/model](lab/model) is a complete example. Names are matched ignoring case and spaces, and
an element answers to its aliases as well as its name. A dimension may also name its levels,
from the leaves up, and say that it is the cube's calendar:

```json
{ "name": "Period", "type": "time", "levels": ["Month", "Quarter", "Year"], "elements": [ ... ] }
```

A query can then say `[Period].Levels('Quarter').Members`, and `Sum(Ytd(), ...)` for a figure
to date.

## The API

Everything is JSON over HTTP. The contract is [openapi.json](openapi.json), also served at
`/openapi.json`.

| Route | Purpose |
| --- | --- |
| `POST /sessions`, `DELETE /sessions` | Sign in and out |
| `GET /model`, `PUT /model` | The whole model, and replacing it against its version |
| `GET`, `PUT`, `DELETE /dimensions/{name}` | One dimension, with its hierarchy and subsets |
| `GET`, `PUT`, `DELETE /cubes/{name}` | One cube, with its rules, feeders and views |
| `POST /cells/read` | A batch of cells, each with its value or its own error |
| `POST /cells/write` | A batch of leaf cells: all of them or none |
| `POST /cells/spread` | A value shared out among the leaves of a consolidation |
| `POST /cells/explain` | How each cell comes by its value: stored, a rule, a total, fed or not |
| `GET /cubes/{cube}/cells` | The cells a cube stores, a page at a time, in a form `/cells/write` takes back |
| `POST /mdx` | An MDX query |
| `GET /cubes/{cube}/views/{view}` | A saved view |
| `GET /cubes/{cube}/feeders/check` | Calculated cells that no feeder feeds |
| `POST /transactions`, then `/transactions/{id}/commit`, `rollback`, `locks` | Transactions |

A read, a write or a query can carry a transaction id; without one it stands alone.

```
curl -s http://127.0.0.1:8080/mdx -H "content-type: application/json" -d "{\"mdx\": \"SELECT {[Period].[Q1].Children} ON COLUMNS, {[Account].[Revenue]} ON ROWS FROM [General Ledger]\"}"
```

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
a real server and need no Excel. Two scripts in [adapter](adapter) check the add-in inside
Excel itself.

| Path | What is there |
| --- | --- |
| `crates/epiphany-core` | The engine: model, cells, rules, MDX, transactions. No HTTP, no disk |
| `crates/epiphany-server` | The program: routes, sign-in, files on disk |
| `adapter` | The Excel add-in and its tests |
| `lab` | The page for trying things by hand, and its model |
| `docs/PLAN.md` | How it is built, what was decided and why, and what is not done |
| `docs/BENCHMARKS.md` | Timings, and how to repeat them |
