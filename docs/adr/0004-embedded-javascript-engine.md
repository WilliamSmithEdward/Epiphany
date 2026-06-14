# ADR-0004: Embedded JavaScript engine and TypeScript flows

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Epiphany maintainers
- **Phase:** 5 (flows: ETL and automation)

## Context

Flows are TypeScript ETL/automation scripts that build dimension members and
load cell values. Running them needs an embedded script engine inside the
single Rust binary. Three forces shape the choice: it must build and link on the
project's toolchain and ship in one static binary; it must be made fully
deterministic (the determinism mandate, ADR-0009); and the script layer only
orchestrates while native Rust does the bulk work, so raw script throughput is
secondary. The roadmap named QuickJS, V8, and WASM as candidates, to be settled
with a spike. A second, related decision is how TypeScript becomes runnable
JavaScript.

## Decision

**1. Embed boa (a pure-Rust JavaScript engine).** A build spike compared the
candidates on this project's constraints:

- **boa** (pure Rust): builds with no C step and no extra linker setup, links
  into the single binary trivially, and is fully controllable for determinism.
  A spike confirmed it compiles on the local `x86_64-pc-windows-gnu` toolchain
  in seconds, runs the orchestration model with Rust host callbacks, and lets
  the host delete the wall-clock global and override the RNG. ~108 transitive
  crates, all permissively licensed.
- **QuickJS (via rquickjs):** small and fast, but compiles bundled C through the
  `cc` crate, adding a C-build step and toolchain risk on the GNU target that
  boa avoids entirely; not pure Rust.
- **V8 (via deno_core / rusty_v8):** heavyweight, relies on prebuilt binaries
  oriented to MSVC, and does not fit the GNU + single-static-binary goal.
- **WASM (via wasmtime):** would require ahead-of-time compiling TypeScript to
  WebAssembly, the wrong authoring ergonomics for "write a TypeScript flow."

boa wins on every constraint that matters here; the only cost (raw JS speed) is
the one that does not, because host functions are vectorized and do the heavy
lifting. The engine sits behind the crate's own `run_flow` boundary, so a faster
backend could replace it later without changing callers.

**2. TypeScript is stripped to JavaScript in-house, not via swc/oxc.** boa runs
JavaScript, so type syntax must be removed before running. A spike showed the
established Rust transpilers pull a disproportionate dependency tree for this
job (oxc resolves to ~350 crates, including a forked React compiler; swc is
comparable), which clashes with the project's deliberate dependency discipline
(the same discipline that kept `insta`/`proptest` out, RG-10). Instead,
`epiphany-flow::strip` is a focused, dependency-free type stripper in the spirit
of the hand-written MDX and rules lexers. It is conservative and **fail-loud**: a
construct it cannot confidently classify is left untouched, so the worst case is
a parse error pointing at the line, never silently corrupted logic. Stripped
regions are blanked to spaces (newlines preserved), so the JavaScript keeps the
TypeScript's line layout and errors map back to source. Type *checking* is the
editor's job (a shipped `.d.ts`); the supported subset is documented in the
module. This is the same "honest scope, documented deferrals" approach the MDX
and rules layers took.

**3. Determinism and sandboxing.** Each run uses a fresh boa context with the
nondeterministic globals removed (the wall-clock constructor deleted, the RNG
made to throw); `ctx.now()` returns the injected clock (ADR-0009). The host
exposes only the flow API, so there is no filesystem or network reach by
default (capability-gated by absence). A loop-iteration and recursion budget
caps runaway scripts deterministically, instead of a wall-clock timeout (which
would be nondeterministic). A flow re-run on the same input is therefore
byte-identical. Numeric cell values cross the boundary as exact decimal strings,
never `f64`, preserving the fixed-point model (ADR-0008).

**4. The flow runner is pure with respect to the model.** `epiphany-flow`
depends on `epiphany-core` (and determinism) only. `run_flow` returns a
`FlowOutcome` (staged elements, edges, and name-addressed cells) plus a report;
it never touches the engine. The API composition layer applies the outcome
through the engine: append elements/edges first (so new members exist), then
resolve and write the cells. This keeps the engine flow-free, makes a flow
unit-testable without a server, and is the same seam philosophy as the calc and
MDX layers.

## Alternatives considered

- **QuickJS / V8 / WASM** for the engine: rejected as above (C-build risk, MSVC
  prebuilts, or wrong ergonomics); boa dominates on this project's constraints.
- **swc or oxc** for transpilation: rejected on dependency cost (hundreds of
  crates for type stripping) against a project that has consistently refused
  large dependencies; the in-house stripper adds zero and fits the existing
  lexer pattern. The trade is a documented, constrained TypeScript subset.
- **Authoring flows in plain JavaScript** (no TypeScript): rejected; typed flows
  with editor type-checking are the committed experience, and stripping is a
  small, well-tested component.
- **A wall-clock execution timeout:** rejected for determinism; the
  iteration/recursion budget bounds runaway scripts reproducibly.

## Consequences

- Flows build into the one static binary with no C toolchain, run
  deterministically, and cannot reach the host beyond the exposed API.
- The supported TypeScript subset is bounded and documented; exotic constructs
  (enums, namespaces, decorators, inline object/function types in annotation
  position, imports) are rejected or unsupported, recorded in
  `crates/epiphany-flow/src/strip.rs` and `docs/ROADMAP.md` (Phase 5). Full
  server-side type-checking and a richer transpiler are forward-compatible
  upgrades behind the same `strip`/`run_flow` boundary.
- The SQL data source and the job scheduler are deferred (a DB driver
  dependency and Phase 8 respectively); CSV, cube-view-as-rows, and source-less
  flows cover the M5 definition of done. Memory caps are best-effort (the
  iteration budget bounds CPU; a hard heap cap is a later enhancement).
- Realized in M5 as `epiphany-flow` (`strip`, `run`, `csv`, `testing`) plus the
  core element-growth path (`Cube::extend_schema`) and the API flow surface;
  gated end to end by `epiphany-api/tests/m5_acceptance.rs`.
