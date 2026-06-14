# ADR-0012: Data-source connectors (HTTP, ODBC, command) and the fetch/transform split

- **Status:** Proposed
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 5 (flows), connector follow-on

## Context

M5 flows read CSV (and source-less). Real ETL needs to ingest from live systems:
HTTP/REST APIs and databases (generic ODBC). This cuts against three things M5
deliberately established: flows are **deterministic** (ADR-0009), **sandboxed**
(no filesystem or network reach from the script), and ship in a **single
static binary** with tight dependency discipline. A naive `ctx.fetch(url)` inside
the flow's JavaScript would break all three at once.

The enabling fact is that M5 already drew the right boundary: `run_flow` is pure
(`rows, params, clock -> FlowOutcome`) and the flow's JavaScript only ever sees
`ctx.input()`. So a connector can be a layer *above* the engine that produces
those same rows, without changing the flow language, sandbox, or determinism
model.

## Decision

**1. Separate the impure fetch (at the edge) from the pure transform.** A
connector fetches rows in Rust, before the flow runs, and materializes them into
the same `Vec<Row>` that `ctx.input()` already consumes. The flow's JavaScript
never performs I/O. There is deliberately **no `ctx.fetch`**: exposing network or
filesystem access to the script would reopen the sandbox and make the transform
nondeterministic and non-unit-testable. Multi-source flows are served by named
inputs (`ctx.input('sales')`), still fetched upfront in Rust.

**2. Connectors live in a new `epiphany-connect` crate (async, feature-gated).**
All I/O lives here; `epiphany-flow` stays pure and depends on `epiphany-core`
only. `epiphany-connect` resolves a `DataSource` to rows. The API layer fetches
(async, on the request) and hands the rows to `run_flow`. Connectors are behind
cargo features so the default build stays lean and pure:
- `http-connector` (reqwest): GET/POST a URL, parse a JSON response (a configured
  record path) or a CSV response into rows.
- `odbc` (odbc-api): connect via the system ODBC driver manager and run a SQL
  query, mapping result rows to `Row`s. This is generic across any ODBC database
  at the cost of a system driver dependency and FFI (the unsafe is internal to
  `odbc-api`; our usage is its safe API). It is off by default and documented as
  *not* part of the pure single-binary profile.
- `command` (std only, no extra dependency): run an external program and read
  its stdout, parsed as CSV or JSON. This is the single connector behind
  "ingest from a Python script", "from a PowerShell script", and "from an exe":
  they differ only in the configured `program` and `args` (`python script.py`,
  `pwsh -File load.ps1`, `./extract.exe`). Its security model is decision 6.

**3. Connections are admin-defined model objects that reference secrets, never
embed them.** A `Connection` (name, kind, endpoint/DSN, options, and a *reference*
to a credential) serializes as model-as-code. Secret *values* live in a separate
secret store (environment-backed initially, a sealed file later), like the
security store, and are never written to Git-tracked model text (the no-secrets
rule). Flows and the import wizard reference a connection **by name**.

**4. Capability gating and SSRF defense are operator-controlled.** Because a
server-side fetch to an arbitrary URL or DSN is an SSRF risk, connections are
configured by an admin with an allowlist of permitted endpoints/DSNs; modelers
pick from named connections rather than typing raw URLs. This also fits the
north-star: business users never see connectors, modelers pick a configured
source, admins define the connections.

**5. Determinism is preserved because external data is an input.** A flow unit
test pins inline rows (the existing `FlowTest.input`) and never touches a live
connector, so tests stay offline and reproducible. A live run fetches a snapshot
of external state "as of now" and the transform is deterministic given those
rows. This is the same semantics as reading a file: we claim the transform is
deterministic, not that a live pull is reproducible across time.

**6. The command connector is arbitrary code execution by design, so it is
fenced by four independent controls.** Running a Python/PowerShell script or an
executable is exactly running host code; treated casually it is the worst kind
of footgun (RCE for anyone who can author a flow). The controls:

1. **Admin-defined, fixed commands only.** The `program` and `args` are set when
   an admin creates the connection. A flow author references the connection *by
   name* and supplies no part of the command line, so a modeler can never make
   the server run arbitrary code, only a pre-approved command.
2. **No shell.** The program is spawned directly with an argv array (never via
   `sh -c` / `cmd /C` over a string), so there is no shell-injection or glob/
   variable-expansion surface.
3. **Off unless explicitly enabled.** The server runs command connections only
   when started with an explicit opt-in (`EPIPHANY_ENABLE_COMMAND_CONNECTORS`);
   otherwise defining or running one is rejected. So enabling host execution is
   a deliberate operator act, independent of who can define connections.
4. **Resource-bounded.** A timeout kills an overrunning process, stdout is capped
   to a maximum byte size (no OOM), and a non-zero exit fails the run with the
   captured stderr.

Parameterized command arguments (a flow passing, say, a date range into a fixed
slot) are deliberately deferred until the controls above are proven; the first
cut runs fully-fixed commands.

## Alternatives considered

- **`ctx.fetch` / DB access inside the flow's JS:** rejected. It breaks the
  sandbox, determinism, and offline unit-testing in one move.
- **Native Rust DB drivers (Postgres/MySQL/SQLite) instead of generic ODBC:**
  considered; cleaner for the single-binary goal (mostly pure Rust, no system
  driver manager). The maintainers chose **generic ODBC** so one connector reaches
  any ODBC-capable database, accepting the system-driver and FFI dependency
  behind an off-by-default feature. Native drivers remain a possible additional
  feature later.
- **Secrets in model-as-code:** rejected (no-secrets rule); connections carry
  secret references only.
- **Always-compiled connectors:** rejected; feature-gating keeps the default
  binary lean and pure, and lets a deployment opt into only what it needs.
- **Reverse ETL (writing out to connectors):** out of scope; this ADR covers
  inbound ingestion only.
- **A free-form `ctx.exec(cmd)` or flow-supplied command arguments:** rejected.
  It would hand arbitrary code execution to anyone who can write a flow. The
  command, like every connection, is an admin artifact; flows only reference it.

## Consequences

- The flow engine, sandbox, and determinism model are unchanged; connectors are
  additive. The flows web workspace gains a source picker; flow tests stay
  offline.
- New dependencies are feature-gated: `reqwest` (with rustls) under
  `http-connector`, `odbc-api` under `odbc`. `cargo-deny` must be re-checked for
  each; the `odbc` profile is documented as requiring a system ODBC driver
  manager and is not part of the pure single-binary build. The `command`
  connector adds no dependency (std `process`), but its capability is gated at
  runtime (decision 6) rather than only at compile time.
- The command connector is implemented first (it answers the immediate
  "ingest from a script/exe" need and needs no third-party crate); HTTP then
  ODBC follow on the same foundation.
- A small connections + secret-reference subsystem is added (admin CRUD, an
  env-backed secret store, an endpoint/DSN allowlist). Scheduled refresh of
  connector-backed flows pairs naturally with the Phase 8 job scheduler.
- To be realized as `epiphany-connect` plus the connection/secret model and the
  flow-run source wiring; this ADR moves to Accepted when that lands, gated by a
  connector acceptance test (a mock HTTP source end to end).
