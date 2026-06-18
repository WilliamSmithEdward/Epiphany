# ADR-0034: Native SQL data source (PostgreSQL + MySQL)

Status: Accepted
Date: 2026-06-17

## Context

Until now a database was reachable only through a `command` connection running
the operator's own client script (ADR-0012 deliberately dropped ODBC to keep the
single pure-Rust binary: an ODBC bridge needs the system driver manager plus
unsafe FFI). That works but is high-friction: the operator must install a client
runtime and a driver, then write and maintain a script just to pull a table.

A native, point-and-click SQL source is the most-requested ergonomic gap. The
two hard constraints carried over from ADR-0012/0019/0030 are non-negotiable:

- **Single static binary** — no system driver manager, no runtime system library,
  no native-tls/OpenSSL; TLS stays on rustls with the **ring** provider (the
  pinned provider across the server and HTTP connector; aws-lc-rs reignites the
  cross-compile/cargo-deny fight).
- **MIT / MIT-adjacent (permissive) licensing** for the whole transitive tree.

## Decision

**1. Reuse the connector boundary unchanged.** Fetch stays impure and at the edge
(`epiphany-connect`); the transform stays pure. A SQL connection produces the
same `Vec<Row>` (`Vec<(column, value)>`) the command and HTTP connectors produce,
so the flow engine, sandbox, determinism, and flow tests are untouched. A live
query is the documented non-deterministic input boundary (flow tests pin inline
rows), exactly as for command/HTTP.

**2. Pure-Rust drivers, not ODBC; one cargo feature per engine.** Each engine is
a pure-Rust driver over rustls pinned to **ring**, behind its own build feature:

- **PostgreSQL** (`postgres` feature): `tokio-postgres` over `tokio-postgres-rustls`
  with bundled `webpki-roots`.
- **MySQL / MariaDB** (`mysql` feature): `mysql_async` with `default-rustls-ring`
  + `webpki-roots`, and `minimal-rust` so it stays pure (no C `libz-sys`).

A build spike confirmed empirically, on the GNU toolchain, that both satisfy the
constraints:

- `cargo deny --all-features check` → **advisories/bans/licenses/sources ok**;
  the whole Postgres + MySQL + TLS tree is MIT / MIT-adjacent (every crate is
  MIT, MIT-OR-Apache-2.0, or an OR-expression an allowlisted license satisfies;
  the only non-permissive tokens that appear, `Unlicense` and `BSL-1.0`, are
  always OR'd with MIT/Apache).
- the dependency graph resolves to **ring** with **no aws-lc-rs, no native-tls,
  no openssl, no C `-sys` libraries**, and both compile + link into the binary.

**SQL Server is intentionally NOT shipped.** Its only pure-Rust driver,
`tiberius` 0.12 (the latest published), pins `rustls` 0.21, which pulls the old
`webpki` 0.22 — a crate with **active** RUSTSEC advisories: certificate
name-constraint *verification bypasses* (RUSTSEC-2026-0098 / -0099) and a
reachable CRL-parsing panic (RUSTSEC-2026-0104). That fails the supply-chain gate
(`cargo deny`) and would make the `verify-full` TLS mode actively unsafe, so SQL
Server is **deferred** until `tiberius` moves to `rustls` 0.23 / `rustls-webpki`.
A SQL Server database is reachable meanwhile through a `command` connection
running the operator's own client script (ADR-0012). New engines remain additive
behind their own feature, so SQL Server can be added the moment its driver is
clean.

**3. The ADR-0030 fail-closed fence, verbatim.** Three independent gates:

- a `postgres` **cargo build feature** — a default build cannot run a SQL
  connection (the API returns `SQL_NOT_BUILT` 422); release builds opt in;
- a runtime **`EPIPHANY_ENABLE_SQL_CONNECTORS`** flag, off by default;
- a **host allowlist** `EPIPHANY_SQL_ALLOWED_HOSTS`, enforced both when a
  connection is defined and again at fetch.

Credentials live in the existing write-only **secret store**, referenced by
**name**; the connector never sees the store (the API resolves the password and
passes it in). The model-as-code carries only the secret name, never the value.

**4. Fixed, admin-defined query; no flow-supplied input.** Like the command
connector's fixed argv, the query is set by an admin at definition time and is
never assembled from flow input, so flows present no SQL-injection surface.
Parameterized/templated queries are deferred.

**5. Threading.** `tokio-postgres` is async, but `fetch_connection_rows` is sync
and runs on the Axum worker (the HTTP connector already fetches there). `fetch_sql`
therefore runs the query on a **dedicated `std::thread` with a current-thread
tokio runtime**, returning `Vec<Row>` synchronously. This avoids a
runtime-within-a-runtime panic and any dependence on `block_in_place` / the
server runtime flavor, and keeps the connect crate a plain synchronous producer.
A fresh connection is opened per fetch (no pool v1; fetches are infrequent:
preview and flow/scheduled runs).

**6. TLS modes (`sslmode`), fail-closed default.** `verify-full` (rustls + the
bundled public roots, default), `require` (encrypt over rustls but **do not**
verify the certificate — for self-signed internal DB certs, the libpq
`sslmode=require` behavior), `disable` (no TLS). The secure mode is the default
([[prefer-fail-closed-defaults]]); an operator with a self-signed internal
database opts down explicitly to `require`. The host allowlist applies in every
mode. `require` is named and documented as the deliberate encrypt-without-verify
choice; it is strictly better than the no-TLS the command-connector-script path
uses today.

**7. Bounds.** A connect/statement timeout (`timeout_ms`, REST-coerced to a safe
default) and a result-row cap (`MAX_SQL_ROWS`, the `MAX_CSV_ROWS` analog).

## Consequences

- The single-binary + permissive-licensing posture is preserved, proven by the
  spike rather than asserted.
- The `postgres` feature's dependency tree is added but **off by default**; the
  lean build is unchanged and release builds opt in (as with `http`/`tls`).
- One driver supports one database; broadening to SQL Server / MySQL is additive.
- Live-fetch verification needs a real Postgres (the impure boundary, like the
  HTTP connector's localhost test). The pure row-mapping, the model-as-code
  round-trip, and the API gates are unit/integration-tested; end-to-end against a
  live database is a documented manual step.

## Deferred

SQL Server (blocked on `tiberius` updating off the vulnerable `webpki` 0.22, as
above); parameterized/templated queries; connection pooling; SQL write-back; a
`verify-ca` (CA-pinned, hostname-unverified) TLS mode; result streaming for very
large tables.
