# Agent Instructions: Epiphany

Epiphany is an in-memory, multidimensional **OLAP server** (Rust) with a **REST API** and a **React + TypeScript** web front end. The plan of record is [docs/ROADMAP.md](docs/ROADMAP.md). Engineering practices are in [docs/agentic_ai_programming_best_practices.md](docs/agentic_ai_programming_best_practices.md). Read both before non-trivial work.

## Three binding mandates (do not violate)
1. **Dead-simple for the end user.** Power lives in the engine; UI surfaces stay shallow (progressive disclosure). Casual users are never *required* to write MDX, rules, or flows, or to touch Git.
2. **Ultra performance and memory efficiency.** Sparse storage, packed integer cell keys, interned strings, compiled rules, streaming. Track the budgets in ROADMAP section 8. Regressions fail CI.
3. **Deterministic and directly testable.** Every feature is testable at its layer. A server-wide deterministic mode (injected clock, seeded RNG/IDs, fixed hash seed, ordered iteration) keeps outputs identical across runs. No flaky tests.

## Repository layout
- `crates/`: the Rust workspace.
  - `epiphany-determinism`: deterministic primitives (clock, RNG, ids). The determinism harness.
  - `epiphany-core`: multidimensional model (dimensions, cubes, sparse cell store, sandboxes, model-as-code).
  - `epiphany-calc`: rules, sparse feeds, automatic feeder inference, and provenance (Phase 4).
  - `epiphany-mdx`: MDX parser and evaluator for subsets and cellsets (Phase 3).
  - `epiphany-flow`: Flows, the TypeScript ETL and automation engine, with data sources and a scheduler (Phase 5).
  - `epiphany-security`: users, groups, and object and element authorization (Phase 7).
  - `epiphany-persist`: durability (transaction log, snapshots, recovery) (Phase 1 and 8).
  - `epiphany-engine`: the concurrent layer over durable stores: MVCC copy-on-write snapshot reads and atomic, all-or-nothing batch commits (ADR-0001) (Phase 2).
  - `epiphany-api`: REST and WebSocket surface on Axum (Phase 2).
  - `epiphany-server`: the daemon and composition root.
- `web/`: React + TypeScript client (Vite).
- `docs/`: ROADMAP, best-practices, and `docs/adr/` (architecture decisions).

## Build, test, run
Rust (from the repo root):
- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Format check: `cargo fmt --all -- --check` (fix with `cargo fmt --all`)
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Run the server: `cargo run -p epiphany-server`
- Optional HTTPS (ADR-0019): add `--features tls` (the release binaries are built with `embed-ui,tls`). It compiles a pure-Rust crypto stack (rustls + ring), so it needs the platform C toolchain; enable at runtime with `EPIPHANY_TLS=on` (self-signed) or `EPIPHANY_TLS_CERT`/`EPIPHANY_TLS_KEY`.

Web (from `web/`):
- Install: `npm ci` (first time: `npm install`)
- Dev server: `npm run dev`
- Typecheck: `npm run typecheck`
- Lint: `npm run lint`
- Build: `npm run build`

CI (`.github/workflows/ci.yml`) runs all of the above and gates merges.

## Local toolchain (Windows dev machine)
- **Rust:** rustup, default toolchain `stable-x86_64-pc-windows-gnu`. The GNU toolchain avoids needing the Visual Studio C++ Build Tools. `cargo` lives at `%USERPROFILE%\.cargo\bin`.
- **mingw-w64 binutils (needed from Phase 2 on):** the rustup GNU toolchain's bundled mingw ships `dlltool` and `ld` but not `as`, so crates that link Windows APIs via raw-dylib (`windows-sys` through `mio`/`tokio`, hence `axum`) fail to compile with a `dlltool ... CreateProcess` error. Phase 1 crates are pure Rust and unaffected. Fix: a portable mingw-w64 (WinLibs) extracted to `C:\Development\tools\mingw64`, used on the build PATH only, with the linker forced to Rust's own self-contained MSVCRT gcc (WinLibs is UCRT; letting its gcc become the linker mixes C runtimes and breaks linking):
  ```bash
  export PATH="/c/Development/tools/mingw64/bin:$PATH"
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$HOME/.rustup/toolchains/stable-x86_64-pc-windows-gnu/lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained/x86_64-w64-mingw32-gcc.exe"
  ```
  CI builds on Linux, so this is a local-Windows-only requirement. **C-compiling crates (the `tls` feature: rustls + ring) need a full GCC, not the binutils-only extract** at `C:\Development\tools\mingw64` (it lacks `cc1`). Install the full WinLibs UCRT GCC (`winget install -e --id BrechtSanders.WinLibs.POSIX.UCRT`) and prepend its bin (`%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_*\mingw64\bin`) before the binutils dir on PATH; keep the forced linker. Then `cargo build -p epiphany-server --features tls` and `cargo deny` build locally.
- **Node:** managed by fnm, with the version pinned in `.node-version`. In a fresh PowerShell, expose `node` and `npm` with:
  ```powershell
  fnm env --shell powershell | Out-String | Invoke-Expression; fnm use
  ```
  The concrete install is under `%APPDATA%\fnm\node-versions\<ver>\installation`.
- **git** is on PATH.

## Supported platforms
Epiphany ships as a single self-contained binary (the web UI is embedded with `--features embed-ui`). Cross-platform and multi-arch support is CI-gated, not assumed: on every push and PR, `cargo clippy -D warnings` and `cargo test --workspace` run on each of the tested targets below, so the cfg-gated paths (e.g. the Windows vs Unix command-connector code) are linted and exercised on every platform. Each runner refreshes to the current stable Rust before building.

| Target | Arch | CI runner | Status |
|---|---|---|---|
| Linux | x86_64 | `ubuntu-latest` | tested + released |
| Linux | aarch64 | `ubuntu-24.04-arm` | tested + released |
| Windows | x86_64 (MSVC) | `windows-latest` | tested + released |
| macOS | aarch64 (Apple Silicon) | `macos-latest` | tested + released |
| macOS | x86_64 (Intel) | (none) | builds, not CI-gated |

On a milestone tag (`m*`) the release job builds and uploads a single binary per tested target. Intel macOS (x86_64) still compiles, but it is not in the matrix: GitHub's Intel macOS runners are being retired and are capacity-starved (jobs can sit queued for hours), so gating CI on them is unreliable. Apple Silicon covers the current Mac platform.

A cube data directory is single-process: the engine serializes in-process writers and the store takes no cross-process OS file lock (see `epiphany-persist`). Run one server per data directory.

## Conventions
- Edition 2021. `unsafe_code = "deny"` workspace-wide; justify any per-crate exception in an ADR.
- **Tests are dependency-free.** The workspace uses no third-party test crates; write golden/parser tests as hand-authored `assert_eq!` and property tests as `DeterministicRng`-seeded loops, not `insta`/`proptest`. This keeps `cargo-deny` green with no new license surface. Adopting snapshot/property tooling is a deliberate, separately-scoped decision (it adds a transitive tree to triage), not a default.
- **Determinism:** never call the wall clock, an RNG, or unordered iteration in logic. Take a `Clock` or RNG from `epiphany-determinism`, and enforce stable ordering wherever output is observable.
- No secrets in source. Copy `.env.example` to `.env` for local config.
- **Naming:** do not reference IBM or TM1 (or related product or feature brand names) in docs or code. Describe capabilities generically. MDX and OData are open standards and are fine to name.
- Small, focused pull requests (RG-01). Update docs and ADRs when behavior or architecture changes (RG-08, RG-16).
- Each phase closes with a green, non-flaky deterministic acceptance suite that proves its definition of done.

## Status
Phase 0 (foundations) is complete: workspace, web app, CI, ADRs, and the determinism harness. Phase 5 (flows: ETL and automation) is complete and tagged as milestone **M5**: TypeScript flows run on the pure-Rust embedded engine **boa** (ADR-0004, chosen over QuickJS/V8/WASM by a build spike for the no-C single-binary build and full determinism control), with a dependency-free in-house TypeScript type stripper (`epiphany-flow::strip`, conservative and fail-loud) instead of a heavyweight transpiler. Flows declare `init`/`schema`/`rows`/`finalize` functions over a vectorized host API (`ctx.input()`, `ensureElements`/`addChild`, `writeCells`); dimensions gain an append-only, index-stable runtime growth path (`Cube::extend_schema`, re-packing cells only when a bit-width grows) so a flow can build members; each run is deterministic and sandboxed (wall-clock global removed, RNG throws, injected `ctx.now()`, no filesystem/network, a loop/recursion budget instead of a wall-clock timeout, exact-string numerics). The runner is pure (core-only) and returns a `FlowOutcome` the API applies through the engine (elements then cells), so `epiphany-engine` stays flow-free. Includes an in-house CSV reader, a guided CSV import wizard, flows + flow tests as durable model-as-code, a deterministic flow test runner, and a web flows workspace. SQL data source and the job scheduler are deferred (Phase 8). Gated by `epiphany-api/tests/m5_acceptance.rs`. A post-M5 follow-on adds **data-source connectors** (ADR-0012): flows ingest from outside the model through admin-defined connections on a fetch-at-the-edge / pure-transform split (a new `epiphany-connect` crate produces the same rows `ctx.input()` consumes, so the engine/sandbox/determinism are unchanged). The first connector is `command` - run a program and read its stdout (CSV/JSON), the one mechanism behind ingest-from-Python/PowerShell/exe - fenced by four controls (admin-defined fixed commands, no shell, the `EPIPHANY_ENABLE_COMMAND_CONNECTORS` server opt-in, and resource limits). An HTTP (reqwest) connector is the planned follow-on; there is deliberately no built-in database/ODBC connector - databases are reached through a command connection running the user's own client script, keeping the server a single pure-Rust binary (ADR-0012). Gated by `epiphany-api/tests/connectors.rs`. Phase 4 (rules, feeds, and calculation engine) is complete and tagged as milestone **M4**: a hand-written, dependency-free rules language (`epiphany-calc`: lexer, recursive-descent parser, canonical round-trippable `Display`) compiled once per version to a resolved, index-addressed AST (ADR-0007) and evaluated on-demand with a per-query memo and precise cycle detection; cross-cube references (fully addressed), area-overlap precedence by source order, and consolidation overrides; automatic feeder inference with under/over-feed validation against the always-correct dense consolidation path, honestly reporting rules it cannot localize as opaque (ADR-0005); calculation provenance ("explain"); rules and rule unit tests as durable model-as-code with a deterministic test runner; and a web rules workspace (editor with located validation, feeder diagnostics, explain tree, test runner). The calc layer depends on core only, reaching values through a core-owned `CellResolver` seam injected by the API `CalcFactory`, so `epiphany-engine` stays calc-free. Gated by `epiphany-api/tests/m4_acceptance.rs`. Phase 3 (subsets, views, and MDX) is complete and tagged as milestone **M3**: a hand-written, dependency-free MDX set engine (`epiphany-mdx`: lexer, recursive-descent parser, tree-walking evaluator) for dynamic subsets; a core query model (`epiphany-core::query`: static/dynamic `Subset`, `View` with crossjoin nesting and zero-suppression, `Cellset`, `execute_view`) behind a core-owned `SetEvaluator` seam implemented in epiphany-mdx and injected at the server (ADR-0011); durable subset/view definitions through the store/engine commit path (checkpoint-on-define, no WAL change); a REST subset/view/cellset surface with owner+visibility enforcement; and a point-and-click web subset editor, view builder, and nested cellset grid (MDX is an opt-in escape hatch). Gated by `epiphany-api/tests/m3_acceptance.rs`. Phase 1 (core model) is complete and tagged as milestone **M1**: the multidimensional model with N/C/S elements, deterministic weighted consolidation with alternate rollups, attributes and aliases, string cells, model-as-code serialization, and runtime persistence (WAL + snapshots + crash recovery) have landed, within the per-cell memory and aggregation budgets. Phase 2 (REST API and minimal web UI) is complete and tagged as milestone **M2**: an Axum REST API (argon2id auth with in-memory sessions, cube and dimension read, cell read/write, transactional all-or-nothing batch, WebSocket change notifications, a published OpenAPI document), the engine's MVCC arc-swap copy-on-write (ADR-0001, in `epiphany-engine`), and a React pivot-grid client served from the single binary (`cargo run -p epiphany-server --features embed-ui` after `npm run build`). HTTPS is deferred to Phase 8 (loopback HTTP for M2). See the [roadmap](docs/ROADMAP.md).
