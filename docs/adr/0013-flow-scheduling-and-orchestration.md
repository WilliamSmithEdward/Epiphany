# ADR-0013: Flow scheduling and orchestration

- **Status:** Proposed
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 8 (scheduling and hardening)

## Context

The user-facing goal for Phase 8 is the ability to "schedule refreshes": run
flows on a schedule without a human poking the API. ROADMAP section 4.G and the
Phase 8 deliverable define a scheduled job precisely as "ordered sequences of
flows run on a schedule," executed by a server-side scheduler that lives in
`epiphany-flow` and is wired in `epiphany-server`. The Phase 8 done-when for this
piece is one line: "a job runs on schedule." Phase 5 (flows) is the only
prerequisite and is already delivered, and [ADR-0012](0012-data-source-connectors.md)
notes that scheduled refresh of connector-backed flows pairs naturally with this
scheduler. No scheduler ADR exists yet.

The question this ADR answers is sharper than "add cron": are async scheduling,
orchestration, and async status polling planned at all, and how can any of them
coexist with the determinism mandate? Three forces collide.

- **A scheduler is intrinsically time-driven, but logic must never read the wall
  clock.** [ADR-0009](0009-determinism-strategy.md) requires that logic take time,
  RNG, and ids as injected dependencies and never call `SystemTime::now`, an
  ambient RNG, or mint ids inline. The Phase 8 DoD must be provable under a pinned
  `ManualClock` with zero real timers. A naive scheduler that computed "now" deep
  in flow execution, or computed a next-fire from `SystemTime`, would make every
  scheduled run non-reproducible.
- **A flow run today is fully synchronous inside one HTTP request.** The current
  `run_flow_handler` resolves the source, fetches rows, calls
  `run.rs:run_flow(source, cube, rows, params, now_millis)`, applies the staged
  `FlowOutcome` through the engine, broadcasts a change event, and returns a
  `RunReport`, all on the request task. There is no run id, no run registry, and
  no run state on `AppState`. A scheduler has no HTTP request to ride on, so runs
  must become first-class, addressable, and pollable.
- **State must be durable in a single binary, with no new system-of-record.**
  The store is single-process (no OS file lock, no cross-process WAL coordination;
  see `epiphany-persist/src/store.rs`), writers serialize per cube over an
  `arc_swap::ArcSwap` snapshot ([ADR-0001](0001-concurrency-model.md)), and the
  WAL is a fast-restart cache that must not hold job history
  ([ADR-0010](0010-audit-logging.md)). A scheduler that survives restart needs
  durable schedule and run state that respects all three.

The roadmap also draws a hard scope line. Event and webhook triggers, gRPC, an
operations console, approval workflows, and cross-process or multi-node
scheduling are deferred or cut in ROADMAP section 13; RG-16 requires that any
reintroduction get its own ADR and phase plan, not a drive-by. So the decision
is as much about what the scheduler is *not* as what it is.

## Decision

The scheduler is a single in-process **declarative reconcile loop** in
`epiphany-flow`, wired in `epiphany-server`. It treats jobs and triggers as
declared desired state, wakes on the injected clock, computes which firings are
due, and drives idempotent runs toward that state. Real time enters at exactly
one boundary and is then carried down as recorded data. We adopt the declarative
base because a convergent (not edge-triggered) loop recovers missed and
interrupted firings on restart from durable state, which is the property a
reliable refresh feature actually needs.

**0. The normative determinism rule (load-bearing, read first).** Real
wall-clock time enters the system at exactly one point: the reconcile loop's
wake. In production the loop holds a `SystemClock`; in tests it holds a
`ManualClock`. On each tick the loop reads `clock.now_millis()` from the injected
`&dyn Clock` *once* into a local `tick_now: u64`. When a firing is selected,
`tick_now` (never a fresh clock read) is frozen into the run record as
`fire_millis` and passed straight into the existing
`run_flow(..., now_millis = fire_millis)` parameter. No downstream code (flow
JavaScript via `ctx.now()`, retry logic, audit timestamps) ever re-reads a clock;
they all read the recorded `fire_millis`. Replaying the same declared triggers,
the same last-fired ledger, and the same `tick_now` under `ManualClock`
reproduces identical run ids, fire-times, cells, and audit timestamps. This is
the whole determinism reconciliation: time is captured at the boundary, recorded,
then injected downward as a value.

**1. The reconcile loop reads the clock; the run it spawns does not.** In
production an OS timer (`tokio::time::interval`) is only a wakeup nudge; it never
supplies a time value to logic. On each wake the loop computes, as a pure
function of `(declared triggers, last_fired ledger, tick_now)`, the set of due
firings: for each `JobSpec` it evaluates `next_due(last_fired, trigger)` and
selects firings where `next_due <= tick_now`. Pending firings are held in a
structure keyed by `(next_due, job_id)` so the due-scan and dispatch order are
stable and observable-deterministic per ADR-0009 section 2, never a `HashMap`
iteration, and so the common case is a cheap `next_due <= now` scan rather than a
re-evaluation of every trigger each tick. Under `ManualClock`, `advance()` past a
due time fires the tick deterministically and the DoD "a job runs on schedule"
is provable with no real timers. Job and run ids come from the injected `IdGen`;
any backoff jitter comes from `DeterministicRng` seeded per run, never an ambient
`rand`.

**2. Jobs are declared model-as-code; the run ledger is durable, framed primary
state.** Two state classes, two homes.

- **Desired state** is a `JobSpec { name, cube, steps: Vec<FlowRef>, trigger,
  enabled }`, persisted like flows via the store's existing
  define/checkpoint path (an immediate snapshot rewrite, the same path
  `define_flow`/`define_connection` already use). A job is a first-class secured
  object (Phase 7) and is reconstructible from the model text, so it survives
  restart in the snapshot.
- **Run and fire state** is a `RunRecord { id, job, fire_millis, state, report,
  error, principal }` plus a per-job `last_fired_millis`. This is durable primary
  data, but it is deliberately **not** the WAL (ADR-0010 forbids job history in
  the WAL) and **not** the audit stream. It is a small append-only **run ledger**
  that reuses the WAL's exact framing primitives (`[len u32][payload][crc u32]`
  behind a magic and version header, replay-to-`good_len` torn-tail truncation),
  fsync'd by the default sync policy and recovered on `Store::open`. Reusing the
  proven framing rather than inventing a new on-disk format is what makes the
  ledger as crash-safe as the WAL; a torn ledger tail is truncated, never
  silently re-firing or dropping a job.

**3. The run id is a deterministic function of the firing, so re-fires dedupe
for free.** A scheduled run's id is derived from `(job, fire_millis)` via
`IdGen`. A firing that is re-derived after a restart, or retried, reuses the same
id; the ledger dedupes it. This upgrades the model from at-most-once to
at-least-once *delivery* with at-most-once *commit*, without importing a
lease/visibility-timeout worker queue. An `Idempotency-Key` field is reserved on
the submit path (defaulting to the derived key) so the same dedupe extends to
manual and API-submitted runs, and so a future at-least-once delivery mode for
connector refreshes is a non-breaking add.

**4. Async API surface: submit returns an id, clients poll.** Submission never
blocks the request. The synchronous `POST /cubes/{cube}/flows/{name}/run` path
that exists today stays for small inline imports (back-compatible). The async
surface is:

- `POST /cubes/{cube}/flows/{name}/run` (and a new `POST /cubes/{cube}/jobs/{name}/run`
  for a manual job kick) allocates a `RunId`, records `Queued`, spawns the
  existing resolve, fetch, `run_flow`, apply-outcome chain on a worker, and
  returns `202 Accepted` with `{ run_id }` and a `Location` header. The connector
  fetch and the CPU-bound boa execution run under `spawn_blocking` (boa relies on
  a per-run thread-local and a synchronous connector poll, so it must stay on one
  blocking thread), and the worker pool is bounded to cap concurrent runs.
- `GET /cubes/{cube}/runs/{id}` returns the `RunRecord` view; on a terminal state
  it returns the same `RunReport` shape (`rows_read`, `cells_written`,
  `elements_added`, `logs`) returned synchronously today.
- `GET /cubes/{cube}/runs` and `GET /cubes/{cube}/jobs/{name}/runs` list recent
  runs.

Run-state lifecycle and transitions:

- `Queued -> Running` when a worker picks up the run.
- `Running -> Succeeded` when the outcome commits.
- `Running -> Failed` on a flow, strip, input, or connector error (terminal,
  carrying the error for the poll response).
- `Running -> Skipped` is not used; instead a trigger that fires while a prior
  run of the *same* job is still `Running` is coalesced to `Skipped` at selection
  time (single-flight per job; see decision 6).
- `Running -> Failed{interrupted}` on restart for any run that was in flight when
  the process crashed; the convergent loop then re-derives that firing as due
  (decision 5).

**5. Crash recovery is convergent, not edge-triggered, with bounded catch-up.**
On `Store::open`, after snapshot and WAL replay, the loop reloads each job's
`last_fired_millis` from the recovered ledger and resumes. Because firing is a
pure function of `(trigger, last_fired, tick_now)`, the loop never relies on
having been awake at the instant a fire was due: a fire missed during downtime is
re-derived as due on the next tick. Missed fires use a bounded catch-up policy,
`catchup: skip | one` (default `one`): at most one coalesced run per trigger on
restart, never a thundering-herd backfill of every fire that elapsed while the
server was down. An in-flight run interrupted by a crash is recovered as
`Failed{interrupted}` and the next reconcile re-derives it as due; this is safe
because of decision 7. Single-process and in-process write serialization are
respected: one loop in the one process that owns the data directory, and runs
write through the same per-cube writer lock as the synchronous path today.

**6. Concurrency: parallel across cubes, serialized within a cube, never
lock-across-a-run.** Jobs writing different cubes run concurrently; jobs writing
the same cube serialize behind that cube's writer lock (ADR-0001). A multi-step
run never holds the writer lock across the whole run: each step stages its
`FlowOutcome` against the current snapshot and publishes one new version under
the lock, so reads stay live and other writers are not starved for the duration
of a flow. A batch staged on a base that has since moved is rejected and the step
re-stages against the fresh snapshot. Single-flight per job (decision 4) is the
default overlap policy: a trigger that fires while the prior run of the same job
is still active is coalesced to a logged-and-audited `Skipped`, so a slow job
cannot pile up duplicate runs.

**7. Retry and idempotency; a half-applied run is impossible by construction.**
A flow's `FlowOutcome` is pre-validated against a *cube clone*
(`Cube::extend_schema` plus `build_write` for every cell) before anything
commits, and the commit itself is the engine's all-or-nothing batch
(`BatchBegin..BatchEnd`, discarded whole on a torn tail). So a run either fully
commits or commits nothing; a crash mid-apply leaves no residue to reconcile, and
the firing simply re-derives as due. Elements and edges are ensured idempotently
(append-only), and a `PlannedCell` sets an exact value, so re-running a firing
that already committed is safe. Failed runs retry with deterministic exponential
backoff (jitter from `DeterministicRng`, seeded per run) up to a per-job
`max_retries` (default 0, a named field so bounded retry is a non-breaking
extension); backoff delay is millis added to `next_due`, keeping retry timing a
pure function of recorded inputs.

**8. Orchestration is ordered chaining, not a DAG, and there are no in-engine
wait primitives.** A job is the roadmap's exact definition: an ordered sequence
of flow steps, run sequentially, fail-fast, each step its own validated
all-or-nothing commit. There is deliberately no cross-job DAG, no fan-out or
fan-in, and no declarative graph. There are no in-engine async wait or
poll-until primitives. Any "wait for an external system" stays inside the user's
connector script, where ADR-0012's `run_command` already polls synchronously
(`try_wait` plus `sleep`) under the run's `spawn_blocking` thread. This keeps
Epiphany out of the workflow-engine business that ROADMAP section 13 defers, and
keeps the recorded-fire-time invariant trivially intact (no step ever needs to
read a clock to decide when to proceed). Chaining is kept forward-compatible with
a future graph model so a later DAG increment, under its own ADR, is additive.

**9. Audit covers every firing and run (ADR-0010).** Every trigger firing and
every run, scheduled or API-submitted, emits a record to the separate
`epiphany-security` append-only audit stream, never the WAL or the run ledger:
actor (the scheduler's service principal for timer-fired runs, or the calling
user for API runs), action (`job.fire`, `flow.run`), target by object identity
(`job:{name}` / the flow), outcome, the injected `fire_millis` as the timestamp,
and the monotonic audit sequence number. Job create, update, and delete audit as
object CRUD, and the explicit full-persist command is audited. Connections are
referenced by identity only; no secrets, tokens, or PII cross the record boundary
(RG-13). A coalesced `Skipped` firing is audited so single-flight coalescing is
observable, not silent. Audit writes never gate startup, and audit timestamps are
deterministic in tests because they are the recorded `fire_millis`. Audit is at
job and flow granularity, not per cell.

## Alternatives considered

- **P4, declarative reconcile loop (adopted as the base).** Jobs and triggers are
  declared desired state; a convergent loop computes due firings from a durable
  `last_fired` ledger and drives idempotent runs. Pros: recovers missed and
  interrupted firings on restart from recovered state rather than relying on
  having been awake, which is exactly the reliability property a refresh feature
  needs; a deterministic run id from `(job, fire_millis)` makes dedupe a pure
  function of recorded inputs; it stays inside the roadmap's ordered-sequence
  scope. Con, and its sharpest edge: the run ledger is net-new primary state that
  is neither WAL nor audit, so its framing and recovery must be as carefully
  crash-tested as the WAL. We mitigate by reusing the WAL's exact framing and
  torn-tail truncation (decision 2) rather than inventing a format.
- **P1, minimal single timer plus in-memory run store.** A single tick loop fires
  existing flow runs and an `Arc<Mutex<RunStore>>` (following the `sessions`
  precedent) holds run status. Pros: the least net-new state, zero new on-disk
  structures, the most literally roadmap-faithful, and it nails the determinism
  boundary. Con: run status is lost on restart and the at-most-once, no-catch-up
  default silently drops a refresh whose window straddled a crash. We graft its
  best parts: the OS-timer-as-wakeup-only pattern (decision 1), single-flight per
  job (decisions 4 and 6), and the named-deferred `max_retries` field (decision
  7), while taking P4's durable ledger to close the recovery gap.
- **P2, durable embedded job queue with a worker pool.** Two new WAL op-kinds, a
  lease and visibility-timeout queue, at-least-once delivery, idempotency keys,
  and retry with backoff. Pros: the most production-robust recovery (a dead
  worker's lease expires and the run is re-leased) and the cleanest reuse of CRC
  WAL framing. Cons: it puts queue operational state into the WAL, which expands
  the crash-recovery surface of the durability cache and sits in tension with
  ADR-0010's "the WAL is a cache, not history" rule, and a worker pool plus lease
  machinery overshoots the Phase 8 DoD. We graft its WAL-framing discipline for
  the ledger and its `Idempotency-Key` plus `Location`-header API hygiene
  (decisions 2 and 3), and keep the lease queue as a reference for a future
  at-least-once increment.
- **P3, declarative flow DAG with first-class async wait primitives.** Jobs are
  directed acyclic graphs with fan-out and fan-in, per-step retry, and in-engine
  `wait_duration`, `poll_until`, and `wait_external` primitives, with five new WAL
  op-codes and full in-flight DAG crash recovery. Pros: the richest model and the
  most honest about the long-term shape, including lifting long durable waits out
  of a pinned `spawn_blocking` thread. Con, conceded by the proposal itself: a
  DAG with async wait primitives is squarely the orchestration-engine territory
  ROADMAP section 13 defers, so it needs its own ADR and phase plan. We defer it
  in full and keep chaining forward-compatible with a graph model (decision 8),
  and carry forward its bounded catch-up policy (decision 5) and its DST
  resolution rule (below) for the deferred calendar trigger.

## Consequences

- **Minimal Phase 8 cut (ships behind the deterministic acceptance suite; DoD "a
  job runs on schedule").** Durable `JobSpec` persisted via the existing
  define/checkpoint path; an `Interval { every_millis }` trigger (DST-immune,
  pure millis arithmetic); the single in-process reconcile loop on the injected
  clock with `(next_due, job_id)` ordering; the async submit-and-poll endpoints
  and the run-state lifecycle of decision 4; the CRC-framed run ledger with
  `last_fired_millis` and convergent restart with `catchup: skip | one`;
  deterministic run id from `(job, fire_millis)`; single-flight per job;
  at-least-once delivery with at-most-once commit; and audit of firings and runs.
  This is provable end to end under `ManualClock.advance()` with no real timers.
- **Deferred to a follow-on increment (its own scope, this ADR or a successor).**
  A `Calendar { cron, tz }` trigger, stored with an IANA timezone string and never
  a fixed offset, so DST is resolved at `next_due` evaluation by the stated rule:
  a wall-clock time skipped by spring-forward fires at the next valid instant; a
  wall-clock time doubled by fall-back fires once (deduped via `last_fired_millis`).
  Also deferred: configurable `max_retries` and backoff beyond the default-off
  field; an at-least-once delivery mode keyed on `Idempotency-Key` for connector
  refreshes; and a data-change trigger (`Trigger::OnCubeChange`) that the
  convergent loop is designed for but that is gated off in Phase 8 to honor the
  no-event-streaming non-goal, pairing later with ADR-0012's connector refresh.
- **Permanently out (ROADMAP section 13; need their own ADR per RG-16).** Event
  and webhook triggers, a declarative DAG and in-engine wait or poll-until
  primitives (P3), approval workflows, an operations console (baseline metrics and
  tracing only), gRPC transport, and cross-process or multi-node scheduling. The
  single-process store invariant makes the last of these unsupported by
  construction.
- **Validation.** A deterministic Phase 8 acceptance suite drives the loop under
  `ManualClock`: a job fires on schedule and commits; a re-derived firing reuses
  its id and is deduped; an interrupted run is recovered as `Failed{interrupted}`
  and the firing re-derives as due with bounded catch-up; a torn ledger tail is
  truncated without re-firing or dropping a job; single-flight coalesces an
  overlapping firing; and firings and runs produce correct, append-only,
  deterministic-timestamp audit records (ADR-0010) that carry no secrets or PII
  (RG-13). Bounded `spawn_blocking` worker count is exercised by the Phase 8 scale
  benchmark.
- **Net new state and surfaces.** A run ledger and a scheduler loop in
  `epiphany-flow`; a run registry, the async run endpoints, and the bounded worker
  pool wired through `epiphany-api` and `epiphany-server`; `JobSpec` as a durable,
  secured, audited model object. The flow engine, sandbox, and determinism model
  are unchanged: the scheduler is a layer above `run_flow`, exactly as ADR-0012's
  connectors are a layer that produces its input rows.
