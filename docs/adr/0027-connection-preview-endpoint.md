# ADR-0027: Connection preview endpoint

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Epiphany maintainers
- **Phase:** Post-roadmap (web UI, W4 connector wizard)

## Context

Defining a command connection (ADR-0012) is currently a blind edit: the admin
types a program, args, and format, saves, and only finds out whether it works by
attaching it to a flow and running the flow. The W4 connector wizard wants a
"Test connection" button that runs the connection and shows a few sample rows, so
the admin can confirm the program emits parseable output before saving it into a
flow pipeline.

This is a new, client-driven, subprocess-invoking surface (distinct from running
a flow), so its contract - what it runs, what it returns, how it is bounded, and
who may call it - deserves to be recorded.

## Decision

Add **`POST /api/v1/cubes/{cube}/connections/{name}/preview`**: run the named
command connection and return a small sample of the parsed rows.

Contract:

- **Runs the connection as defined.** It executes the saved `CommandSpec`
  (program, args, format, working_dir, timeout) through the same
  `epiphany-connect` path a flow uses; it never takes a program or args from the
  request body, so it adds no command-injection surface beyond ADR-0012.
- **Read-only with respect to the model.** It returns rows; it never stages or
  commits any change.
- **Bounded.** The connector's existing fences apply unchanged: the per-run
  timeout, the 16 MiB stdout cap, and the ingestion row cap (ADR-0012 addendum).
  The response itself returns at most the first **20** parsed rows plus the total
  row count, so a large feed cannot bloat the response or the UI.
- **Gated by `Connection:Write` and the command-connector enable flag.** Running
  a program is host code execution and its output can be sensitive, so preview is
  restricted to holders of `Connection:Write` (the same right that defines
  connections), not merely `Connection:Read`, and it still requires
  `EPIPHANY_ENABLE_COMMAND_CONNECTORS` like every other command-connector run.
  A program that exits non-zero returns the captured stderr as a 422, so the
  admin can debug it.
- **Audited.** The preview is recorded like other connection operations.

## Alternatives considered

- **Amend ADR-0012 instead of a new ADR.** Rejected: preview is a distinct,
  client-driven execution surface with its own timeout/cap/gating contract; a
  focused ADR keeps it independently citable. ADR-0012 remains the connector
  foundation.
- **Gate on `Connection:Read`.** Rejected: preview executes the program and can
  surface sensitive output; tying it to `Connection:Write` keeps execution with
  the role that defines connections.
- **Reuse the flow-run endpoint with an empty flow.** Rejected: clunky, and it
  conflates "test the data source" with "run a transform"; a dedicated endpoint
  is clearer and easier to gate and bound.

## Consequences

- The connector wizard's "Test" button calls this endpoint and shows the sample
  rows (or the stderr on failure) inline, capped at 20 rows.
- No new engine capability; it is a thin REST handler over the existing
  `epiphany-connect` runtime, the existing fences, and the existing model lookup.
- OpenAPI documents the path and its response; the route-coverage self-check
  includes it.
