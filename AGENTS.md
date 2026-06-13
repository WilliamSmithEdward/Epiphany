# Agent Instructions — Epiphany

Epiphany is an in-memory, multidimensional **OLAP server** (Rust) with a **REST API** and a **React + TypeScript** web front end. The plan of record is [docs/ROADMAP.md](docs/ROADMAP.md); engineering practices are in [docs/agentic_ai_programming_best_practices.md](docs/agentic_ai_programming_best_practices.md). Read both before non-trivial work.

## Three binding mandates (do not violate)
1. **Dead-simple for the end user.** Power lives in the engine; UI surfaces stay shallow (progressive disclosure). Casual users are never *required* to write MDX, rules, or flows, or to touch Git.
2. **Ultra performance & memory efficiency.** Sparse storage, packed integer cell keys, interned strings, compiled rules, streaming. Track budgets (ROADMAP §8); regressions fail CI.
3. **Deterministic & directly testable.** Every feature testable at its layer; a server-wide deterministic mode (injected clock, seeded RNG/IDs, fixed hash seed, ordered iteration). Same inputs → identical outputs. No flaky tests.

## Repository layout
- `crates/` — the Rust workspace:
  - `epiphany-determinism` — deterministic primitives (clock, RNG, ids). **The determinism harness.**
  - `epiphany-core` — multidimensional model: dims, cubes, sparse cell store, sandboxes, model-as-code (Phase 1).
  - `epiphany-calc` — rules + sparse feeds + auto-feeder inference + provenance (Phase 4).
  - `epiphany-mdx` — MDX parser/evaluator for subsets & cellsets (Phase 3).
  - `epiphany-flow` — **Flows**: TypeScript ETL/automation, data sources, scheduler (Phase 5).
  - `epiphany-security` — users/groups, object & element authz (Phase 7).
  - `epiphany-persist` — durability: transaction log + snapshots + recovery (Phase 1/8).
  - `epiphany-api` — REST + WebSocket surface, Axum (Phase 2).
  - `epiphany-server` — the daemon / composition root.
- `web/` — React + TypeScript client (Vite).
- `docs/` — ROADMAP, best-practices, and `docs/adr/` (architecture decisions).

## Build / test / run
Rust (from repo root):
- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Format check: `cargo fmt --all -- --check`  (fix with `cargo fmt --all`)
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Run server: `cargo run -p epiphany-server`

Web (from `web/`):
- Install: `npm ci` (first time: `npm install`)
- Dev server: `npm run dev`
- Typecheck: `npm run typecheck`
- Lint: `npm run lint`
- Build: `npm run build`

CI (`.github/workflows/ci.yml`) runs all of the above and gates merges.

## Local toolchain (Windows dev machine)
- **Rust**: rustup, default toolchain `stable-x86_64-pc-windows-gnu` (GNU — avoids needing Visual Studio C++ Build Tools). `cargo` lives at `%USERPROFILE%\.cargo\bin`.
- **Node**: managed by **fnm**; version pinned in `.node-version`. In a fresh PowerShell, expose `node`/`npm` with:
  ```powershell
  fnm env --shell powershell | Out-String | Invoke-Expression; fnm use
  ```
  (the concrete install is under `%APPDATA%\fnm\node-versions\<ver>\installation`).
- **git** is on PATH.

## Conventions
- Edition 2021; `unsafe_code = "deny"` workspace-wide — justify any per-crate opt-out in an ADR.
- **Determinism:** never call the wall clock, an RNG, or unordered iteration in logic — take a `Clock`/RNG from `epiphany-determinism`, and enforce stable ordering wherever output is observable.
- No secrets in source; copy `.env.example` to `.env` for local config.
- **Naming:** do not reference IBM or TM1 (or related product/feature brand names) in docs or code — describe capabilities generically. MDX and OData are open standards and are fine to name.
- Small, focused PRs (RG-01); update docs/ADRs when behavior or architecture changes (RG-08/16).
- Each phase closes with a **green, non-flaky deterministic acceptance suite** proving its definition of done.

## Status
**Phase 0 (foundations) complete:** workspace, web app, CI, ADRs, determinism harness. Next: **Phase 1** — core model + persistence + model-as-code. See the [roadmap](docs/ROADMAP.md).
