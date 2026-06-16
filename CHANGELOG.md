# Changelog

All notable changes to Epiphany are recorded here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Releases are git tags
of the form `mN[.M]`, where the integer part tracks the roadmap phase and the
point part is a follow-on release; binaries for each release are attached to the
matching [GitHub release](https://github.com/WilliamSmithEdward/Epiphany/releases).

## [m8.7] - 2026-06-15

The performance release, and the first tag to gather the post-roadmap programs
that landed after `m8.6`.

### Performance

- **Deterministic parallel aggregation** (ADR-0028). Large cold view reads now
  aggregate across every core: a view's value grid is filled by scoped-thread
  workers, one per disjoint row band, above a cell-count threshold (small reads
  stay serial). Results are provably bit-identical to the serial path regardless
  of worker count or scheduling, verified by a serial-vs-parallel equality test.
  Measured 4.5x to 6.9x faster on large consolidated views (for example a
  40k-cell all-consolidated crossjoin drops from about 454 ms to about 72 ms on
  14 cores). No new dependency.
- **Persistent view cache** (ADR-0028). Repeat reads of an unchanged view are
  served from a bounded, version-keyed cache, meeting the cached-read budget
  (p99 under about 100 ms). The cache key is lossless over the cube version, the
  view shape, the active sandbox, and the caller's exact element-deny set, so it
  is self-invalidating on any write and never returns a stale or cross-user
  result. Configurable with `EPIPHANY_VIEW_CACHE_ENTRIES` (default 256, 0
  disables).
- New self-contained `view_exec` benchmark; `docs/PERFORMANCE.md` and the README
  performance section updated with the observed numbers.

### Added

- **Web UI overhaul** (ADR-0020): a vendored design-system foundation (tokens,
  dark mode, Radix primitives, no component framework), a persona-gated app shell
  with a command palette, and an overhauled pivot grid with drill-down
  provenance.
- **Model editing from the UI and REST** (ADR-0021): create cubes, add members,
  build consolidation hierarchies, and define attributes without editing model
  files. The engine cube set became swap-on-write so cubes can be created at
  runtime.
- **Excel add-in** (ADR-0022): an Excel-DNA `.xll` with `=EPIPHANY.READ(...)`, a
  WebView2 login that reuses the server sign-in (token stored encrypted), and
  transactional write-back.
- **Shared, independent dimensions** (ADR-0024): a server-level dimension
  library; cubes reference a shared dimension and a single grow fans out to every
  referencing cube.
- **Data-source connector preview and admin dashboards** (ADR-0027): a gated,
  row-capped connection preview, a global runs view, and a Server Overview
  dashboard.
- **In-house web syntax highlighting** for the rules and flow editors (ADR-0026),
  with no heavyweight editor dependency.
- Onboarding polish: a first-run welcome card, a login hint, and
  `docs/QUICK_START.md`.
- Native Windows service (SCM) hosting and deployment artifacts.
- This changelog.

### Changed

- **Modular per-object-kind permissions** (ADR-0023): roles for users and groups
  granted per object kind, fail-closed by default. This supersedes the object
  grants of ADR-0015 and the global cube grants of ADR-0016, which were removed
  along with the open-by-default posture.

### Security

- **Tier-2/3 hardening** (ADR-0017 family and the new ADR-0025): login-timing
  dummy-hash to remove user enumeration, parser recursion-depth guards, a
  password-strength policy, sliding session idle-timeout with revoke on password
  change, CSV/JSON ingestion row caps, a validated connector working directory,
  and a documented operator-managed at-rest-encryption posture.

## [m8.6] - 2026-06-15

- **Optional TLS / HTTPS** (ADR-0019), off by default. `EPIPHANY_TLS=on` serves
  HTTPS with an auto-generated self-signed certificate; `EPIPHANY_TLS_CERT` and
  `EPIPHANY_TLS_KEY` serve a real certificate. Behind a `tls` cargo feature;
  release binaries include it.

## [m8.5] - 2026-06-15

- **HTTP-surface hardening** (ADR-0018): security response headers (nosniff,
  frame-deny, referrer-policy, HSTS, a same-origin CSP) and an explicit 8 MiB
  request body-size limit.

## [m8.4] - 2026-06-14

- **Complete automatic feeder inference** (ADR-0005): inference rewritten as a
  fixpoint over potentially-non-zero leaves, with base-potency analysis closing
  the constant/conditional under-feed. Read-path safety was re-verified: reads
  always use the dense, always-correct consolidation, so a feeder can never make
  a total wrong.

## [m8.3] - 2026-06-14

- **Authentication and credential hardening** (ADR-0017): per-username login
  lockout, enforcement of must-change-password before data access, owner-only
  (0600) on-disk secret files from creation, and a read-once generated admin
  password written to a file rather than stdout.

## [m8.2] - 2026-06-14

- **Global cube grants with explicit deny** (ADR-0016): broad-across-all-cubes
  grants and per-cube deny with most-specific-tier-wins precedence. (Later
  superseded by the modular permission model in ADR-0023.)

## [m8.1] - 2026-06-15

- **Secure-by-default cube access**: an ungranted cube is closed to non-admins by
  default; access is opened only by an explicit grant or by opting into a trusted
  single-org posture.

## [m8] - 2026-06-15

Phase 8, the final roadmap phase.

- **Flow scheduling and orchestration** (ADR-0013): a declarative in-process
  reconcile loop with interval triggers, a durable CRC-framed run ledger, and
  convergent crash recovery, all reproducible under the injected clock.
- **Operational hardening**: audit retention and rotation, performance and memory
  benchmarks validated against the budgets.

## [m7] - 2026-06-15

- **Object and element security** (ADR-0015) with a four-level lattice, plus
  deny-the-rollup element security that closes the subtraction-inference leak.
- **Audit logging** (ADR-0010): an append-only, CRC-framed, admin-queryable
  stream with no secrets or PII.

## [m6] - 2026-06-14

- **What-if sandboxes** (ADR-0014): per-user, per-cube copy-on-write overlays
  where rules and consolidations recompute over proposed changes, then commit or
  discard.

## [m5.1] - 2026-06-14

- **Command data-source connector** (ADR-0012): run an admin-defined program and
  read its CSV/JSON output as flow input (for example a Python or PowerShell
  script for a database pull), behind four fail-closed controls.
- Cross-platform, multi-architecture CI; first release with prebuilt binaries for
  Linux x86_64/aarch64, Windows x86_64, and macOS aarch64.

## [m5] - 2026-06-14

- **Flows** (ADR-0004): TypeScript ETL and automation on an embedded pure-Rust
  JavaScript engine, with an in-house dependency-free type stripper, a CSV import
  wizard, runtime dimension extension, and deterministic flow unit tests.

## [m4] - 2026-06-14

- **Calculation engine** (ADR-0007): a rules language with compiled, on-demand
  evaluation, per-query memoization, and cycle detection; exact decimal numerics
  (ADR-0008); automatic feeder inference and validation (ADR-0005); and
  calculation provenance ("explain").

## [m3] - 2026-06-13

- **Query model** (ADR-0011): a dependency-free MDX set engine, static and
  dynamic subsets, views with crossjoin nesting and zero-suppression, cellsets,
  and the point-and-click web subset editor, view builder, and cellset grid.

## [m2] - 2026-06-13

- **REST API and web client**: an Axum JSON API (argon2id auth, sessions, cube
  and cell reads, transactional batch writes, WebSocket, hand-written OpenAPI), an
  MVCC arc-swap engine (ADR-0001), and a React pivot grid with write-back. Ships
  as a single binary with the UI embedded.

## [m1] - 2026-06-13

- **Core model**: dimensions and elements with alternate rollups and weighted
  consolidations, attributes and aliases, string cells, a packed-key sparse cell
  store (ADR-0006, about 17 bytes per cell), exact `Fixed` numerics (ADR-0008),
  model-as-code TOML round-trip (ADR-0003), and a write-ahead log with snapshots
  and crash recovery.

[m8.7]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m8.7
[m8.6]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m8.6
[m8.5]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m8.5
[m8.4]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m8.4
[m8.3]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m8.3
[m8.2]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m8.2
[m8.1]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m8.1
[m8]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m8
[m7]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m7
[m6]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m6
[m5.1]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m5.1
[m5]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m5
[m4]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m4
[m3]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m3
[m2]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m2
[m1]: https://github.com/WilliamSmithEdward/Epiphany/releases/tag/m1
