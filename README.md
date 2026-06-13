# Epiphany

A self-hostable, in-memory **multidimensional OLAP server** with a clean **REST API** and a **React + TypeScript** web UI — cubes, weighted consolidations, a rules-and-feeds calculation engine, MDX, TypeScript "flows" for ETL, what-if sandboxes, and a fast pivot grid with write-back.

> **Status: Phase 0 (foundations).** Greenfield and under active construction — see the [roadmap](docs/ROADMAP.md).

## What makes it different
- **Model-as-code** — the whole model is human-readable, Git-versionable text.
- **TypeScript flows** for ETL/automation (not a proprietary scripting DSL).
- **Automatic feeder inference** + **calculation provenance** ("why is this cell this value?").
- **Deterministic & directly testable** by construction; **ultra performance / memory efficiency**; **dead-simple** UX.

## Quickstart
Prerequisites: Rust (stable) and Node (version in [`.node-version`](.node-version)).

```sh
# Rust engine (from repo root)
cargo test --workspace
cargo run -p epiphany-server

# Web client
cd web
npm ci
npm run dev
```

## Layout & docs
- Architecture, mandates, and commands: [AGENTS.md](AGENTS.md)
- Plan of record: [docs/ROADMAP.md](docs/ROADMAP.md)
- Architecture decisions: [docs/adr/](docs/adr/)
- Engineering practices: [docs/agentic_ai_programming_best_practices.md](docs/agentic_ai_programming_best_practices.md)

## License
TBD — see [docs/ROADMAP.md](docs/ROADMAP.md) §12 (open question).
