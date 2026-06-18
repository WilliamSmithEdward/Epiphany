# ADR-0035: Global, multi-cube flows and schedules

Status: Accepted
Date: 2026-06-17

## Context

Flows, flow-tests, schedules (jobs), and connections have all lived inside each
cube's `Model` (`epiphany-core::Model.flows/flow_tests/jobs/connections`), with
per-cube REST routes (`/cubes/{cube}/flows/...`), jobs that reference flows by
name within their own cube, and a scheduler that dispatches a job against the cube
that stores it. A flow is therefore *owned* by a cube.

That ownership is artificial. The flow *execution* is already cube-agnostic: the
TypeScript body uses a `ctx` whose target cube is injected at run time
(`run_flow(source, cube, ...)`), so the same body could run against any cube. The
coupling is only in storage, routing, job/test name resolution, and scheduler
dispatch.

The decision (confirmed with the user): make flows and schedules **server-global
objects, not owned by any cube**, and let a flow **target multiple cubes in a
single run**. Automation (an ETL/orchestration concern) belongs at the server
level, not nested under one cube.

## Decision

**1. A server-global Automation model.** Flows, flow-tests, jobs (schedules), and
connections move out of the per-cube `Model` into a new server-level
`Automation` model (`epiphany-core`), persisted as its own model-as-code file at
`{data_dir}/automation/automation.model` and loaded at boot, mirroring the
shared-dimension registry (ADR-0024). It is held on `AppState`
(`automation: Arc<Mutex<AutomationStore>>`), separate from the cube `Engine`; the
engine continues to own only cube models. Connections and flow-tests move too
(they are referenced by flows by name, so they cannot stay per-cube once flows are
global).

**2. The flow `ctx` is multi-cube.** The body addresses a cube explicitly:

```ts
const sales = ctx.cube('Sales')
sales.ensureElements('Region', rows.map(r => r.Region))
sales.writeCells(rows.map(r => ({ coord: { Region: r.Region }, value: r.Value })))
ctx.cube('Forecast').writeCells(...)   // a second cube in the same run
```

`ctx.cube(name)` returns a cube-scoped handle exposing a full CRUD surface, every
operation gated at apply time by the running principal's object and element
security (decision 7), so a flow can only touch what its principal could touch by
hand:

- cells: `readCell(coord)` / `writeCell(coord, value)` / `clearCell(coord)` and the
  bulk `writeCells(arr)`. Writes and clears are staged; a clear stages an empty
  write. Reads resolve against a read view captured for the run (below).
- members and hierarchy (append-only, since the store is append-only):
  `ensureElements` / `ensureElement` / `ensureConsolidated` / `addChild` on the
  cube's own embedded dimensions, plus `members(dim)` to read the current set.
- structure: `ctx.createCube(spec)`, `cube.defineDimension(spec)`,
  `cube.defineAttribute(spec)` / `setAttributeValue(...)`, reusing the
  model-editing engine ops (ADR-0021).
- properties and metadata: `cube.property(key)` / `setProperty(key, value)` over a
  fixed allowlist of cube-level fields the model-editing API already exposes
  (description and similar metadata, and the rules text), never arbitrary internal
  state.

The cube-agnostic methods (`input`, `param`/`params`, `now`, `log`, `cubes()`)
stay on `ctx`. `ctx.cubeName()` is removed.

Reads need live state, which the pure runner does not otherwise have. `run_flow`
therefore takes a `FlowReader` (a trait defined in `epiphany-flow`, implemented by
the engine/API) that resolves cell values, member lists, and cube properties
against a single read view captured at run start and masked by the run principal's
element security, so reads are deterministic within a run and never expose masked
cells. Flow tests and preview pass a fixture or empty reader.

**Flows also act on dimensions, not only cubes.** Because dimensions are global
(ADR-0024/0031), `ctx.dimension(name)` returns a handle (`ensureElements`/
`ensureElement`/`ensureConsolidated`/`addChild`) that grows the *global* dimension
and fans the additions out to every cube that uses it (the existing
`grow_dimension` path), so a flow can maintain a shared dimension once for all
cubes. A cube handle's element methods still edit a cube's own embedded
(non-registry) dimension; members of a registry-backed dimension are maintained
through `ctx.dimension(name)`, matching the divergence guard (ADR-0024). So a flow
acts on any mix of cubes and dimensions, owned by none.

**Multiple data sources, UI-driven, global or flow-scoped.** A flow's *inputs*
(unlike its outputs, which are pure code) are configured on the flow in the UI as
a list of named data sources. Each entry is either:

- a reference to a **global connection** (command/HTTP/SQL) picked from the global
  connection store: its name shows read-only (the global name) and is marked
  global; or
- a **flow-scoped connection** the author defines inline on the flow (any
  connection kind), named uniquely within the flow.

Global connection names are unique across the server; flow-scoped names are unique
within the flow and may reuse a global name. In code a global source is read by its
bare name (`ctx.input('sales_db')`) and a flow-scoped source by a `local.` prefix
(`ctx.input('local.daily_csv')`), so the two namespaces never collide.
`ctx.input()` with no name returns the sole source when exactly one is configured
and errors when there are several; `ctx.sources()` lists the addressable names. At
run time the API resolves every configured input to rows (fetching each connection
under the existing connector gates, or applying a manual run's ad-hoc inline rows
for a named source) and passes the name->rows map to the runner. So one flow can
join a CSV and a SQL query, fan the result across several cubes, and grow a shared
dimension, in one run. A flow-scoped connection obeys the same connector controls
as a global one (command opt-in, HTTP/SQL build feature plus enable flag plus host
allowlist, secrets referenced by name from the global secret store).

**Back-compatible default cube.** A flow carries an optional `default_cube`. When
set, the legacy cube-less calls (`ctx.writeCells(arr)`, `ctx.ensureElements(...)`)
target it, so existing flow bodies and the starter templates keep working; when
unset, a cube-less call errors ("name a cube with ctx.cube(...)"). `default_cube`
is a convenience target, not ownership (freely changeable), and a flow may
ignore it and address cubes explicitly. It is a migration and back-compatibility
shim set automatically for lifted per-cube flows (decision 8); the authoring UI
offers no output-cube picker, since outputs are named in code.

**3. Multi-target outcome; per-target validate-then-apply.** `run_flow` drops its
`cube` parameter and returns a `FlowOutcome` keyed by target: a
`BTreeMap<String, CubeChanges>` of cube writes (staged elements + edges + numeric/
string cells) plus a `BTreeMap<String, DimChanges>` of global-dimension growth
(elements + edges), with the cube-agnostic logs/counts. The pure runner only
stages changes. The API pre-validates each cube's changes against a clone of that
cube and each dimension's growth against the registry, then applies cube writes
per cube and dimension growth via `grow_dimension` (which fans out). Each cube's
write and each dimension grow is transactional; across targets it is sequential
after a full pre-validation pass, so a partial apply can arise only from an
unexpected mid-apply failure (documented; cross-target atomic commit is a future
item). Element security still applies at write time per the *target* cube's
element ACLs.

**4. Schedules are global.** A job's steps reference global flows by name; the
scheduler iterates the global job set (no per-cube dispatch). A job no longer has
a cube; the cubes written are whatever its flows' bodies address. The run ledger's
per-run `cube` becomes optional (a global flow run is labelled by flow, not cube).

**5. Connections are global**, referenced by global flows by name and resolved
from the global store. The existing connector controls are unchanged (command
opt-in, HTTP/SQL build feature + enable flag + host allowlist, secret store by
name); they simply move to the server level.

**6. REST moves to the server level.** `/api/v1/flows`, `/flows/{name}`,
`/flows/{name}/run`, `/flows/tests`, `/schedules`, `/connections` (no `/cubes/`
prefix). The per-cube routes are removed rather than aliased: Epiphany is
pre-1.0, and a global flow run writes to the cubes its body names, so a per-cube
flow route no longer has a meaning.

**7. Authorization, run-as principal, fail-closed.** Flow, Schedule, and
Connection become server-global object kinds; authoring them (create, edit, delete)
is gated by a server admin or a matching global grant (consistent with the global
Dimension grant, ADR-0024/0031). Authoring a flow does not grant the data access
the flow exercises: a flow run carries a *principal*, and every host operation
(read, cell write, member add, structural edit, property change) is gated by that
principal's object and element security exactly as the same action would be from
the API or UI, fail-closed (secure by default). A manual run executes as the
calling user. A scheduled run has no caller, so it executes as the flow's recorded
**owner** (the principal who created it, carried on the flow and re-stamped only by
an explicit owner change); this is auditable and bounds an unattended flow to a
real principal's rights rather than running privileged. Reads are masked and writes
are validated per target cube and dimension at apply time, so global authoring
never bypasses per-target data confinement.

**8. Migration.** On boot, any flows/flow-tests/jobs/connections still found in a
cube model are lifted into the global Automation store (on a name collision the
cube-scoped name is prefixed and a warning logged) and the migrated flow's
`default_cube` is set to its origin cube so its existing body keeps working
unchanged. A migrated flow's `owner` (decision 7) is set to the first server admin
if the legacy flow recorded none, so scheduled runs of lifted flows have a real
run-as principal. The bundled demo and the test fixtures are updated to author
global, cube-targeted flows directly.

## Consequences

- Flows/schedules/connections are authored once at the server level and a flow can
  fan out to many cubes, so the ETL/orchestration surface matches how operators
  think about it, not the cube tree.
- Larger blast radius than the connector work: it restructures the model, the flow
  host API, persistence, the scheduler, the REST surface, the web, and migrates
  existing data. Done in gated phases.
- The flow body API changes (`ctx.cube(name).…`); the `default_cube` shim keeps
  existing bodies and templates working, so the break is opt-in.
- The flow becomes a general programmable surface over the model (cells read/write/
  clear, member growth, structural creation, allowlisted properties), so reads make
  the runner depend on a captured, principal-masked read view rather than only its
  injected inputs. Determinism is preserved (the view is fixed at run start); flow
  tests that read live state must pin a fixture reader.
- Cross-cube atomic commit is best-effort (per-cube transactional, sequential
  across cubes after pre-validation); true multi-cube atomicity is deferred.

## Deferred

Cross-cube atomic commit; per-step target-cube overrides on a schedule (a flow
already chooses its cubes); a flow "dry-run against cube X" preview that resolves
coordinates (today preview only compiles the body).
