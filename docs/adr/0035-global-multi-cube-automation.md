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

`ctx.cube(name)` returns a cube-scoped handle with the former write methods
(`ensureElements`/`ensureElement`/`ensureConsolidated`/`addChild`/`writeCells`/
`writeCell`). The cube-agnostic methods (`input`, `param`/`params`, `now`, `log`,
and a new `cubes()` listing the available cube names) stay on `ctx`.
`ctx.cubeName()` is removed.

**Flows also act on dimensions, not only cubes.** Because dimensions are global
(ADR-0024/0031), `ctx.dimension(name)` returns a handle (`ensureElements`/
`ensureElement`/`ensureConsolidated`/`addChild`) that grows the *global* dimension
and fans the additions out to every cube that uses it (the existing
`grow_dimension` path), so a flow can maintain a shared dimension once for all
cubes. A cube handle's element methods still edit a cube's own embedded
(non-registry) dimension; members of a registry-backed dimension are maintained
through `ctx.dimension(name)`, matching the divergence guard (ADR-0024). So a flow
acts on any mix of cubes and dimensions, owned by none.

**Back-compatible default cube.** A flow carries an optional `default_cube`. When
set, the legacy cube-less calls (`ctx.writeCells(arr)`, `ctx.ensureElements(...)`)
target it, so existing flow bodies and the starter templates keep working; when
unset, a cube-less call errors ("name a cube with ctx.cube(...)"). `default_cube`
is a convenience target, not ownership (freely changeable), and a flow may
ignore it and address cubes explicitly.

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

**7. Authorization.** Flow, Schedule, and Connection become server-global object
kinds, gated by a server admin or a matching global grant (consistent with the
global Dimension grant, ADR-0024/0031). A flow run's cell/element writes remain
subject to the *target* cube's element security at apply time, so global authoring
does not bypass per-cube data confinement.

**8. Migration.** On boot, any flows/flow-tests/jobs/connections still found in a
cube model are lifted into the global Automation store (on a name collision the
cube-scoped name is prefixed and a warning logged) and the migrated flow's
`default_cube` is set to its origin cube so its existing body keeps working
unchanged. The bundled demo and the test fixtures are updated to author global,
cube-targeted flows directly.

## Consequences

- Flows/schedules/connections are authored once at the server level and a flow can
  fan out to many cubes, so the ETL/orchestration surface matches how operators
  think about it, not the cube tree.
- Larger blast radius than the connector work: it restructures the model, the flow
  host API, persistence, the scheduler, the REST surface, the web, and migrates
  existing data. Done in gated phases.
- The flow body API changes (`ctx.cube(name).…`); the `default_cube` shim keeps
  existing bodies and templates working, so the break is opt-in.
- Cross-cube atomic commit is best-effort (per-cube transactional, sequential
  across cubes after pre-validation); true multi-cube atomicity is deferred.

## Deferred

Cross-cube atomic commit; per-step target-cube overrides on a schedule (a flow
already chooses its cubes); a flow "dry-run against cube X" preview that resolves
coordinates (today preview only compiles the body).
