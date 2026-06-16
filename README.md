# Epiphany

A self-hostable, in-memory **multidimensional OLAP server** for planning and
analytics: cubes and hierarchies, a rules-and-feeds calculation engine, MDX,
TypeScript "flows" for ETL and automation, what-if sandboxes, object and element
security, scheduled jobs, and a fast pivot grid with write-back. It ships as a
single static binary with a clean JSON REST API and a React + TypeScript web UI.

> **Status: feature-complete across the nine-phase roadmap (0-8).** Latest
> release is `m8.7`; hardening and performance work continue as point releases.
> See the [roadmap](docs/ROADMAP.md), the [changelog](CHANGELOG.md), and the
> [releases](https://github.com/WilliamSmithEdward/Epiphany/releases).

## Core features

**Modeling**
- Cubes over dimensions with numeric, consolidated, and string elements,
  **alternate rollups** and **weighted consolidations**, plus attributes and
  aliases.
- **Build the model from the UI** (or REST): create a cube, add members, build
  consolidation hierarchies, and define attributes from the web client's Data
  Model workspace, no model-file editing required (ADR-0021).
- **Model-as-code**: the entire model is canonical, human-readable text and is
  the Git source of truth; the binary snapshot is only a runtime cache.
- Sparse, packed-key cell storage tuned for memory (about 17 bytes per numeric
  leaf cell, see [Performance](#performance)).

**Calculation**
- A rules engine with compiled, on-demand evaluation and per-query memoization.
- **Automatic feeder inference** and under/over-feed validation, so sparse
  consolidation stays correct without hand-maintained feeders; reads always use
  the dense, always-correct path so a feeder mistake can never silently change a
  total.
- **Calculation provenance ("explain")**: ask why a cell holds the value it does
  and get the stored values, rules, and consolidation path behind it.

**Query and entry**
- MDX-based subsets, views, and cellsets with crossjoin nesting and
  zero-suppression.
- A high-performance **pivot grid with write-back** and transactional batch
  writes.
- A **persistent view cache** and **deterministic parallel aggregation**: repeat
  reads of a view are served instantly from a version-keyed cache that never
  returns a stale or cross-user result, and large cold views aggregate across all
  cores, several times faster, with bit-identical results (ADR-0028).
- Live change notifications over WebSocket.

**Automation and integration**
- **Flows**: TypeScript ETL and automation on an embedded JavaScript engine,
  where scripts orchestrate and native Rust does the bulk work. Includes a CSV
  import wizard and **data-source connectors** (run an admin-defined program and
  read its CSV/JSON output, e.g. a Python or PowerShell script for a database
  pull).
- **Scheduled jobs**: ordered flow sequences on an in-process scheduler with a
  durable run ledger and convergent crash recovery.
- **Excel add-in** (a single Excel-DNA `.xll`): pull live cube values with
  `=EPIPHANY.READ(...)`, sign in through an embedded WebView2 screen that reuses
  the server login (token stored encrypted, never in the workbook), and write an
  edited table back in one transaction (ADR-0022, see
  [`excel-addin/`](excel-addin/README.md)).

**What-if and collaboration**
- Per-user, per-cube **sandboxes**: copy-on-write overlays where rules and
  consolidations recompute over your proposed changes, then commit or discard.

**Security and operations**
- Users and groups, **object and element security** (four-level lattice), and
  **global cube grants with explicit deny** for broad baselines with per-cube
  exceptions. Secure by default: an ungranted cube is closed unless granted.
- An append-only **audit log**, retention and rotation, login lockout against
  brute-forcing, and owner-only on-disk secret files.
- Durable persistence: write-ahead log plus snapshots with crash recovery, and
  MVCC snapshot isolation for consistent concurrent reads.

**Quality as a feature**
- A built-in **model testing framework**: rule and flow unit tests stored as
  model-as-code and run deterministically, so you can prove a model is correct.
- A server-wide **deterministic mode** (injected clock, seeded IDs, ordered
  iteration) makes the whole system reproducible and directly testable.

## Performance

Performance and memory efficiency are binding requirements here, not
afterthoughts, and every budget is gated in CI so a regression fails the build.
Epiphany is built to stay fast on large models: sparse packed-key storage keeps
memory near the theoretical floor, consolidations run on a dense always-correct
path, repeated reads are served from a coherent cache, and large cold reads
aggregate in parallel across every core. Numbers below are release-mode
measurements on a development machine and are indicative; full methodology is in
[docs/PERFORMANCE.md](docs/PERFORMANCE.md).

**Highlights**

- **~17 bytes per numeric leaf cell** (CI-asserted; budget 24) and
  **~13M cells/sec/core** bulk load, an order of magnitude over budget.
- **Sub-microsecond point reads** (~37 ns).
- **Cold consolidation ~100x under the 1 s latency budget**, and large cold views
  aggregate **4.5x to 6.9x faster in parallel** across cores (a 40k-cell
  all-consolidated crossjoin drops from ~454 ms to ~72 ms on 14 cores) with
  results provably **bit-identical** to the serial path.
- **Repeat reads are effectively free**: a version-keyed view cache serves an
  unchanged view in well under the 100 ms cached-read budget, and never returns a
  stale or cross-user result.

| Measurement | Observed | Budget |
|---|---|---|
| Memory per numeric leaf cell | about 17 bytes/cell | <= 24 bytes (CI-enforced) |
| Bulk-load throughput | about 13M cells/sec/core | about 1M/sec/core |
| Point read (`get_leaf`) | about 37 ns/op | sub-microsecond |
| Cold consolidated read (scans 100k cells) | about 10 ms/call | p99 under about 1 s |
| Large cold view, parallel aggregation (14 cores) | about 72 ms (40k-cell crossjoin, 6.3x vs serial) | p99 under about 1 s |
| Repeat view read (cached) | a refcount bump plus the response transform | p99 under about 100 ms |
| Scheduler reconcile due-scan | 2000 jobs vs a 1000-run ledger in about 11 ms/tick | cheap vs the tick period |

Bulk-load clears its budget by an order of magnitude, cold consolidation runs
roughly 100x under the latency budget, and bytes-per-cell is within budget and
asserted in CI so a regression fails the build. The view cache is keyed
losslessly on the cube version, the view shape, the active sandbox, and the
caller's exact element-deny set, so it accelerates the common case without ever
crossing a security boundary; parallel aggregation is gated above a cell-count
threshold so small reads stay serial. Benchmarks are self-contained (no external
framework): run `cargo bench -p epiphany-core` (cube ops and `view_exec`) and
`cargo bench -p epiphany-flow`.

## Quickstart

### Run a prebuilt binary (fastest)

Download the binary for your platform from the
[latest release](https://github.com/WilliamSmithEdward/Epiphany/releases/latest)
(Linux x86_64 and aarch64, Windows x86_64, macOS aarch64), then run it:

```sh
./epiphany-server-linux-x86_64
```

On first run it creates an `admin` user and writes the generated password to an
owner-only file under the data directory (the log shows the path); read it once
and delete it. Then open the web UI at http://127.0.0.1:8080/.

### Run from source

Prerequisites: Rust (stable) and Node (the version in
[`.node-version`](.node-version)).

```sh
# API + engine only
cargo run -p epiphany-server

# Single binary with the web UI bundled in
cargo run -p epiphany-server --features embed-ui
```

### Web client (development)

```sh
cd web
npm ci
npm run dev
```

### Configuration

Configuration is zero-config by default and overridden by `EPIPHANY_*`
environment variables. The most useful:

| Variable | Purpose | Default |
|---|---|---|
| `EPIPHANY_BIND` | Listen address | `127.0.0.1:8080` (loopback) |
| `EPIPHANY_DATA_DIR` | Durable data directory | `./data` |
| `EPIPHANY_TLS` | `on` serves HTTPS with an auto-generated self-signed certificate | off (HTTP) |
| `EPIPHANY_TLS_CERT` / `EPIPHANY_TLS_KEY` | PEM cert + key to serve a real certificate (takes precedence) | none |
| `EPIPHANY_ENABLE_COMMAND_CONNECTORS` | Allow admin-defined programs as flow data sources | off |

### HTTPS / TLS

HTTPS is optional and off by default. The easiest way to turn it on is one
variable:

```sh
EPIPHANY_TLS=on ./epiphany-server-linux-x86_64   # serves https://127.0.0.1:8080/
```

That generates a self-signed certificate into the data directory on first run
(browsers will show a self-signed warning, which is expected for local and
internal use). For a real certificate, point at your PEM files instead:

```sh
EPIPHANY_TLS_CERT=/path/cert.pem EPIPHANY_TLS_KEY=/path/key.pem ./epiphany-server-...
```

TLS serves on the same `EPIPHANY_BIND` address. The prebuilt release binaries
include TLS; a from-source build needs `--features tls` (and the platform's C
toolchain, since the crypto is compiled).

See [AGENTS.md](AGENTS.md) for the full configuration surface and the supported
platforms.

## Architecture

A Rust Cargo workspace of focused crates plus a React/Vite web client:

- `epiphany-core` (model, sparse storage, consolidation),
  `epiphany-calc` (rules, feeders, provenance), `epiphany-mdx` (MDX),
  `epiphany-flow` (flows, scheduler, run ledger),
  `epiphany-connect` (data-source connectors), `epiphany-security`
  (auth, ACLs, audit), `epiphany-persist` (WAL and snapshots),
  `epiphany-engine` (MVCC concurrency), `epiphany-api` (REST),
  `epiphany-server` (the binary), and `epiphany-determinism` (the test seam).
- Layering is strict: the engine and calculation core carry no security or I/O
  dependencies; cross-cutting concerns reach them through injected seams. This is
  what keeps the system deterministic and directly testable at every layer.
- Clients are thin and live alongside the workspace: the React + TypeScript web
  client in [`web/`](web/) and the Excel-DNA add-in in
  [`excel-addin/`](excel-addin/); both call the REST API and hold no model logic.

## Documentation

- Architecture, conventions, and commands: [AGENTS.md](AGENTS.md)
- Plan of record and scope: [docs/ROADMAP.md](docs/ROADMAP.md)
- Performance budgets and methodology: [docs/PERFORMANCE.md](docs/PERFORMANCE.md)
- Running as a service (systemd, Docker, launchd, Windows): [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)
- Architecture decision records: [docs/adr/](docs/adr/)
- Engineering practices: [docs/agentic_ai_programming_best_practices.md](docs/agentic_ai_programming_best_practices.md)
- API reference: the server serves its OpenAPI document at
  `/api/v1/openapi.json`.

## License

Licensed under the [MIT License](LICENSE). Third-party dependencies are
restricted to permissive licenses (MIT, Apache-2.0, BSD, ISC, Unicode-3.0,
Zlib), enforced by `cargo deny` in CI.
