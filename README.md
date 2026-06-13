# Epiphany

A self-hostable, in-memory **multidimensional OLAP server** with a clean **REST API** and a **React + TypeScript** web UI. It provides cubes, weighted consolidations, a rules-and-feeds calculation engine, MDX, TypeScript "flows" for ETL, what-if sandboxes, and a fast pivot grid with write-back.

> **Status: Phase 1 (in progress).** Greenfield and under active construction. See the [roadmap](docs/ROADMAP.md).

## What makes it different
- **Model-as-code:** the whole model is human-readable, Git-versionable text.
- **TypeScript flows** for ETL and automation, instead of a proprietary scripting language.
- **Automatic feeder inference** and **calculation provenance**, so you can ask why a cell holds the value it does.
- Three hard requirements shape the design: it stays deterministic and directly testable, it is fast and memory-efficient, and it stays simple for the people who use it.

## Quickstart
Prerequisites: Rust (stable) and Node (the version in [`.node-version`](.node-version)).

```sh
# Rust engine (from the repo root)
cargo test --workspace
cargo run -p epiphany-server

# Web client
cd web
npm ci
npm run dev
```

## Layout and docs
- Architecture, conventions, and commands: [AGENTS.md](AGENTS.md)
- Plan of record: [docs/ROADMAP.md](docs/ROADMAP.md)
- Architecture decisions: [docs/adr/](docs/adr/)
- Engineering practices: [docs/agentic_ai_programming_best_practices.md](docs/agentic_ai_programming_best_practices.md)

## License
To be decided. See [docs/ROADMAP.md](docs/ROADMAP.md), section 12.
