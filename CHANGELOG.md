# Changelog

All notable changes to Epiphany are recorded here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Releases are git tags
of the form `mN[.M]`, where the integer part tracks the roadmap phase and the
point part is a follow-on release; binaries for each release are attached to the
matching [GitHub release](https://github.com/WilliamSmithEdward/Epiphany/releases).

## [Unreleased]

### Added

- **Editable MDX queries in the cube view.** The "MDX for this view" dialog is now
  an editable textarea with a **Run** button that executes a full
  `SELECT … ON COLUMNS / ON ROWS FROM [cube] [WHERE ( … )]` query against the cube
  and renders the resulting cellset, surfacing parse/validation errors inline. The
  `epiphany-mdx` parser gained a `SELECT`-query layer (`parse_query`) above its set
  sublanguage, and a new `POST /api/v1/cubes/{cube}/mdx` endpoint executes it through
  the same `Read` authorization and element-masking path as the ad-hoc cellset query.
- **Copy to / Move to for dimension members.** The member action menu now offers
  **Copy to** (add the member to another consolidation as an alternate rollup, keeping
  its existing parents; a consolidation it already belongs to is greyed as "already
  contains") and **Move to** (reparent it under exactly one consolidation or the top
  level, with a confirm when that would drop its other rollups). Self and descendant
  targets are excluded so a cycle can't be formed.
- **Separate zero-suppression for rows and columns.** The single "Suppress zeros"
  option is now two independent toggles, **Suppress zero rows** and **Suppress zero
  columns**, on the cube viewer's Save dialog and the view editor. Views saved before
  this map a legacy `suppress_zeros: true` to both flags.

### Changed

- **Views are created from the cube viewer.** The duplicative "Add view..." authoring
  path was removed; arrange a layout in the cube viewer and use **Save view** to create
  one. The Views workspace now opens and edits existing views in place.

### Fixed

- **Cube view: pivot rows/cells no longer duplicate or strand cells** under an
  alternate rollup (a member reachable from two consolidation parents). Rows, cells,
  and header spans are keyed by each member's drill path instead of its bare name,
  and saved-subset member lists are de-duplicated.
- **Cube view: the "Who can see it" dropdown** in the Save-view dialog now paints
  above the dialog instead of behind it (the dropdown layer ranked below the modal
  overlay in the z-index scale).
- **Dimension editor: right-clicking a member opens the action menu at the cursor**
  (it previously anchored to the row's ⋯ button).
- **Cube tab strip: removed the spurious vertical scroll arrows.**
- **Durability: a checkpoint can no longer lose data on a crash.** The snapshot is
  fsynced (file and directory) before the write-ahead log is cleared, and a
  reindexing dimension edit (reorder/delete/insert) folds outstanding writes and
  empties the WAL before rewriting the snapshot, so recovery can never replay a
  stale element index onto the new layout. A WAL truncated below its header by a
  crash is now treated as fresh rather than failing the open.
- **A failed model-definition commit no longer leaks into the next one.** If
  persisting a definition change fails midway, the in-memory model is rolled back
  to the last published version, so the partial change cannot ride out on a later
  successful commit.
- **What-if sandbox overrides now follow structural dimension edits.** Reordering,
  inserting, or deleting a member remaps each sandbox override to its member's new
  index (and drops overrides on a deleted member) instead of leaving them on the
  wrong member or panicking the checkpoint.
- **Flows: the TypeScript stripper no longer drops `switch` case bodies** such as
  `case 0: break;` (a case/default colon was mistaken for a type annotation), and
  no longer corrupts `interface` used as an ordinary identifier.
- **Dimension aliases: reassigning an alias no longer leaves the old one
  resolvable** (which also falsely blocked another member from claiming the freed
  name).
- **Cube view: an alternate-rollup member can now be expanded independently under
  each parent** (expansion is keyed by drill path, not member name); a slow cell
  read can no longer overwrite a newer one; and a set that fails to resolve surfaces
  the error instead of silently blanking the axis.
- **Dimension editor: rapid keyboard and context-menu structural edits are gated**
  so they cannot race a prior in-flight edit, and **member-table range selection**
  survives a re-sort or filter.
- **A non-admin schedule author can open the jobs workspace** again (the admin-only
  run list no longer fails the whole load), and **opening a saved view** no longer
  drops its layout when the cube loads slowly.
- **Windows service: a failed startup now reports a non-zero exit to the SCM**
  instead of a clean stop.
- **Dimension editor: drag-and-drop is a reliable, parent-aware move.** A member can
  now be dragged into a consolidation, and every drag emits the minimal, duplicate-free
  edits: dropping onto a consolidation moves the dragged occurrence there while keeping
  the member's other rollups, and the additive add runs before the detach so an
  interrupted multi-step move can never orphan a member. Editor and pivot now agree on
  child order, and the drop-inside target band is larger and recomputed at release.
- **Login: you stay signed in after reloading the page.** The session is restored from
  the cookie on load (it was previously held only in memory), including the forced
  password-change screen, and the duplicate password-manager autofill prompt is gone.
- **A render error in one pane no longer blanks the whole app.** A new error boundary
  shows a recoverable fallback for the affected pane and leaves the tab strip and tree
  usable; switching or closing the tab clears it.
- **No more silent data loss from background reloads.** Unsaved rule edits survive a
  WebSocket reload caused by activity in another tab; editing a flow or schedule that
  was deleted elsewhere now disables Save (so it is not silently re-created) and says so;
  entering the Administration view honors the unsaved-edit guard; and deleting an open
  object no longer pops a nonsensical post-delete discard prompt.
- **Object tree stays correct under concurrent loads and search.** A slower, stale
  response can no longer overwrite freshly loaded children, and a delete or rename during
  a search no longer leaves a stale row in the results.
- **Dimension editor robustness and accessibility.** Adding a member as a child is now
  all-or-nothing (a declined conversion or a failed step no longer strands an orphan with
  the form trapped on a duplicate-name error), duplicate names are caught before the
  round-trip, an empty dimension shows guidance, successful edits and keyboard moves are
  announced to screen readers, and a pathological cyclic edge set can no longer overflow
  the tree builder.
- **Cube viewer: stale numbers are not shown during a refresh.** While re-querying after
  a layout or filter change the grid dims and announces "Refreshing", so a previous
  slice's values are never read as the current ones, and an empty row axis shows an
  explicit "No data" row.
- **Accessibility: closing the active tab moves keyboard focus to a neighbor** (rather
  than losing it on the document body), and the Administration sub-nav is now a correct
  button group instead of a malformed tabs widget.

### Security

- **The last administrator can no longer be deleted or demoted**, which would have
  locked everyone out of the admin control plane.
- **Sandbox lifecycle endpoints now require cube access** (Read to list/create/get/
  discard, Write to commit), matching the data endpoints; a user with no grant on a
  cube could previously enumerate and operate sandboxes there.
- **Deleting a user, resetting their password, or an admin setting their password
  now revokes that user's sessions**, so a stale token stops working immediately.
- **The session cookie is marked `Secure` when the server serves over TLS**, so it
  is never sent on a plain-HTTP request.
- **Serving plain HTTP on a non-loopback address now logs a warning** that the API
  and login are exposed unencrypted.

## [m8.8] - 2026-06-18

The modeling release: native SQL sources, global multi-cube automation, and full
dimension structural editing, on top of the connector and spreading work that
landed after `m8.7`.

### Added

- **Native SQL data sources** (ADR-0034): a flow can ingest directly from
  PostgreSQL (tokio-postgres) or MySQL and MariaDB (mysql_async), both pure-Rust
  over rustls/ring with no native-tls, openssl, or aws-lc-rs. Each driver is
  behind its own off-by-default build feature and is fenced at runtime by an
  enable flag (`EPIPHANY_ENABLE_SQL_CONNECTORS`), a host allowlist
  (`EPIPHANY_SQL_ALLOWED_HOSTS`), a secret referenced by name, and a fixed admin
  query; `ssl_mode` supports verify-full, require, and disable. SQL Server is
  deferred (its current Rust driver pins a vulnerable TLS stack that fails the
  supply-chain gate); reach it through a command connection meanwhile.
- **Global, multi-cube, code-first automation** (ADR-0035): flows, schedules, and
  connections moved out of per-cube models into one server-global automation
  model. A single flow can read and write several cubes and grow several
  dimensions in one run, owned by none of them. Data sources are UI-driven (a
  global connection reference or a flow-scoped local connection, addressed in code
  by bare name or `local.<name>`), while outputs stay code-first. Scheduled runs
  execute as the flow's owner, fail-closed. Existing per-cube flows, jobs,
  connections, and flow tests migrate into the global store on first boot.
- **Dimension structural editing and a cube-agnostic editor** (ADR-0036): a
  dimension is now fully editable. Reorder, reparent, convert kind, insert,
  delete, add a member to a consolidation, and remove a member from one
  consolidation, with every index-changing edit remapping stored cells
  transactionally (and fanning the same remap out to every referencing cube for a
  shared dimension). A standalone, hierarchy-only, table-driven, drag-and-drop
  dimension editor with full keyboard parity (WCAG 2.2 SC 2.5.7): each drag
  gesture has a row-menu equivalent, and a member is picked up with Space and
  moved with the arrow keys. Delete is intent-aware: a member that rolls up to one
  or more consolidations chooses between removing it from selected consolidations
  (kept, with its data) and deleting it from the dimension (removed everywhere,
  behind an explicit data-loss confirm); a root member deletes from the dimension
  directly.
- **One global dimension namespace** (ADR-0031): a single dimensions list spanning
  the registry and cube-embedded dimensions, with a promote action to reuse an
  embedded dimension across cubes, and attributes carried through promotion to
  every referencing cube.
- **Scalable member table** (ADR-0032): one shared, virtualized table backs the
  dimension and set editors, with toggleable attribute columns, wildcard and alias
  search, sortable columns, a flat or hierarchy view, inline attribute-value
  editing, per-column filters, relationship set operators (children, descendants,
  parents, ancestors, siblings, leaves-of), and keep or hide.
- **Object-explorer overhaul**: an object-centric tree that shows the dimension
  consolidation hierarchy and a global object search, multiple dimensions per
  pivot axis with nested headers, an MDX previewer for the cube viewer, a tabbed
  object workspace, and saved Views and dimension Sets.
- **HTTP fetch connector and secret store** (ADR-0030): a flow can ingest from an
  HTTP(S) API (CSV or JSON) in addition to a command. The capability is off by
  default and bounded by a host allowlist (`EPIPHANY_ENABLE_HTTP_CONNECTORS`,
  `EPIPHANY_HTTP_ALLOWED_HOSTS`); redirects are not followed (SSRF control).
  Credentials live in an owner-only secret store, referenced by name from the
  connection, so they never enter the model or Git; the value is write-only over
  the API (`/api/v1/secrets`, admin) and never returned, logged, or audited.
  Behind an `http` build feature (ureq over rustls/ring; no native-tls, no
  aws-lc-rs).
- **Data spreading** (ADR-0029): enter a value at a total and distribute it
  across the leaves underneath, by `equal`, `proportional`, `repeat`, or `clear`.
  Spreads are exact (the leaves sum back to the entered value) and deterministic,
  honor the active what-if sandbox, and are fail-closed under element security
  (if any contributing leaf is denied, the whole spread is refused). New
  `POST /api/v1/cubes/{cube}/cells/spread` endpoint and a pivot-grid spread
  control. Spreading into a weighted consolidation is refused in v1.
- Admin reset of a user to a system-generated temporary password with a forced
  change at next sign-in, and an unsaved-edit guard across the flow, view, and
  schedule editors.
- View-cache counters on the admin Server Overview dashboard (cached views,
  hits, misses, hit rate) via a new `GET /api/v1/overview` endpoint.

### Security

- **Fail-closed element security on global dimension reads** (ADR-0033): reading a
  global dimension masks its members, edges, and attribute values by the union of
  the referencing cubes' element ACLs; an unknown principal is denied. Supersedes
  the deferred per-id re-key with no ACL-format change or migration.
- The SQL and HTTP connectors are off by default and require an explicit build
  feature, a runtime enable flag, and a host allowlist; their secrets are
  referenced by name and never returned, logged, or audited. The supply chain
  stays clean (`cargo deny --all-features check` is green).

### Changed

- Audience-appropriate copy: admin, developer, and first-run wording scrubbed from
  the pre-auth and non-admin surfaces, and the operator panels gated to admins.
- Em dashes removed from the ADRs and the GUI copy (house style).

### Fixed

- Explorer tree: right-click now targets the clicked object's menu, a nested-row
  click no longer bubbles to ancestor rows (re-selecting a parent and collapsing
  roots), and an initially-expanded node loads its children on mount.
- Reparenting a populated leaf into a consolidation no longer leaves an orphan
  cell that broke snapshot reload.
- `FlowDto.name` is optional in the request body, so a name-less update no longer
  returns 422.

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
