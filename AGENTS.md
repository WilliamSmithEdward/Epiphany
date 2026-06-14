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
  CI builds on Linux, so this is a local-Windows-only requirement.
- **Node:** managed by fnm, with the version pinned in `.node-version`. In a fresh PowerShell, expose `node` and `npm` with:
  ```powershell
  fnm env --shell powershell | Out-String | Invoke-Expression; fnm use
  ```
  The concrete install is under `%APPDATA%\fnm\node-versions\<ver>\installation`.
- **git** is on PATH.

## Conventions
- Edition 2021. `unsafe_code = "deny"` workspace-wide; justify any per-crate exception in an ADR.
- **Tests are dependency-free.** The workspace uses no third-party test crates; write golden/parser tests as hand-authored `assert_eq!` and property tests as `DeterministicRng`-seeded loops, not `insta`/`proptest`. This keeps `cargo-deny` green with no new license surface. Adopting snapshot/property tooling is a deliberate, separately-scoped decision (it adds a transitive tree to triage), not a default.
- **Determinism:** never call the wall clock, an RNG, or unordered iteration in logic. Take a `Clock` or RNG from `epiphany-determinism`, and enforce stable ordering wherever output is observable.
- No secrets in source. Copy `.env.example` to `.env` for local config.
- **Naming:** do not reference IBM or TM1 (or related product or feature brand names) in docs or code. Describe capabilities generically. MDX and OData are open standards and are fine to name.
- Small, focused pull requests (RG-01). Update docs and ADRs when behavior or architecture changes (RG-08, RG-16).
- Each phase closes with a green, non-flaky deterministic acceptance suite that proves its definition of done.

## Status
Phase 0 (foundations) is complete: workspace, web app, CI, ADRs, and the determinism harness. Phase 3 (subsets, views, and MDX) is complete and tagged as milestone **M3**: a hand-written, dependency-free MDX set engine (`epiphany-mdx`: lexer, recursive-descent parser, tree-walking evaluator) for dynamic subsets; a core query model (`epiphany-core::query`: static/dynamic `Subset`, `View` with crossjoin nesting and zero-suppression, `Cellset`, `execute_view`) behind a core-owned `SetEvaluator` seam implemented in epiphany-mdx and injected at the server (ADR-0011); durable subset/view definitions through the store/engine commit path (checkpoint-on-define, no WAL change); a REST subset/view/cellset surface with owner+visibility enforcement; and a point-and-click web subset editor, view builder, and nested cellset grid (MDX is an opt-in escape hatch). Gated by `epiphany-api/tests/m3_acceptance.rs`. Phase 1 (core model) is complete and tagged as milestone **M1**: the multidimensional model with N/C/S elements, deterministic weighted consolidation with alternate rollups, attributes and aliases, string cells, model-as-code serialization, and runtime persistence (WAL + snapshots + crash recovery) have landed, within the per-cell memory and aggregation budgets. Phase 2 (REST API and minimal web UI) is complete and tagged as milestone **M2**: an Axum REST API (argon2id auth with in-memory sessions, cube and dimension read, cell read/write, transactional all-or-nothing batch, WebSocket change notifications, a published OpenAPI document), the engine's MVCC arc-swap copy-on-write (ADR-0001, in `epiphany-engine`), and a React pivot-grid client served from the single binary (`cargo run -p epiphany-server --features embed-ui` after `npm run build`). HTTPS is deferred to Phase 8 (loopback HTTP for M2). See the [roadmap](docs/ROADMAP.md).
