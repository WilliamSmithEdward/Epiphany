# Epiphany wide-net code review - 2026-07-02

20 scope-focused review agents over the full codebase (11 Rust crates, web/, excel-addin/), findings deduplicated but NOT independently verified - treat each as a reviewer claim to confirm before fixing.

Stats: 176 raw findings -> 160 after dedup. Severity: 11 critical / 68 major / 81 minor.

## Synthesis

# Epiphany Multi-Agent Code Review — Synthesis

## 1. Executive Health Summary

Epiphany's foundations are reported as unusually strong: exact fixed-point i64/i128 arithmetic makes consolidation order-independent, MVCC snapshot reads are lock-free, the WAL happy path is correctly framed/fsynced/torn-tail-safe, determinism discipline (BTreeMaps, injected clocks, canonical serialization) is pervasive, and the numeric core is arguably better than incumbent f64-based OLAP engines. The dominant failure pattern is that the **API composition layer defeats the core it composes**: the production resolver (`crates/epiphany-api/src/calc_factory.rs`) densely enumerates the cartesian leaf product on every consolidated read, discards the per-query memo, ignores the pinned MVCC snapshot, and silently drops rules that fail to compile — so the shipped calculation engine is reported as not credible beyond toy models despite a passing test suite. Durability is sound on the happy path but reviewers found the error lanes diverge acked state from disk (WAL append failures poison the log; the shared-dimension registry can be wholesale deleted after a swallowed load error). Security enforcement is fail-closed in the security crate but fail-open at several consuming seams (flow reads, cross-cube rules, WebSocket fanout, cube-scoped dimension edits). Multiple single-request denial-of-service vectors exist (uncapped cellset crossjoins, MDX Descendants stack overflow, feeder/explain dense materialization). The web client and Excel add-in are disciplined but each misses a headline ADR deliverable (persona gating, read coalescing). Roughly a dozen reported issues are critical, but most fixes are localized — the architecture itself is not in question.

## 2. Top 10 Issues (Priority Order)

1. **Every production consolidated read densely enumerates the leaf cartesian product; feeders never wired** — `crates/epiphany-calc/src/eval.rs:279` / `crates/epiphany-core/src/cube.rs:1220` — a single "Total" read on a modest sparse cube becomes ~10^8–10^12 recursive calls (with an unchecked usize product that wraps in release), making the product unusable at real cube sizes while ADR-0005's `consolidate_fed` has zero production call sites.
2. **Rules that stop compiling after a model edit are silently dropped, zeroing every rule-derived cell** — `crates/epiphany-api/src/calc_factory.rs:93` — an element delete/rename via the dimension API (never re-validated) makes ALL of a cube's rules vanish from reads with no error, diagnostic, or log: silently wrong numbers.
3. **Slug-colliding cube names silently overwrite another cube's snapshot and WAL** — `crates/epiphany-engine/src/lib.rs:1119` — the duplicate check is case-insensitive-only while on-disk identity is `slug(name)`, so creating "Sales?Plan" beside "Sales Plan" permanently destroys the existing cube's durable data via the ordinary API.
4. **WAL keeps appending after a partial append failure; recovery silently truncates later acknowledged commits** — `crates/epiphany-persist/src/store.rs:307` — a transient ENOSPC leaves a torn frame mid-log, days of subsequent fsynced/acked writes land after it, and the next restart discards and physically erases all of them.
5. **Swallowed registry load error plus orphan sweep can permanently delete every shared dimension** — `crates/epiphany-persist/src/registry.rs:72` + `crates/epiphany-engine/src/lib.rs:336` — `unwrap_or_default()` turns any load failure into an empty registry whose next save deletes all dimension bodies on disk; the sweep also runs before the new index is durable (registry.rs:63), creating exactly the failed-load state.
6. **Flow reads bypass cube-level security entirely (Flow:Write = read any cube)** — `crates/epiphany-api/src/flow_reader.rs:70` — only element masks are applied, so a standard "flow author" can read and exfiltrate cells from cubes they hold zero grants on, directly contradicting ADR-0023's no-privilege-escalation promise.
7. **Cube-scoped dimension edit fans out to all referencing cubes with weaker authorization** — `crates/epiphany-api/src/model_routes.rs:609` — a user with Dimension:Write on one cube can delete/convert shared-dimension members (and their stored cells) in every other referencing cube, bypassing the global gate and element-ACL checks the sibling `/dimensions/{id}/edit` route enforces.
8. **View cache key omits cross-cube rule dependencies: stale numbers served indefinitely** — `crates/epiphany-api/src/view_cache.rs:113` — a write to a referenced cube (e.g. FX rates) never invalidates cached cellsets of cubes whose rules read it, so acknowledged writes are invisibly absent from views with no TTL backstop.
9. **CalcFactory ignores the pinned snapshot, breaking MVCC isolation and poisoning the version-keyed cache** — `crates/epiphany-api/src/calc_factory.rs:224` — values come from a fresh engine re-snapshot while axes and the cache key come from the pinned version, so torn version-mislabeled cellsets get cached and served to every reader of that version.
10. **No cardinality cap on cellset execution: one authenticated read OOMs the whole server** — `crates/epiphany-api/src/query_routes.rs:709` / `crates/epiphany-core/src/query.rs:599` — crossjoin tuples are eagerly materialized with no bound (unlike spread's 200k cap), so a three-dimension crossjoin view allocates tens of GB and takes down the in-memory server for all users.

Near-misses worth flagging: MDX `Descendants` unbounded recursion aborts the process on deep hierarchies (`crates/epiphany-mdx/src/eval.rs:194`, critical); consolidation diamond tie-break depends on edge-declaration order that serialization doesn't preserve, so consolidated values can change across a restart (`crates/epiphany-core/src/dimension.rs:154`); commit versions restart at 0 each boot, exposing cross-restart ABA on the optimistic CAS and sandbox cache aliasing (`crates/epiphany-engine/src/lib.rs:306`, `view_cache.rs:106`).

## 3. Per-Subsystem Health

- **core** — Storage/packing math, weighted consolidation, and transactional edits are sound; hot-path allocation waste (cube.rs:1143), a string-cell over-drop on kind conversion (cube.rs:839), and quadratic `extend_schema` (cube.rs:966) are the debts.
- **calc** — Carefully engineered exact arithmetic and memoization, but the feeder soundness contract is violated for cross-cube/opaque-chained rules (feeders.rs:173) and Display/parse round-trip changes IF semantics (ast.rs:407).
- **mdx** — Small, deterministic, well-tested parser; one critical eval-time gap (unbounded Descendants recursion, eval.rs:194) plus semantic sharp edges (shallow path validation, no-op ASC/DESC B-forms).
- **flow** — Honest staged-outcome design, but the TS stripper breaks on arrow-function bodies and call-site generics (strip.rs:125/161), CSV input hygiene fails silently (BOM, bare CR), and the sandbox budget doesn't meter native builtins (run.rs:49).
- **persist** — WAL happy path is correct per ADR-0002; error lanes are not: post-failure appends poison the log (store.rs:251/307), Windows rename durability is asserted not ensured (store.rs:324), and kind-converting edits can brick boot (store.rs:708).
- **engine** — MVCC core is solid; seams leak: slug-collision data loss (lib.rs:1119), versions restart at 0 (lib.rs:306), best-effort unfsynced registry persistence (lib.rs:455), and restore_model can't undo durable WAL side effects (lib.rs:1189).
- **security** — The crate is fail-closed and ADR-faithful, but lifecycle hygiene is absent (grants never purged on delete, store.rs:445) and restriction-list element ACLs fail open on typos and tolerant loads (security_routes.rs:547, store.rs:731).
- **api** — Auth core (Argon2, sessions, lockout) is well built; WebSockets never re-validate sessions or filter events by cube security (ws.rs:45/62), Argon2 under the global mutex is an unauthenticated DoS (auth.rs:144), and the dimension-edit fan-out hole (model_routes.rs:609) is critical.
- **server/connect** — Connectors are careful (no shell, output caps, clean kills) but edge cases bite: grandchild pipe hang (lib.rs:166), 3xx-redirects-as-success (http.rs:56), post-materialization SQL row cap (sql.rs:191), and fail-open env parsing (config.rs:187).
- **web** — DTOs are drift-free and a11y is real, but every 401 is mislabeled "session expired" with no return-to-login (client.ts:149/133), the WS never reconnects (CubeApp.tsx:624), the pivot grid is unvirtualized (PivotGrid.tsx:652), and the ADR-0020 persona shell was never built.
- **excel-addin** — Faithful thin client, but WebView2 login fails outright on standard Office installs (ConfiguratorForm.cs:60), the async UDF identity key collides and omits sandbox/server (Functions.cs:39), and the promised read-coalescing layer doesn't exist (Functions.cs:44).

## 4. OLAP-Fidelity Verdict

Both OLAP lenses converge on the same split verdict. **The core model is credible and in places better than incumbents**: exact scaled-i64 cells with i128 weighted accumulation give order-independent, deterministic consolidation; spreading is remainder-exact; rule precedence, zero suppression (rows-then-columns on rule-aware values per ADR-0011), crossjoin ordering, and parallel grid fill (bit-identical across worker counts) are all correctly implemented; ADR-0039's diamond dedup is a documented, defensible departure from per-path summing. **The engine as deployed is not** — every serious problem sits at the calc/API seam and they compound: dense leaf enumeration on all consolidated reads with feeders unwired (the headline differentiator buys nothing at runtime), a fresh evaluator per cell forfeiting the ADR-0007 memo, snapshot/MVCC breakage poisoning the version-keyed cache, cross-cube cache staleness, and silent rule-set drops after structural edits. Semantics gaps compound the fidelity story: writes to rule-covered leaves are accepted-then-shadowed (incumbents reject them), leaf-only default rule scope silently sums ratios at every total (compiled.rs:72), one failing cell (DivByZero on a sparse denominator) aborts an entire cellset (query.rs:603), and cellsets have no string channel so text renders as editable numeric zeros. Individually these have narrow blast radii on toy models — which is why CI passes — but together they mean the shipped read path is not enterprise-credible until the seam fixes (mostly localized: thread the pinned snapshot, cache compiled rules per version, wire `consolidate_fed`/sparse fallback, per-cell error values) land.

## 5. Recurring Themes / Root Causes

1. **The composition layer defeats the crates.** Nearly every critical finding lives at a seam, not in a crate: `calc_factory.rs` alone ignores pinned snapshots, rebuilds engines per cell, recompiles all rules per request, and swallows compile failures — undoing MVCC, memoization, feeders, and rule validity that the underlying crates implement correctly. The benchmarks validating performance budgets use a resolver production never runs (`view_exec.rs`).
2. **Silent degradation instead of fail-loud on error paths.** `let _ =` and `unwrap_or_default()` convert I/O and parse failures into wrong data: empty registries that delete dimensions, empty CompiledModels that zero rule values, swallowed CSV BOM/CR/quote anomalies, 3xx-as-empty-rows, unrecognized `EPIPHANY_TLS` values disabling TLS. The codebase's stated fail-loud ethos is enforced in the cores and abandoned at the edges.
3. **Missing resource bounds on user-reachable paths.** Spread has a 200k cap; nothing else does — cellset crossjoins, dense consolidation, explain/feeder-diagnostics materialization, MDX Descendants recursion, flow native builtins, and SQL result buffering each let one authenticated request (or one flow) OOM, hang, or abort the process that holds all data in memory.
4. **Fail-closed crate, fail-open consumers.** The security crate resolves grants correctly, but enforcement seams skip it: flow reads and cross-cube rule/explain reads never check the referenced cube's grants, WS fanout broadcasts coordinates to everyone, element ACLs (a restriction list) fail open on typos/tolerant loads/deleted owners, and object lookups precede authorization in several mutating handlers.
5. **Acked-vs-durable divergence and restart discontinuities.** After any failed WAL append, checkpoint, or registry save, the server keeps serving while disk silently diverges from acknowledged state; commit versions and sandbox scope ids restart at 0 each boot (ABA, cache aliasing); and edge-declaration order — semantically load-bearing per ADR-0039 — is not what canonical serialization persists, so consolidated values can change across a plain restart.

## Completeness critique (what this review did NOT cover)

## Completeness Critique

### 1. Areas no scope plausibly covered

- **CI pipeline** — `F:/GitHub/Epiphany/.github/workflows/ci.yml`: release job builds with `--features embed-ui,tls,postgres,mysql`; cache key spans `target/` across OSes; artifact staging. No scope owns CI correctness.
- **Deploy configs** — `F:/GitHub/Epiphany/deploy/` (Dockerfile, `epiphany.service`, `com.epiphany.server.plist`). The Dockerfile builds with `embed-ui,tls` only, while CI release binaries add `postgres,mysql` — a feature drift nobody reviewed. The systemd unit has no sandboxing directives (`NoNewPrivileges`, `ProtectSystem`, etc.).
- **Supply-chain/build config** — `deny.toml`, `rust-toolchain.toml`, workspace `Cargo.toml`/`Cargo.lock`, `.node-version`. cargo-deny runs in CI but its allowlist/exceptions were not in any scope.
- **Docs drift** — `F:/GitHub/Epiphany/docs/` contains 39 ADRs plus DEPLOYMENT/PERFORMANCE/QUICK_START; no scope checked ADR-vs-implementation consistency.
- **Web build/entry files** — `web/vite.config.ts`, `web/eslint.config.js`, `web/index.html` (CSP/meta), `web/src/host.ts` (WebView2 postMessage bridge to the Excel add-in — a cross-boundary trust surface), `web/src/templates.ts`, `web/src/styles/`. These fall between "web-core" and "excel-addin".
- **Excel add-in packaging** — `excel-addin/Epiphany.ExcelAddIn.csproj` (dependency pins, signing) likely outside the 8 code findings.

(No `build.rs`, `.sql`, or shell scripts exist — nothing missed there.)

### 2. Suspiciously low finding counts

- **api-auth-http (5) and api-workspaces (4)** — `epiphany-api` is the largest crate (27 files, ~11.4k LOC) and the security boundary (ADR-0017/0018). 9 combined findings across two scopes for that surface suggests under-review.
- **web-core (7)** — non-component web code is ~3.1k LOC including a single 1,338-line `web/src/api/client.ts` plus 21 files under `web/src/ui/`; 7 findings is thin.
- **core-query-model (7)** — `epiphany-core` is ~8.9k LOC in only 9 files (very large files invite shallow passes).
- **web-components (11)** — 29 components / ~12.6k LOC including grid/pivot logic; 11 findings is a low density.
- **determinism-lens (4)** — the crate is tiny (171 LOC), so 4 may be fine, but if the lens was meant to span engine/calc determinism (ADR-0009), verify it actually did.

### 3. Follow-up review questions

1. Did any scope exercise the **feature-gated paths** (`tls`, `postgres`, `mysql`, `embed-ui` in `epiphany-server`) that are off by default but ship in release binaries?
2. Is the **Dockerfile/CI feature mismatch** (`postgres,mysql` missing from the Docker image) intentional and documented in DEPLOYMENT.md?
3. Is the **WebView2 host bridge** (`web/src/host.ts` + excel-addin `ConfiguratorForm`/`TokenStore`) validated on both sides — message origin, token handling?
4. Does `deny.toml` (48 lines) contain license/advisory exceptions that deserve scrutiny?
5. Do ADR-0019 (TLS) and ADR-0025 (at-rest encryption) still match implementation, given `TokenStore.cs` and the persist crate?
6. Was `web/src/api/client.ts` (1,338 LOC) explicitly in web-core's pass, or skipped as "generated-looking" glue?

## Lens assessments

### core-storage
crates/epiphany-core is in good shape overall: the packed-key layout math (bits_for, offsets, narrow<=64 gate, wide fallback, re-pack on bit-width growth) is correct and well tested, weighted consolidation is exact i128 arithmetic with the ADR-0039 diamond-dedup semantics implemented faithfully, structural edits are genuinely transactional via clone-and-swap, and the text serialization sorts everything it emits so canonical round-trips hold. I verified several suspicious areas (pack/unpack at the 64-bit boundary, delete/insert/reorder permutation remaps, edge idempotency, zero-write sparsity, alias reindexing) and found them sound.

The significant issues cluster in two places. Correctness: converting a string element back to numeric over-drops string cells that remain valid (silent data loss), and the dense-enumeration seam consolidate_with computes its cartesian size with an unchecked usize product that wraps in release builds (silently wrong rollups on very large leaf spaces). Performance, which the project mandates as a first-class requirement: the all-leaf fast path in get/consolidate_with/consolidate_fed allocates full per-dimension weight maps before it short-circuits, extend_schema's per-edge idempotency check re-materializes and sorts the whole edge list (quadratic batch growth on the runtime dimension-build path), and Cube::build applies attribute values one at a time while cloning the entire dimension per value. Remaining findings are genuine but smaller: stale sandbox overrides after kind conversions, a panic path in spread_leaves on an over-long coordinate, an alias-index divergence from persisted state, unbounded interner growth, and an undocumented iteration-order contract on cell_entries.

### core-query-model
crates/epiphany-core's query/serialization half is in strong shape overall: crossjoin ordering is deterministic (author order, first-spec-slowest), zero-suppression composes rows-then-columns correctly against the same resolver-supplied grid used for output (so rule-calculated values are honored by suppression), the parallel grid fill is genuinely bit-identical to serial with deterministic first-error semantics, Fixed's Display/FromStr are canonical inverses for all practically reachable values, and the TOML model-as-code layer achieves byte-identical parse-to-serialize fixed points with careful back-compat (legacy suppress_zeros, top_level pin, ADR-0035 automation lift-out). Sandbox coordinate remapping through the Model wrappers (reorder/insert/delete) is correct and injective.

The significant problems cluster at the seams. First, the production CellResolver implementation (CalcFactory) ignores the pinned snapshot it is handed and re-snapshots the whole engine, breaking MVCC read isolation and poisoning the version-keyed view cache under concurrent writes. Second, the Cellset carries no string channel at all, so string cells render as numeric 0 in every saved/ad-hoc/MDX view and zero-suppression silently deletes rows/columns whose only content is text. Third, index-stable structural edits (set-kind, leaf-to-consolidation promotion via reparent/add-child) strand sandbox overrides that then make the entire sandbox permanently uncommittable, with no per-override delete to recover. Additionally, view execution has no cardinality cap (a dense grid plus eagerly materialized crossjoin tuples make a single authenticated request an OOM vector), and canonical serialization over-sorts View.context, so observable ordering changes across a restart.

### calc
crates/epiphany-calc is carefully engineered overall: exact fixed-point arithmetic with pinned banker's rounding, per-query memo with sandbox scope partitioning, sound Computing-marker cycle detection, deterministic BTreeSet/sorted outputs, and first-match-in-author-order precedence that is well tested. The dense consolidation on the rules read path is a deliberate ADR-0005/0007 decision and was not reported. The significant problems cluster at three seams: (1) the feeder-inference soundness contract is violated for rules mixing same-cube and cross-cube inputs additively, and for rules chained downstream of opaque rules - both confirmed empirically to produce under-feed that the module and ADR-0005 explicitly promise cannot happen for analyzable rules; (2) the "canonical Display that round-trips through parse" contract is broken with a semantics-changing divergence (confirmed: (IF c THEN 1 ELSE 2) + 3 reparses as IF c THEN 1 ELSE (2+3)); and (3) at the API seam, PinnedRegistry::build silently degrades a cube to rule-less when its rules stop compiling after a model edit (element delete/rename is reachable via the dimension-edit API with no rule revalidation), silently zeroing every rule-derived number on the read path.

One further observation outside my scope but worth routing to the epiphany-api reviewer: calc_factory.rs's CalcCellResolver::value builds a fresh CalcEngine (and empty memo) per individual cell read, and PinnedRegistry::build re-parses and re-compiles every cube's rules on every resolver construction (per request). This discards exactly the per-query memoization eval.rs is designed around ("a value is computed at most once per query") and multiplies the dense-consolidation cost by the number of cells in a view; combined with compile()'s O(V*E) descendants() walk it makes rule compilation a per-request cost despite the compiled module documenting "compile once per published model version".

### mdx
crates/epiphany-mdx is a small, well-factored crate: the hand-written lexer handles doubled-delimiter escapes correctly ([a]]b], "" and '' all verified by tests), bracketed names are correctly exempt from keyword treatment, keywords are consistently case-insensitive, AND/OR/NOT precedence is standard, infix `*` and N-ary Crossjoin agree on left associativity, and parser recursion is properly guarded by a shared depth counter across set and predicate nesting. Determinism is taken seriously: every HashSet is used only for membership, with emission order always coming from definition order, canonically sorted edges, or a stable position tie-break. Error spans are generally accurate, including EOF spans and the duplicate-axis span captured before the bump.

The one serious gap is the evaluator's data-driven recursion in Descendants: the crate defends carefully against hostile nesting in the parser but not against hostile (or accidental) hierarchy depth at eval time, where an overflow aborts the whole server. The remaining findings are semantic sharp edges - shallow member-path validation, the <> vs NOT= divergence on missing attributes, ORDER's B-forms being no-ops, a doc/impl mismatch on axis-less SELECT, and error diagnostics pointing at the wrong token for bare-attribute predicates - all worth fixing but none affecting computed numbers on well-formed models.

### flow
crates/epiphany-flow is a well-factored crate with an unusually honest internal design (staged-outcome purity, injected clock/reader, deterministic dedup, ordered maps everywhere observable). The scheduler and ledger are solid; FlowOutcome application order (elements/edges before cells, dimensions before cubes) is correct in both the test runner and the API layer. The two weakest points are (1) the TypeScript stripper, whose "conservative fail-loud" contract is broken in practice: the single most common TS idiom — an arrow function with a block body — is misclassified as an object literal so annotations inside it are never stripped, and call-site generic arguments pass through and re-parse as comparison chains (sometimes silently); and (2) input hygiene at the edges: UTF-8 BOM, bare CR, stray quotes, and non-string JSON coordinate members are all silently swallowed rather than surfaced, which in an OLAP loader translates into wrong or missing numbers with no error.

The sandbox itself is genuinely deterministic (Date deleted, not shadowed; Math.random unrecoverable; reads pinned through an injected reader), but the documented "budget" only bounds interpreter loop iterations and recursion — native builtins (string repeat, Array.fill, regress regex backtracking) and the log vector are unmetered, so a buggy or hostile flow can still exhaust the memory/CPU of the process that holds the entire in-memory model, which is exactly the failure mode the MAX_STAGED comment claims to have excluded.

### persist
The WAL core in crates/epiphany-persist is carefully built: CRC32-framed records, torn-tail detection, all-or-nothing batch framing, fsync-per-write by default, temp-then-rename snapshots, and checkpoint ordering (snapshot durable before WAL clear) are all correct on the happy path and well tested, matching ADR-0002. The weaknesses are at the edges. First, the sibling stores do not share the WAL's discipline: the shared-dimension registry and automation store perform write-temp-then-rename with no fsync at all, the registry deletes "orphan" dimension files before the new index is durable, and the engine swallows both load and save errors around them - a chain that can end in permanent deletion of every shared dimension. Second, error paths break the invariants recovery relies on: a failed WAL append leaves a partial frame mid-log while the store keeps appending acknowledged writes after it, and a kind-converting structural edit can leave a WAL whose replay is rejected, blocking the whole server from booting. Third, Windows durability is asserted rather than ensured: sync_dir is a documented no-op and std rename has no write-through, yet checkpoint durably truncates the WAL immediately after the un-flushed rename. Finally, the ADR-0037 slug collision guard checks only case-insensitive equality, so two coexisting-legal cube names that slug identically let create_cube silently overwrite another cube's snapshot and WAL.

The audit log (ADR-0010) mirrors the WAL framing correctly and is properly non-gating, fsynced per record, and retention-bounded via server config; remaining issues there are integrity-of-history smells (wholesale destruction on a corrupt header, sequence restart after an empty compaction) rather than durability bugs. ADR-0025's at-rest posture (no in-binary crypto, owner-only secret files) is implemented as decided.

### engine
The MVCC core of crates/epiphany-engine is well built: lock-free arc-swap snapshot reads, a serialized per-cube writer that validates and durably logs before publishing, an optimistic base-version CAS, and a correct restore-from-published rollback for failed definition ops (with a regression test). The concurrent-reader stress test and the dim_topology -> writer lock-order discipline are solid. The documented best-effort multi-cube fan-out (ADR-0036 deferred hardening) and whole-model clone-per-commit (ADR-0001 M2 cut) were treated as deliberate and not reported.

The significant risks are at the seams the module docs gloss over: (1) cube creation's duplicate check uses case-insensitive name equality while the on-disk identity is slug(name), so distinct names that slug identically silently clobber another cube's snapshot and WAL - real data loss; (2) the store keeps appending to the WAL after a partial append failure, and recovery truncates at the first torn frame, silently deleting later acknowledged commits; (3) restore_model cannot un-happen durable side effects, so a failed sandbox-commit checkpoint leaves an aborted batch in the WAL that a crash resurrects; and (4) commit versions restart at zero each boot, so the optimistic CAS and every version-keyed cache are exposed to cross-restart ABA. The remaining findings are smaller consistency and robustness debts (wrong CommitOutcome returned from fan-out edits, rules TOCTOU, poisoned-mutex panics, post-commit unordered change-feed broadcasts, a global lock held across serial disk checkpoints, and best-effort registry persistence that can silently lose an unreferenced dimension).

### security-crate
The epiphany-security crate itself is small, deterministic, and largely faithful to its ADRs: fail-closed per-kind grants (ADR-0023), max-wins resolution over sorted maps, admin bypass, per-request re-resolution, byte-stable serialization. Two things I deliberately did NOT report: absence of explicit deny (ADR-0016's deny tiers were consciously removed by ADR-0023) and case-sensitive principal/object names (exact-match is applied consistently everywhere — core has no case-folding — so there is no normalization mismatch to exploit, though it feeds the validate-on-grant gap below).

The real authorization risks are at the enforcement seams that consume this crate. The two worst: TypeScript flow runs can read any cube with no cube-level gate (only element masks), making the ADR-0023 "flow author" role a read-everything role despite the ADR's explicit "a flow is never a privilege-escalation path" promise (writes were gated as promised, reads were not); and the calc element mask is scoped to the queried cube's ordinal, so cross-cube rule references pull referenced-cube values with neither the reader's cube grant nor their element ACLs applied. Lifecycle hygiene is the other theme: grants and element ACLs are never purged when users/groups/cubes are deleted, so name reuse resurrects old permissions; tolerant load and the unvalidated element-ACL grant endpoint both fail open because element security is a restriction list (a dropped or dangling row widens access, unlike ordinary grants where it narrows).

### api-auth-http
The authentication core in crates/epiphany-api/src is well engineered: session tokens are 32 bytes of OsRng (never the deterministic RNG), base64url-encoded, stored server-side with absolute TTL plus optional idle expiry on the injected clock; login runs a dummy Argon2 verify for unknown users (no enumeration timing channel) and returns a uniform "invalid credentials" 401; the per-username lockout is checked before Argon2; must_change_password is enforced fail-closed in the one AuthPrincipal extractor; password change/admin reset/user delete all revoke sessions; secrets and password hashes are never serialized into responses; connection detail is redacted for non-admins; ADR-0018 headers and the 8 MiB body cap are applied in build_router so tests exercise them (the missing request-timeout layer is an explicitly documented ADR-0018 deferral, not reported). Every protected handler takes the AuthPrincipal extractor - I found no unauthenticated route besides the documented /healthz, /api/v1/openapi.json, and login.

The real gaps are (1) the WebSocket channel: authentication and privileges are evaluated only at upgrade time, so revoked/expired sessions and demoted admins keep live event streams, and the change-event fan-out ignores cube-level and element-level security entirely (only sandbox ownership is filtered), leaking cube names and changed-cell coordinates to any authenticated user; and (2) availability: Argon2 verification runs synchronously on a tokio worker while holding the global security mutex that every request's authorization gate also needs, making a handful of concurrent bogus logins (rotating usernames to sidestep the per-username lockout) an effective whole-API denial of service.

### api-model-endpoints
The API layer is generally well-structured: authorization is centralized (authz.rs), writes funnel through the engine's atomic per-cube batch commit, numeric values travel as decimal strings end-to-end (no f64 drift), consolidated-cell writes are rejected with a typed 422, and the view cache is keyed losslessly on version + shape + sandbox scope + exact deny set, so no cross-user cache leakage was found. Spread arithmetic is exact and deterministic with a hard 200k-leaf cap.

The two biggest problems are in the shared-dimension fan-out surface and in result-size bounding. The cube-scoped structural-edit endpoint reaches the same registry-wide fan-out as the global endpoint but with a strictly weaker guard (per-cube Dimension:Write, element-restriction checked on one cube instead of all referencing cubes), which is a cross-cube privilege-escalation and data-destruction hole. Cellset execution has no size cap at all (unlike spreading), so any authenticated reader can OOM the server with a crossjoin. Secondary issues: name-to-index resolution races against ADR-0036 index remaps on the versionless write paths; an i64 overflow in proportional spread with near-cancelling weights; and a handful of guard-ordering/existence-leak inconsistencies where mutation handlers look objects up before authorizing.

### api-workspaces
Owner+visibility enforcement on subsets/views is sound: can_read (public OR owner OR admin) gates list/get/members, and can_modify (owner OR admin) gates replace/delete, so user A cannot read or mutate user B's private subset/view by id. Element-security suppression is correctly applied to member/preview/cellset enumerations, and the rule-test/feeder/flow-test diagnostics fail closed for element-restricted (or non-admin, for global flow tests) callers. The secret store is write-only (no value echo in GET, preview, or DTOs), and command/HTTP/SQL connectors are double-gated (definition + fetch re-check the enable flag and host allowlist; redirects disabled for SSRF). The significant gap is at the object-authorization layer of the flow subsystem: flow live reads (and, secondarily, connection fetches) enforce only element security, never the cube/connection object grant, so a Flow:Write holder can read cube data they have no Cube:Read grant on - a confidentiality breach contradicting ADR-0035 decision 7. explain_cell has a parallel cross-cube leak. These, plus a minor existence-disclosure ordering bug on replace/delete, are the findings worth acting on."}

### server-connect
The three scoped crates are carefully engineered overall: the command connector spawns a fixed argv with no shell, drains stdout/stderr on threads to avoid pipe deadlock, caps output correctly (cap enforced during read, not after), and kills+waits on timeout so no zombie is left; config defaults are loopback-only with a warning on non-loopback plain HTTP; self-signed cert generation uses rcgen/ring OS randomness (I verified EPIPHANY_DETERMINISTIC is never read anywhere - there is no deterministic-key path, only a stale mention in .env.example); the determinism crate (SplitMix64, ManualClock, IdGen) is clean and unbiased. ADR-0012/0019/0025 deliberate decisions (fallback-to-HTTP on featureless builds, LocalSystem service default, Windows ACL posture) were checked against docs before evaluating.

The significant issues cluster in the connector edge cases: a successful child exit can still hang the run forever if a grandchild inherits the stdout pipe (the join has no deadline); the HTTP connector's no-redirect policy silently converts a 301/302 into a successful zero-row fetch (a data-correctness trap since http->https redirects are ubiquitous); a byte-index String::truncate on a remote-controlled error body can panic; and the SQL connectors enforce their row cap only after fully materializing the result set in memory, defeating the stated memory-exhaustion guard. Config parsing has a fail-open wrinkle (unrecognized EPIPHANY_TLS values silently disable TLS) and silently ignores malformed values including EPIPHANY_BIND=localhost:8080.

### olap-core-semantics
The numeric core is genuinely strong by OLAP standards: exact scaled-i64 cells with i128 weighted accumulation make consolidation order-independent and deterministic (better than the incumbent f64 norm), rule arithmetic pins round-half-to-even, spreading is remainder-exact and total-preserving, sandbox zero overrides are kept as explicit entries, and batch/sandbox-merge commits are validated all-or-nothing on a clone before an atomic publish. Rule precedence (first matching area in source order) and explicit consolidation overrides are honored consistently across the evaluator, views, and explain. The ADR-0039 dedupe of diamond rollups is a deliberate, documented departure from the textbook per-path sum; its side effect - a parent that is visibly not the sum of its displayed children when children overlap, with same-depth weight ties resolved by edge-declaration order - is worth surfacing to users but is not a bug per the ADR.

The serious problems are all at the calc/API seam, and they compound: the production server unconditionally injects the rule-aware resolver, whose consolidated reads always densely enumerate the cartesian leaf product (feeders and consolidate_fed are never wired into any read path, so the headline feeder-inference differentiator currently buys nothing at runtime); each output cell gets a fresh evaluator, so the ADR-0007 per-query memo never spans a cellset; the resolver ignores the pinned snapshot and re-snapshots the live engine, breaking MVCC read isolation and poisoning the version-keyed view cache; and a rule set that stops compiling after a structural edit is silently treated as rule-less, reverting every derived cell to stored/aggregated values with no signal. Separately, writes to rule-covered leaves are accepted but shadowed (incumbent engines reject them), the leaf-only default scope of unconstrained rule areas silently sums ratio measures at every total, and one failing cell (e.g. division by an empty cell) aborts an entire cellset. Individually most of these have narrow blast radii on toy models - which is why the test suite passes - but together they mean the calculation engine as deployed is not yet credible at enterprise cube sizes.

### olap-query-semantics
The query core is in strong shape: axis resolution, crossjoin tuple order (first-spec-slowest), and zero suppression (judged on calculated, rule-aware values; rows first over all columns, then columns over surviving rows, exactly as ADR-0011 decision 3 specifies) are correctly and deterministically implemented, and empty axes/all-suppressed grids are handled as valid empty cellsets. Parallel aggregation (ADR-0028 Stage B) is genuinely determinism-safe by construction — disjoint row-band slot writes, unchanged within-cell i128 reduction, deterministic lowest-ordinal error selection — and is pinned by a serial-vs-parallel bit-equality test across worker counts {2,3,5,7}. The MDX sublanguage is a small, hand-written, deterministic subset with clearly documented deliberate deviations from the standard (sets are de-duplicated everywhere, ASC/DESC behave as flat BASC/BDESC sorts, no Hierarchize); those are defensible product choices for a subset-as-selection model, though `.Children`/`.Descendants` silently discarding the authored rollup order is a fidelity gap worth fixing.

The risk concentrates at the API seam where the view cache and the rule-aware resolver meet MVCC. The cache key (cube, version, shape, sandbox scope, mask denied-set) is well designed for single-cube dependencies and correctly self-invalidates on cell writes, model/rule/subset edits, sandbox writes, and (indirectly, via fan-out commits) global-dimension edits; element-security changes are safe because the effective denied-pair set is itself the key. But it is blind to cross-cube rule dependencies (stale numbers served indefinitely after a referenced cube changes), the resolver factory re-snapshots the world instead of using the pinned snapshot (torn reads and version-mislabeled cache entries under concurrent writes), and the sandbox scope id is not unique across restarts (cross-sandbox cache aliasing). Separately, the shipped resolver builds a fresh CalcEngine per cell, forfeiting the per-shard memo reuse ADR-0028 decision 9 promised. These are seam bugs, not core-model bugs — the fixes are localized (key on all pinned versions or a global version, thread the pinned snapshot into the registry, make scope ids restart-unique, cache the engine per worker).

### determinism-lens
Epiphany's determinism discipline is exceptional for a codebase of this size: BTreeMap/BTreeSet at every API, security, engine, and persist seam; canonical sorted model-as-code serialization; an injected Clock/IdGen used consistently (the only SystemTime/Instant reads are the production SystemClock, benches, and the external-command timeout in epiphany-connect); fixed-point i64 arithmetic that makes reduction order irrelevant; a parallel view fill (query.rs fill_grid_parallel) that writes disjoint bands and deterministically reports the lowest-row-major-ordinal error; and a boa JS sandbox that deletes Date and traps Math.random. FxHash-keyed cell stores iterate deterministically for a given insertion sequence, and every consumer of cell_entries() either sorts or accumulates order-independently. Session tokens and argon2 salts use OsRng deliberately (security over reproducibility; neither is asserted-on or format-affecting).

The leaks that remain are at seams the test harness does not byte-compare. The most significant is semantic: ADR-0039 makes consolidation weights for equal-depth multi-parent diamonds depend on edge-declaration order, but ADR-0003's canonical serialization sorts edges by (parent, child) and the loader replays them in sorted order, so the tie-break — and therefore consolidated cell values — can silently change across a checkpoint/restart or export/import with zero user edits. The remaining findings are RandomState-HashMap iteration reaching durable bytes in the flow run ledger (crash-recovery appends and a retention tie-break) and filesystem read_dir order deciding slug-collision winners during boot migration.

### concurrency-durability-lens
The happy-path durability story is genuinely sound: every commit is validated on a clone, framed as one WAL unit, fsynced before the new version is published or the HTTP response returns (ack-after-durable), torn tails and unterminated batches are discarded whole on recovery, checkpoints write snapshot-then-clear-WAL in the safe order, and replay is idempotent set-semantics so the rename/truncate crash window is harmless. The reindexing-edit "checkpoint before AND after" discipline in store.rs is careful work, and Engine::checkpoint takes the per-cube writer lock, so a snapshot can never be cut mid-commit.

The weaknesses are concentrated in the error and edge lanes, which is exactly where a durability layer earns its keep: (1) after any failed WAL append/fsync or failed checkpoint the store stays in service with the on-disk state silently diverged from the acked in-memory state — later acknowledged commits can be discarded at recovery or land on remapped coordinates; (2) commit versions restart at 0 on every boot, so optimistic base_version tokens are reusable across restarts (lost-update ABA); (3) the shared-dimension registry's persistence is best-effort and never fsynced despite ADR-0024 declaring it the fail-closed durable authority, and a corrupt/torn registry silently loads as empty; (4) the documented single-process assumption is unenforced — a second server on the same data dir corrupts silently rather than failing cleanly; and (5) all durable I/O (fsync, full-model checkpoint serialization) runs inline on tokio worker threads while holding a std::sync::Mutex.

### perf-lens
The core storage design is genuinely strong: packed u64 coordinate keys with a bare (u64, Fixed) hash entry, interned string cells, a dependency-free FxHash, MVCC lock-free reads, and a keyed view cache that follows ADR-0028 carefully. The serious performance problems live in the API composition layer, which quietly defeats the core's own optimizations on the production hot paths. The single biggest issue: the server always injects the rule-aware CalcFactory, and its CalcEngine consolidates every non-leaf read by densely enumerating the cartesian product of all descendant leaves (Cube::consolidate_with) instead of the sparse populated-cell scan (Cube::get) — even for cubes with zero rules — while the feeder machinery built to make that sparse (consolidate_fed, ADR-0005) has no production call site at all. Compounding it, the resolver builds a fresh CalcEngine (empty memo) per cell read — contradicting ADR-0028 decision 9's "within-shard memo reuse is kept" — and re-parses/re-compiles every cube's rules per request. Notably, the benchmarks that validated the section-8 view budgets (view_exec.rs) use a stored-cells resolver that production never uses, so the budget numbers do not measure the shipped read path.

On the write path, every batch commit deep-clones the whole cube once for validation (avoidable — validation is coordinate-local) on top of the ADR-0001-documented publish clone, making bulk load O(total cells) per batch rather than O(batch). Several smaller per-cell/per-request allocations round it out (memo keys, WS fanout re-serialization, per-cell name re-resolution in the DTO, MDX edge-list rebuilds). I did not re-report items the ADRs explicitly defer (TOML snapshots, per-what-if-write checkpoint, structural-sharing store) except where the deferral interacts with an undocumented cost.

### web-core
The web client layer is unusually well-documented and mostly type-accurate: I spot-checked a dozen hand-written DTOs (CellDto, CellsetDto, TraceDto with its flattened kind tag, RunDto, SandboxDto, AuditRecordDto, the error envelope, the WS ChangeEvent) against the Rust serde structs and found no drift; the editor templates in templates.ts match the rule parser and flow runtime exactly; model/tree.ts is deterministic, cycle-guarded, and unit-tested. Components (out of scope) generally guard stale responses with cancelled flags, and the SandboxBar disciplines the module-global sandbox header correctly.

The significant problems cluster around session handling in api/client.ts: a blanket "any 401 = session expired" rule in request() rewrites the server's "invalid credentials" and "current password is incorrect" messages into a false "Your session has expired" (and wipes the bearer token as a side effect), and when a session genuinely expires mid-use nothing tells App.tsx, so the user is left in a dead UI that errors on every action instead of being returned to the Login screen. Lesser items: a header-span grouping key in tree.ts that joins member names with a plain space (collidable, since names may contain spaces), an unused batchWrite that also forgoes the server's optimistic-concurrency base_version, App's bootstrap treating any network/5xx failure as "not signed in", and the absence of AbortSignal/timeout support in the fetch wrapper.

### web-components
The React component layer is unusually disciplined for its size: dirty-state guards funnel through a single navigate(), the explorer tree and PivotGrid both use per-request generation counters against response races, tuple keys are deliberately collision-free (U+0001/U+0002 separators), and a11y work (roving tabindex, live regions, focus-after-close) is real, not cosmetic. The significant risks cluster in four places: (1) CellsetGrid identifies cells positionally with uncontrolled inputs keyed by value, so a WebSocket-triggered re-run mid-edit can commit a user's typed number to the wrong coordinate; (2) the WebSocket never reconnects, so after one drop every grid silently serves stale data while the tooltip claims "reconnecting"; (3) the pivot grid — the product's defining surface per ADR-0020 — is fully unvirtualized and un-memoized, rendering and fetching the entire row×column cartesian product; and (4) the ADR-0020 persona-gated shell was never built: everything is gated on a single isAdmin flag, so business users get the full modeler chrome (Rules, Flows, Dimensions, raw MDX) that the accepted ADR says they must never see.

Smaller correctness issues (broken Shift-range selection in the ADR-0032 member table, the explain panel wiping the modeler's analysis on every remote cell write, un-guarded async effects) are individually modest but sit on the exact paths the mandates care about: large-model performance, multi-user liveness, and dead-simple UX for casual users.

### excel-addin
The add-in is architecturally faithful to ADR-0022's thin-client mandate: no engine logic, cube names URL-escaped, certificate validation untouched (no ServerCertificateCustomValidationCallback anywhere), the token DPAPI-encrypted at CurrentUser scope per the documented decision (null entropy is consistent with that ADR choice, since entropy embedded in the binary adds nothing), token never written to workbooks, and errors mapped to client-safe messages. The server's all-or-nothing batch semantics plus rank/element validation (verified in crates/epiphany-api/src/resolve.rs) provide a solid backstop for the write path.

The significant problems cluster in two places. First, in-Excel runtime behavior that CI cannot exercise: WebView2 initialized without a user-data folder will fail outright on standard Program Files Office installs, killing the entire login flow; and the commit path blocks Excel's UI thread on a 30s network call. Second, the async read path diverges from its own ADR: the promised per-recalc coalescing layer does not exist (one blocking HTTP POST and one parked ThreadPool thread per formula cell, a starvation trap at report scale), and the hand-rolled ExcelAsyncUtil.Run identity key concatenates without separators and omits sandbox/server, so distinct requests can collide and deliver the wrong number into a cell. The write-back path's silent fallbacks (guessed Value column, dropped blank coordinates, first-area-only multi-area reads) are mostly caught by server validation but should fail fast client-side for a tool whose one job is writing numbers into a system of record.

## All findings

### CRITICAL (11)

#### critical-1. Broken rules after model edits silently drop ALL rules for a cube on the read path
- **Where:** `crates/epiphany-api/src/calc_factory.rs:90`  |  **Category:** olap  |  **Found by:** calc

PinnedRegistry::build swallows rule parse/compile errors and substitutes an empty CompiledModel, so the cube evaluates as rule-less. Rules are validated only at define time, but dimension edits (element delete/rename via DimensionEdit, exposed by the dimension-edit API with no rule revalidation - grep of dimension_routes.rs shows none) can invalidate previously-valid rules afterwards. From the next read on, every rule-derived cell on that cube (not just cells touched by the broken rule - ALL rules are dropped) silently reverts to stored values, typically zero. Users see wrong numbers with no error anywhere; explain shows Stored/Consolidation as if no rules existed. This is exactly the stale-compiled-rules-after-model-edit hazard, and it is not covered by any ADR (ADR-0021 never mentions rules).

```
rules::parse(&s.rules().source)
    .ok()
    .and_then(|doc| compile(s.cube(), &cr, &doc, s.version()).ok())
    .unwrap_or(CompiledModel {
        version: s.version(),
        rules: Vec::new(),
    })
```

**Suggested fix:** Surface a compile failure instead of silently degrading: either fail reads on that cube with a clear RULE_COMPILE_ERROR, or keep serving but attach a loud model-health diagnostic. Additionally, revalidate rule sources in the dimension-edit handlers (compile_source already exists) and reject or warn on edits that break rules.

#### critical-2. Descendants evaluator recurses unbounded on hierarchy depth - stack overflow aborts the server
- **Where:** `crates/epiphany-mdx/src/eval.rs:194`  |  **Category:** bug  |  **Found by:** mdx

collect_descendants recurses once per hierarchy level with no depth guard. Core's add_child rejects cycles but places no limit on chain depth, so a dimension with a deep consolidation chain (tens of thousands of levels, buildable through the dimension-editing API or an ETL flow importing pathological parent-child data) makes any `.Descendants` / `Descendants(...)` evaluation overflow the thread stack. A Rust stack overflow aborts the whole process, so one query crashes the OLAP server for every user. The parser explicitly defends against exactly this class of input (MAX_PARSE_DEPTH / TooDeep at parser.rs:83), and core's own traversals (`reaches`, `leaf_weights` in dimension.rs) are deliberately iterative - the evaluator is the one data-driven traversal left recursive. Reachable from the HTTP surface via dynamic subsets (MdxEvaluator) and the MDX query endpoint.

```
fn collect_descendants(
    adjacency: &BTreeMap<u32, Vec<u32>>,
    node: u32,
    ...
    if let Some(children) = adjacency.get(&node) {
        for &child in children {
            collect_descendants(adjacency, child, seen, out);
```

**Suggested fix:** Rewrite collect_descendants iteratively with an explicit work stack (mirroring Dimension::reaches), or add a depth counter that returns a clean MdxEvalError past a fixed backstop.

#### critical-3. Slug-colliding cube names silently overwrite another cube's snapshot and WAL
- **Where:** `crates/epiphany-engine/src/lib.rs:1119`  |  **Category:** bug  |  **Found by:** persist, engine

ADR-0037 requires create_cube to reject any name 'that collides case-insensitively ... (or by slug-equivalent characters)' because both map to one folder. The guard only checks eq_ignore_ascii_case, but slug() (crates/epiphany-persist/src/slug.rs) maps every non-[a-z0-9-_] character to '-': "Sales Plan", "Sales-Plan", and "Sales?Plan" are all distinct under eq_ignore_ascii_case yet all slug to "sales-plan". Store::create is documented to replace what's there ('Any existing WAL in dir is replaced', store.rs:127) - write_snapshot renames over the live snapshot.model and open_fresh_wal truncates the live wal.log. So a user creating a cube whose name slugs like an existing cube's permanently destroys the existing cube's on-disk data through the ordinary API; both in-memory cubes then checkpoint over each other's files until restart, after which one cube's data is gone.

```
if let Some(existing) = self
    .cubes
    .load()
    .keys()
    .find(|k| k.eq_ignore_ascii_case(name))
{ ... }
// then:
let store = Store::create(cubes_dir.join(slug(name)), cube).map_err(BatchError::Persist)?;
```

**Suggested fix:** Compare slug(name) against slug(existing) for every existing cube (the check ADR-0037 actually specifies), and additionally harden Store::create to refuse (or error) when dir already contains a snapshot.model, so no future caller can clobber a live store.

#### critical-4. Swallowed registry load error plus orphan-file deletion can permanently delete every shared dimension
- **Where:** `crates/epiphany-persist/src/registry.rs:72`  |  **Category:** bug  |  **Found by:** persist

save_registry deletes every <id>.model file whose id is not in the entry set passed by the caller, trusting that set to be the complete registry. The engine loads the registry with `load_registry(&dir).unwrap_or_default()` (epiphany-engine/src/lib.rs:336), so ANY load failure - a corrupt index.toml, one missing/unreadable <id>.model body (see the deletion-ordering and missing-fsync findings, which both create exactly that state), or a transient permission/IO error - silently yields an EMPTY registry and the server boots normally. The next shared-dimension mutation then calls save_registry with that empty/partial set, whose orphan sweep deletes every previously stored dimension body from disk. Result: permanent, unlogged loss of the entire shared-dimension library (ADR-0024 primary data). save_registry's own result is also discarded at the engine (`let _ = save_registry(dir, &entries);`, lib.rs:455), so even the destructive rewrite failing is invisible.

```
if !keep.contains(&id) {
    let _ = std::fs::remove_file(&path);
}
// engine caller (lib.rs:336):
let entries = load_registry(&dir).unwrap_or_default();
// engine save (lib.rs:455):
let _ = save_registry(dir, &entries);
```

**Suggested fix:** Never delete files based on an in-memory set that may reflect a failed load: fail boot (or degrade to read-only for dimensions) when load_registry errors instead of unwrap_or_default; make save_registry surface errors; and consider quarantining (renaming) rather than deleting orphan bodies.

#### critical-5. WAL keeps appending after a partial append failure; recovery silently discards and truncates all later acknowledged commits
- **Where:** `crates/epiphany-persist/src/store.rs:307`  |  **Category:** bug  |  **Found by:** engine

If wal.write_all fails partway (e.g. ENOSPC), a torn frame is left mid-file, the engine surfaces BatchError::Persist and the cube stays in service (apply_batch, engine lib.rs:808-814, does nothing to remediate). The File cursor sits after the torn bytes, so the next successful set_batch appends a complete, fsync'd, acknowledged batch AFTER the torn frame. wal::replay stops at the first torn/corrupt record (wal.rs:144-157 'break'), so on the next restart every acknowledged commit after the tear is dropped - and Store::open then truncates the file to good_len (store.rs:178-180), physically deleting them. Operational story: disk fills briefly, one write errors, space is freed, the server keeps accepting writes for days, then a restart silently loses all of them. The same hazard applies to the set_pin WAL append (store.rs:694).

```
self.wal.write_all(&framed)?;
if self.sync_on_write {
    self.wal.sync_data()?;
}
// 3. Adopt the validated trial; the WAL already reflects it durably.
self.model.cube = trial;
```

**Suggested fix:** Record the WAL offset before each append; on any append/sync error, set_len back to that offset (or at minimum mark the store failed so the engine refuses further commits on the cube until a successful checkpoint), so the log can never contain a tear followed by live records.

#### critical-6. Flow reads bypass cube-level security entirely (Flow:Write = read any cube)
- **Where:** `crates/epiphany-api/src/flow_reader.rs:70`  |  **Category:** security  |  **Found by:** security-crate, api-workspaces

ApiFlowReader::with_view builds a per-cube view for ctx.cube(name).readCell()/.members() with only the element mask applied — it never checks the run principal's Cube:Read on the cube being read. run_flow_handler (flow_routes.rs:550) gates only global Flow:Write. So a user holding the standard ADR-0023 'flow author' role (Cube:Read on one cube + Flow:Write) can author and run a flow that reads cells from EVERY cube on the server — including cubes they hold zero grants on — and exfiltrate via the run report/log or by staging the values into a cube they can write (authorize_outcome gates writes, not reads). ADR-0023 explicitly promises 'a flow is never a privilege-escalation path... the flow's effects are authorized as the runner'; that was implemented for writes (flow_routes.rs:141 authorize_outcome) but not for reads. Element masks alone don't help: most cubes are protected by cube-level grants, not element ACLs.

```
let snapshot = self
    .state
    .engine
    .snapshot(cube)
    .ok_or_else(|| FlowReadError::UnknownCube(cube.to_string()))?;
let mask = element_mask_for(&self.state, &self.username, &snapshot);
let resolver = self.state.cells.resolver_with(&snapshot, None, mask.as_ref());
```

**Suggested fix:** In with_view, resolve the run principal and require cube_access(username, cube) >= Read before capturing the view (fail-closed for unknown cubes/principals), mirroring authorize_outcome's per-cube write gating. Add an acceptance test: a Flow:Write-only user's flow reading an ungranted cube gets AccessDenied.

#### critical-7. Cube-scoped dimension edit fans out to all referencing cubes with weaker authorization than the registry endpoint
- **Where:** `crates/epiphany-api/src/model_routes.rs:609`  |  **Category:** security  |  **Found by:** api-model-endpoints

POST /cubes/{cube}/dimensions/{dim}/edit requires only Dimension:Write scoped to that one cube (which a single-cube Cube:Admin also confers via effective()), and checks deny_if_element_restricted only on that cube. But for a registry-backed dimension, Engine::edit_dimension (epiphany-engine/src/lib.rs:559-592) publishes the registry generation and fans the edit out to EVERY referencing cube. The sibling endpoint POST /dimensions/{id}/edit (dimension_routes.rs:459-499) for the exact same operation requires GLOBAL Dimension:Write and denies a caller element-restricted on ANY referencing cube. Consequence: a user granted modeling rights on cube A alone can issue {"op":"delete"} or {"op":"set_kind"} on a shared dimension and destroy members and their stored cell values in cubes B and C where they hold zero access, and can delete/convert members hidden from them by element ACLs on those other cubes — bypassing the fail-closed invariant the id-route explicitly enforces. Note add_elements (lines 441-447) blocks cube-local mutation of registry-backed dimensions precisely to force such changes through the globally-gated library route; the far more destructive structural-edit route has no such guard.

```
require_kind_access(
    &state,
    &auth,
    ObjectKind::Dimension,
    Some(&cube),
    AccessLevel::Write,
)?;
if body.touches_element_data() {
    deny_if_element_restricted(&state, &auth, &snapshot(&state, &cube)?)?;
```

**Suggested fix:** In edit_cube_dimension, when state.engine.dimension_backing(&cube, &dimension) is Some, either (a) reject with the same 409 divergence message add_elements uses and direct callers to /dimensions/{id}/edit, or (b) apply the id-route's guards: global Dimension:Write plus deny_if_element_restricted over every referencing cube.

#### critical-8. Every consolidated read in production densely enumerates the leaf cartesian product; feeders are never used on any read path
- **Where:** `crates/epiphany-calc/src/eval.rs:279`  |  **Category:** olap  |  **Found by:** olap-core-semantics

The server always injects CalcFactory (epiphany-server/src/main.rs:177), so every cube read goes through CalcEngine::compute, and every non-all-leaf coordinate calls Cube::consolidate_with, which enumerates the full cartesian product of contributing leaves per dimension (cube.rs:1220 'total = per_dim.iter().map(|w| w.len()).product()'). The sparse stored-cell scan (Cube::get) is bypassed even for cubes with zero rules, and Cube::consolidate_fed - the entire point of ADR-0005 feeder inference - is referenced nowhere outside cube.rs and feeders.rs validation. On a modest cube (e.g. 4 dims x 100-1000 leaves) a single grand-total read requires 10^8-10^12 recursive value() calls (each allocating a boxed coord and a memo entry), so any view touching a total effectively hangs the request; at extreme sizes the unchecked usize product wraps in release (no overflow-checks in the workspace profile), yielding a silently wrong (e.g. zero) total instead. ADR-0005/0007 sanction dense as the correctness truth, but they also promise the fed sparse path as the read optimization; it was never wired, so the product is unusable beyond toy models.

```
// Consolidate, pulling each contributing leaf back through the
// resolver so rule-derived leaves are included with correct weights.
cube.consolidate_with::<CalcError, _>(coord, |lc| self.value(ordinal, lc))
// cube.rs:1220
let total: usize = per_dim.iter().map(|w| w.len()).product();
for n in 0..total { ... acc += weight * i128::from(leaf_value(&combo)?...) }
```

**Suggested fix:** Route consolidated reads through consolidate_fed using the inferred FeederIndex (rebuild it per published version), falling back to the sparse stored-scan Cube::get algebra when the cube has no rules; keep consolidate_with only as the validation oracle. Use saturating_mul with a hard cap on the dense product as a guard.

#### critical-9. Rules that fail to compile after a model edit are silently dropped, reverting every derived cell to stored/aggregated values
- **Where:** `crates/epiphany-api/src/calc_factory.rs:93`  |  **Category:** bug  |  **Found by:** olap-core-semantics

PinnedRegistry::build swallows any rule parse/compile failure and substitutes an empty CompiledModel. The comment justifies this by 'the API rejects [bad rules] at define time', but structural edits invalidate that premise: deleting or renaming an element or dimension referenced by a rule is not re-validated anywhere (dimension_routes.rs performs no rule compile check), after which compile() fails with UnknownMember and the ENTIRE rule set of the cube - not just the broken statement - vanishes from every read. Margins, overrides, and cross-cube derivations silently become zero or raw aggregation with no error, no diagnostic, and no log; the numbers just change. Classic engines either block the edit or surface a loud rule-error state.

```
rules::parse(&s.rules().source)
    .ok()
    .and_then(|doc| compile(s.cube(), &cr, &doc, s.version()).ok())
    .unwrap_or(CompiledModel {
        version: s.version(),
        rules: Vec::new(),
    })
```

**Suggested fix:** Re-compile the cube's rules as part of validating any structural edit (reject or warn), and make read-time compile failure loud: fail reads with a RULE_COMPILE_ERROR (or serve values flagged degraded), never a silent empty rule set.

#### critical-10. View cache key omits cross-cube rule dependencies: stale numbers served indefinitely after writes to a referenced cube
- **Where:** `crates/epiphany-api/src/view_cache.rs:113`  |  **Category:** bug  |  **Found by:** olap-query-semantics

Rules support cross-cube references (e.g. `Sales.Revenue = Units * FX!Rate`, exercised in epiphany-calc/src/eval.rs:803), and view execution evaluates them through a PinnedRegistry that snapshots every cube. But the cache key contains only the TARGET cube's MVCC version. Commit versions are per-cube (Engine::apply_batch updates only that cube's CubeState), so a write to cube FX bumps FX's version, not Sales'. A cached Sales cellset therefore keeps being served with pre-write FX numbers until something commits to Sales or the entry is evicted by LRU — there is no TTL. ADR-0028 decision 6 claims version-keying is 'the entire invalidation story' and that read-after-write consistency 'holds by construction'; that only holds for single-cube dependencies. Users of any cube with cross-cube rules see acknowledged writes not reflected in views, indefinitely.

```
ViewCacheKey {
    cube: read.cube.to_string(),
    version: read.version,
    shape,
    sandbox_scope,
    mask,
}
```

**Suggested fix:** Include the versions of every cube the target's compiled rules can read (the compile step already resolves cross-cube ordinals), or key on the global max commit id across cubes when the target has cross-cube rules, or simply disable caching for cubes whose compiled rules contain cross-cube references.

#### critical-11. Store stays in service after a failed WAL append/fsync; later acknowledged commits are silently discarded on recovery
- **Where:** `crates/epiphany-persist/src/store.rs:307`  |  **Category:** olap  |  **Found by:** concurrency-durability-lens

set_batch (and set_leaf/set_string/set_pin) append to the WAL and fsync, but on any I/O error the store simply returns the error and keeps serving. Two divergence modes follow. (a) Torn hole: if write_all fails partway (e.g. ENOSPC), a partial frame is left in the file and the cursor sits after it; once writes succeed again (space freed, transient error cleared), subsequent batches are appended AFTER the garbage, fsynced, and acknowledged to clients — but wal::replay stops at the first bad CRC, so on the next restart every acknowledged commit after the hole is silently discarded (recovery even truncates them away via set_len(replay.good_len)). (b) Phantom batch: if write_all fully framed the batch but sync_data fails, the client gets a 5xx and the in-memory cube is not updated (trial dropped), yet the intact frame is on disk — after a crash, replay resurrects a write the client was told failed. There is no truncate-back-to-durable-offset, no fail-stop/poisoning of the store, and no repair on the error path (restore_model touches only memory).

```
self.wal.write_all(&framed)?;
if self.sync_on_write {
    self.wal.sync_data()?;
}
// 3. Adopt the validated trial; the WAL already reflects it durably.
self.model.cube = trial;
```

**Suggested fix:** On any WAL append/sync error, either fail-stop the store (reject all further writes until reopen/recovery) or record the last-known-durable offset and set_len back to it before allowing the next append; add a crash test that injects a partial append followed by successful commits and asserts recovery keeps the later commits.

### MAJOR (68)

#### major-1. String-to-numeric kind conversion drops string cells that are still valid
- **Where:** `crates/epiphany-core/src/cube.rs:839`  |  **Category:** bug  |  **Found by:** core-storage

retype_cells_for_kind for (String -> Leaf) drops EVERY string cell whose coordinate component in dimension d equals the converted element. But a string cell only needs at least one string component (set_string, lines 1096-1124): a cell such as [E(String in dim A), Comment(String in dim B)] remains a perfectly valid string coordinate after E becomes a numeric leaf, because Comment still carries the string typing. The code deletes it anyway, so a user converting one member's kind silently loses unrelated stored text at intersections with other string elements. ADR-0036 only sanctions clearing the member's own incompatible value, not cells still addressable after the conversion. (The doc comment on retype_cells_for_kind also claims values are 'moved to the matching store when they transfer cleanly' - no move is ever implemented, only drops.)

```
(Str, Leaf) => {
    // String leaf -> numeric: text has no numeric value, so clear it.
    self.drop_cells_for_element(d, element, false, true);
}
```

**Suggested fix:** In the (Str, Leaf) arm, drop only string cells for which the converted element was the sole String component: keep the cell when any other coordinate component is still ElementKind::String. Also fix the stale 'moves the value' doc comment to describe the actual clear-only behavior.

#### major-2. consolidate_with computes the dense combo count with an unchecked usize product
- **Where:** `crates/epiphany-core/src/cube.rs:1220`  |  **Category:** bug  |  **Found by:** core-storage

The dense rule-aware consolidation seam multiplies per-dimension leaf counts with Iterator::product() on usize. The workspace release profile does not enable overflow-checks, so on a large cube (e.g. 7 dimensions x ~600 leaves each, ~2.8e19 combos) the product wraps silently and the mixed-radix walk enumerates a garbage-sized (possibly tiny) combo count, returning a silently wrong consolidated value instead of an error. In debug/test builds it panics instead. Contrast spread_leaves (spread.rs lines 86-94), which deliberately uses saturating_mul plus a cap for the same computation - this path has neither the saturation nor any bound, so even without overflow a top-level rollup can attempt an effectively unbounded dense enumeration on one request thread.

```
let total: usize = per_dim.iter().map(|w| w.len()).product();
if total == 0 {
    return Ok(Fixed::ZERO);
}
```

**Suggested fix:** Use checked_mul (or saturating_mul plus an explicit cap like MAX_SPREAD_LEAVES) and return ModelError::Overflow / a dedicated too-large error instead of enumerating; mirror the spread_leaves pattern.

#### major-3. Leaf reads through get/consolidate_* allocate per-dimension weight maps before the all-leaf fast path
- **Where:** `crates/epiphany-core/src/cube.rs:1143`  |  **Category:** performance  |  **Found by:** core-storage, olap-core-semantics

Cube::get builds per_dim by calling leaf_weights for every dimension (each call allocates a HashMap, HashSet, VecDeque and result Vec, then is re-collected into another HashMap) BEFORE checking all_leaf and short-circuiting to a direct store lookup that uses none of it. Every pure-leaf cell read - the hottest operation in view execution via StoredCells::value - therefore pays roughly 4-5 heap allocations per dimension that are immediately discarded. consolidate_with (lines 1203-1215) and consolidate_fed (lines 1269-1284) repeat the same pattern. On a 100k-cell view over a 6-dimension cube this is millions of wasted allocations, directly against the ultra-performance mandate.

```
let mut per_dim: Vec<HashMap<u32, i64>> = Vec::with_capacity(self.rank());
let mut all_leaf = true;
for (d, &idx) in coord.iter().enumerate() {
    if self.dimensions[d].element(idx)?.kind != ElementKind::Leaf {
        all_leaf = false;
    }
    per_dim.push(self.dimensions[d].leaf_weights(idx)?.into_iter().collect());
}
```

**Suggested fix:** First scan the coordinate kinds (element() only, no allocation); return the direct lookup when all components are numeric leaves, and only then build per_dim for the consolidation path. Apply the same reordering in consolidate_with and consolidate_fed.

#### major-4. extend_schema edge idempotency check rebuilds and sorts the entire edge list per edge, making batch growth quadratic
- **Where:** `crates/epiphany-core/src/cube.rs:966`  |  **Category:** performance  |  **Found by:** core-storage

apply_growth checks whether each incoming edge already exists by calling dim.edges(), which allocates a Vec of ALL edges and sorts it (dimension.rs lines 147-156), then linearly scans it - once per edge spec. Building a dimension with E edges through extend_schema (the runtime path flows use, and the path Cube::build funnels everything through) costs O(E^2 log E) allocations and sorts. A flow loading a 100k-edge hierarchy pays ~100k full edge-list materializations and sorts. Additionally each extend_schema call clones the entire cube including both cell stores (line 436) even when only dimension metadata changes, so repeated idempotent calls on a populated cube each copy every cell.

```
if let Some(&(_, _, w)) = dim
    .edges()
    .iter()
    .find(|&&(p, c, _)| p == parent && c == child)
```

**Suggested fix:** Query the parent's own edge list directly (dim.children[parent] via a small accessor returning weight for a given child) instead of materializing/sorting all edges per spec; it is O(children-of-parent). Longer term, stage extend_schema on cloned dimensions only and rebuild cell stores solely when the layout actually changes.

#### major-5. Cube::build applies attribute values one per call, cloning the whole dimension each time
- **Where:** `crates/epiphany-core/src/cube.rs:373`  |  **Category:** performance  |  **Found by:** core-storage

Cube::build loops over d.attribute_values and calls set_attribute_values once per single (element, attribute, value) triple. set_attribute_values (line 477) clones the entire Dimension - elements Vec, name index, children edge lists, attr_values, alias map - for transactional staging on every call. Materializing a cube from a registry dimension (the ADR-0024/0033 path this code explicitly exists for) with V attribute values over an E-element dimension costs O(V*E) time and allocation; with ADR-0032-scale member tables (100k+ members with aliases) cube creation degrades to minutes of pure cloning.

```
for (element, attr, value) in &d.attribute_values {
    cube.set_attribute_values(&d.name, attr, &[(element.clone(), value.clone())])?;
}
```

**Suggested fix:** Group d.attribute_values by attribute and pass each group as one slice to set_attribute_values (one dimension clone per attribute instead of per value), or add a batch path that stages one clone for all values.

#### major-6. Element kind conversion strands sandbox overrides, making the whole sandbox permanently uncommittable
- **Where:** `crates/epiphany-core/src/query.rs:1334`  |  **Category:** bug  |  **Found by:** core-storage, core-query-model

Model only remaps sandbox override coordinates for reorder/insert/delete; the comment declares kind-affecting edits safe because they are index-stable. But Cube::set_element_kind (leaf->consolidated or leaf->string) and reparent_element/add_child_element (which promote a leaf parent to a consolidation and drop its stored cells, cube.rs:638-642, 702-704, 843-845) leave any sandbox override addressing that element in place. The stale override is invisible on read (the calc overlay is only consulted at the all-leaf terminal), so the user cannot see it - but commit_sandbox (epiphany-persist/src/store.rs:491-514) replays every override through set_batch, where set_leaf rejects WriteToNonLeaf/CellTypeMismatch and aborts the ENTIRE commit wholesale. There is no API to remove a single override (sandbox_set_cells only upserts; a zero write is stored as an explicit override), so after an admin converts an element's kind, every sandbox holding an override on it can only be discarded in full - losing the user's entire what-if work.

```
// Index-stable edits (reparent/add-child/set-kind) need
// no remap, so they stay on `cube` directly.
```

**Suggested fix:** Add Model wrappers for set_element_kind / reparent / add_child that drop sandbox overrides (numeric and string) whose coordinate addresses the converted element, mirroring how the cube drops its own stored cells; alternatively make commit_sandbox skip-or-report overrides that are no longer leaf-writable instead of failing wholesale.

#### major-7. CalcFactory resolver ignores the pinned snapshot, breaking MVCC read isolation and poisoning the view cache
- **Where:** `crates/epiphany-api/src/calc_factory.rs:224`  |  **Category:** concurrency  |  **Found by:** core-query-model, olap-core-semantics, olap-query-semantics

The production CellResolver seam implementation discards the ReadSnapshot it is handed (using only its cube name) and re-snapshots every cube from the live engine at resolver-construction time. Route handlers pin `snap` first, resolve axes/subsets/sandbox from `snap.model()`, key the view cache on `snap.version()`, and echo `snap.version()` to the client - but the cell VALUES come from whatever version is published when PinnedRegistry::build runs. A commit landing between snapshot() and resolver construction yields a cellset whose axes come from version N and whose values come from version N+1, reported and CACHED as version N; the poisoned entry is then served to every concurrent reader of version N (ViewCacheKey doc claims 'a stale entry can never be hit', which this violates). This contradicts ADR-0001/0014's 'a read pins one base snapshot'. Secondarily, `ordinal_of(...).unwrap_or(0)` would silently resolve an unknown cube to ordinal 0 and read a different cube's values rather than erroring.

```
let registry = PinnedRegistry::build(&self.engine);
let target = registry.ordinal_of(snapshot.cube().name()).unwrap_or(0);
// The overlay covers the target cube's leaves only (ADR-0014).
let overlay = sandbox.map(|sb| OwnedOverlay::new(target, sb));
```

**Suggested fix:** Build the PinnedRegistry around the passed-in `snapshot` for the target cube (only sibling cubes needed for cross-cube rules may be re-snapshotted, ideally captured once per request), and replace `unwrap_or(0)` with an error. Verify with a test that commits a write between snapshot() and resolver construction and asserts the cellset matches the pinned version.

#### major-8. Cellset has no string-value channel: string cells render as 0 and zero-suppression drops rows/columns whose only data is text
- **Where:** `crates/epiphany-core/src/query.rs:593`  |  **Category:** olap  |  **Found by:** core-query-model

execute_view_with fills the grid exclusively through CellResolver::value; CellResolver::string_value (query.rs:97) is never called and Cellset carries only `cells: Vec<Fixed>`. A coordinate addressing a String (S) element consolidates to numeric 0 (Dimension::leaf_weights returns no leaves for a string element), so every saved view, ad-hoc cellset, and MDX query renders a populated comment/text cell as "0" (the API DTO even hard-codes kind: "numeric" at query_routes.rs:1011, while its own doc at dto.rs:93 promises 'text (string)'). Worse, with suppress_zero_rows/columns on, a row or column whose only content is string cells is judged all-zero and silently deleted - users' comment data vanishes from views. The live PivotGrid (which fetches cells individually and does show strings) has a comment mirroring this engine behavior for suppression, but the cellset path cannot display string data at all, and the same string cell is also reported editable=true with value "0", inviting a write that the server will reject.

```
let cell_at = |r: usize, c: usize, scratch: &mut Vec<u32>| -> Result<Fixed, QueryError> {
    ...
    cells.value(scratch)
};
...
(0..nrows).partition(|&r| (0..ncols).any(|c| !at(r, c).is_zero()))
```

**Suggested fix:** Give Cellset an optional string channel (e.g. per-cell Option<String> populated via string_value for coordinates addressing an S element), render kind: "string" in the DTO, and treat a populated string cell as non-zero for suppression (or document the deferral in an ADR and at minimum stop reporting string cells as editable numeric zeros).

#### major-9. No cardinality cap on view execution: dense grid and eager crossjoin let one request exhaust memory
- **Where:** `crates/epiphany-core/src/query.rs:599`  |  **Category:** performance  |  **Found by:** core-query-model

execute_view_with allocates a dense nrows*ncols Vec<Fixed> and crossjoin() eagerly materializes every axis tuple as an owned Vec<Vec<u32>> (query.rs:1507-1521) with no upper bound anywhere in core or in the API handlers (only the view CACHE has a MAX_CACHE_CELLS ceiling, which is checked after computation). Spreading has MAX_SPREAD_LEAVES = 200_000, but a view does not: any authenticated user with Read on a cube can POST an ad-hoc cellset or MDX crossjoin placing several large dimensions on one axis (e.g. three 1,000-member dimensions on rows = 10^9 tuples, tens of GB in tuple Vecs alone before the grid is even allocated), taking down the whole in-memory server. With a rule-aware resolver each consolidated cell additionally enumerates its dense leaf space, multiplying the cost.

```
let nrows = row_tuples_idx.len();
let ncols = column_tuples_idx.len();
let total = nrows * ncols;
...
let mut grid = Vec::with_capacity(total);
```

**Suggested fix:** Introduce a MAX_CELLSET_CELLS-style cap (checked from per-spec list lengths BEFORE materializing crossjoin tuples, saturating like spread's cap) returning a structured QueryError the API maps to 422, with an operator-configurable limit.

#### major-10. Feeder inference under-feeds analyzable rules with additive cross-cube terms (confirmed)
- **Where:** `crates/epiphany-calc/src/feeders.rs:173`  |  **Category:** olap  |  **Found by:** calc

A rule mixing same-cube and cross-cube inputs additively, e.g. ['Measure':'Rev'] = value['Measure':'Units'] + 'FX'!['Pair':'EUR'];, is classified analyzable (inputs non-empty) and NOT reported opaque, but base_potent treats every CExpr::Cell as false, so targets are fed only where the same-cube input is potent. Where Units is empty the actual value equals the FX scalar (non-zero) and the coordinate is not fed. I confirmed with a probe: opaque=[], South/Rev not fed, engine value = 5, validate_feeders reports under_fed=[[1,1]]. This directly violates the module doc and ADR-0005 section 2 ("it never under-feeds an analyzable rule" - the ADR only defers cross-cube-ONLY rules to the opaque path). Second manifestation of the same root cause: coordinates produced by opaque rules never enter the `potent` set, so a downstream same-cube rule reading them (Net = Margin where Margin is cross-cube-driven) is silently under-fed AND not reported opaque. The moment consolidate_fed consumes this index (its stated purpose), consolidated totals silently drop contributions - the classic OLAP wrong-zero; today it already makes the /feeders/diagnostics endpoint report the system's own inferred index as under-fed.

```
let inputs: Vec<&CCell> = cells.iter().copied()
    .filter(|c| c.cube == target_ordinal)
    .collect();
let bp = base_potent(&rule.expr);
if bp || !inputs.is_empty() {
// and in base_potent (line 117):
CExpr::Cell(_) => false,
```

**Suggested fix:** Treat a cross-cube (non-target-ordinal) cell as a potential base contribution in base_potent (feed the whole area, a sound over-feed), or report any rule containing a cross-cube cell in an additive/branch position as opaque. Also mark opaque rules' target coordinates as potent (or provide a manual-feeder seed parameter to infer_feeders) so chained rules downstream of opaque rules are not silently under-fed.

#### major-11. Canonical Display/parse round-trip changes semantics for IF in binary/nested position (confirmed)
- **Where:** `crates/epiphany-calc/src/rules/ast.rs:407`  |  **Category:** bug  |  **Found by:** calc

Expr::If is printed without parentheses, and its THEN/ELSE sub-expressions are printed bare, so an If used as a binary operand loses its grouping. Confirmed: parse("['M':'x'] = (IF value > 0 THEN 1 ELSE 2) + 3;") displays as "['M': 'x'] = (IF value > 0 THEN 1 ELSE 2 + 3);", which reparses as IF value > 0 THEN 1 ELSE (2 + 3) - the value flips from 4 to 1 whenever the condition holds, and Display is not even idempotent (once != twice). The dangling-else case (IF a THEN (IF b THEN x) ELSE y) diverges the same way. This violates the module's own contract ("a canonical Display that round-trips back through parse", "Display is the canonical comparison"): any AST-equality comparison via Display can equate semantically different rules, and any future canonicalize/format feature built on Display would silently change users' numbers. The existing round-trip corpus and property tests never place IF inside a binary expression, which is why this passes CI.

```
Expr::Bin { op, left, right } => write!(f, "({left} {op} {right})"),
...
} => match otherwise {
    Some(o) => write!(f, "IF {cond} THEN {then} ELSE {o}"),
    None => write!(f, "IF {cond} THEN {then}"),
},
```

**Suggested fix:** Parenthesize If (and its else-less form) whenever it is not the top of an expression - simplest is to always print IF expressions as (IF ... THEN ... ELSE ...), or wrap then/otherwise operands in parens when they are Bin. Add (IF c THEN a ELSE b) + x and nested dangling-else cases to the round-trip corpus.

#### major-12. infer_feeders/validate_feeders materialize the full dense cartesian product of every rule's target area
- **Where:** `crates/epiphany-calc/src/feeders.rs:321`  |  **Category:** performance  |  **Found by:** calc

area_leaf_coords builds a Vec of every leaf coordinate a rule's area selects. A typical rule constrains one dimension and leaves the rest Any, so the target set is the full leaf cross-product of all other dimensions - on a production-size cube (e.g. 5 dims x 1000 leaves = 10^12 coords, ~40 bytes each) Vec::with_capacity(total) aborts on allocation failure or OOMs the whole in-memory server. This is reachable by a single authenticated GET /cubes/{cube}/feeders/diagnostics call (rule_routes.rs:294). The fixpoint loop then re-walks the entire target Vec every round. ADR-0005 explicitly bounds the INPUT expansion with INPUT_EXPANSION_CAP "so inference cost stays bounded", but the target expansion has no cap at all, defeating the ADR's own scaling motivation ("enumerating the full dense leaf space ... does not scale").

```
let total: usize = per_dim.iter().map(|v| v.len()).product();
if total == 0 {
    return Vec::new();
}
let mut out = Vec::with_capacity(total);
for n in 0..total {
```

**Suggested fix:** Cap the target expansion like the input expansion (report the rule as too-large-to-infer/validate beyond a bound), and stream targets with a mixed-radix iterator instead of materializing the Vec (the fixpoint only needs iteration plus the fed/potent membership checks).

#### major-13. Explain materializes and sorts the entire dense leaf space under a consolidated cell
- **Where:** `crates/epiphany-calc/src/provenance.rs:183`  |  **Category:** performance  |  **Found by:** calc

contributing_leaves builds a Vec<Vec<u32>> of every contributing leaf coordinate of a consolidated coord and then sorts it, and explain_node evaluates every one (even at ExplainDepth::Immediate, since breadth is unbounded - only recursion depth is budgeted). Explaining a top-level total on a realistic cube (10^8+ leaf combinations) allocates one heap Vec per coordinate (~40+ bytes each, multi-GB) before sorting, on top of the dense evaluation time. POST /cubes/{cube}/cells/explain (rule_routes.rs:215) exposes this to any read-authorized user, so one explain call on a large model can OOM the server. The dense read path next door (Cube::consolidate_with) deliberately streams with a single reusable combo buffer; provenance should not be strictly heavier in memory than the value computation it explains.

```
let mut out = Vec::with_capacity(total);
for n in 0..total {
    let mut rem = n;
    let mut c = vec![0u32; cube.rank()];
    ...
    out.push(c);
}
out.sort();
```

**Suggested fix:** Iterate leaves with a streaming mixed-radix walk over per-dim sorted leaf lists (which yields a deterministic order without materializing), keep only the non-zero contributions (already filtered), and cap the number of reported contributions per node (e.g. top-N with a count of the rest).

#### major-14. Arrow-function block bodies are classified as object literals, so type annotations inside them are never stripped
- **Where:** `crates/epiphany-flow/src/strip.rs:125`  |  **Category:** bug  |  **Found by:** flow

After `=>`, both `=` and `>` fall into the catch-all operator arm (prev=Op), so the `{` that opens an arrow function's block body is pushed as an object-literal scope. Inside that scope handle_colon() keeps every colon as an 'object key separator', so documented-supported variable annotations are left in the output and boa fails with a SyntaxError. Verified: `const f = (x) => { const y: number = x; return y; };` and `ctx.input().forEach((r) => { const v: number = Number(r.Value); ... });` pass through byte-identical. This is the single most common TS callback idiom, so most real TS flows fail to run with a confusing JS parse error even though the constructs are in the stripper's documented supported subset (annotations on variables). The crate's own tests avoid arrows entirely (they use `function(r){}` and `for..of`), which is why this is not caught.

```
'{' => {
    // Object literal in expression position, else a block.
    let is_object = matches!(self.prev, Prev::Op | Prev::KeywordExpr);
    self.braces.push(is_object);
```

**Suggested fix:** Detect `=>` explicitly (it is two chars, trivially matched in run()) and set a state such that a following `{` is classified as a block. Alternatively track the previous two significant chars and treat `{` after `=>` as a block.

#### major-15. Call-site generic arguments pass through and re-parse as comparison chains — sometimes silently wrong values
- **Where:** `crates/epiphany-flow/src/strip.rs:161`  |  **Category:** bug  |  **Found by:** flow

The stripper handles generics only on `function NAME<...>(` declarations. A `<` after a value (call-site type arguments: `parse<number>(x)`, `arr.map<Row>(f)`, generic arrows `const f = <T>(x) => x`) falls into the catch-all operator arm and is left untouched. Verified: `const n = parse<number>(x);` and `ctx.input().map<Row>(f);` come out byte-identical, and JS parses them as `(parse < number) > (x)` — a relational chain. When the type argument names a value binding (Number, String, Boolean, Array, Object are all legal TS types people write), the expression evaluates to a boolean with NO error: silently corrupted logic, directly violating the module's stated contract ('the worst case is a runtime parse error … never silently corrupted logic', lines 8-10). When the type name was stripped (an interface), the user instead gets a baffling ReferenceError at an unrelated spot.

```
_ => {
    // Any other operator/punctuation puts us in expression
    // position (so a following `{` is an object, `/` is a regex).
    self.prev = Prev::Op;
    self.i += 1;
}
```

**Suggested fix:** When `<` is seen with prev==Value, attempt scan_angle_balanced_from(); if it balances and is immediately followed by `(` or a template literal, either blank it (it is a type-argument list) or return a loud StripError ('call-site generic arguments are not supported'). Fail-loud is the cheap, contract-preserving option.

#### major-16. cubeWriteCellsJson silently drops non-string coordinate members and whole cells
- **Where:** `crates/epiphany-flow/src/run.rs:651`  |  **Category:** bug  |  **Found by:** flow

The CTX_PRELUDE passes the flow's coord object through JSON.stringify unchanged, so JS numbers stay JSON numbers. host_cube_write_cells (and parse_coord for reads) keeps only entries where `member.as_str()` is Some: a coordinate like `{ Year: 2024, Region: 'North' }` silently loses the Year dimension, and a cell whose members are all numeric is silently not staged at all (`if !coord.is_empty()`), with no error and no count anywhere. Users writing the natural `writeCell({ Year: 2024, ... }, v)` get either quietly missing cells (data loss with a clean run report) or a later confusing 'element <missing> not found' apply error naming no culprit. Same silent-skip pattern applies to elements/edges with non-string fields via str_field().

```
if let Some(obj) = item.get("coord").and_then(|c| c.as_object()) {
    for (dim, member) in obj {
        if let Some(m) = member.as_str() {
            coord.insert(dim.clone(), m.to_string());
        }
    }
}
if !coord.is_empty() {
```

**Suggested fix:** Coerce JSON numbers/booleans to their exact string form (or coerce in the prelude with String(member)), and throw a JsNativeError for null/undefined/object members instead of dropping them — the runner is otherwise scrupulously fail-loud.

#### major-17. dedup_specs ignores edge weight, silently keeping the first weight and bypassing core's EdgeWeightConflict guard
- **Where:** `crates/epiphany-flow/src/run.rs:827`  |  **Category:** bug  |  **Found by:** flow

Edges are deduped by (dimension, parent, child) only. If a flow stages the same edge twice with different weights (e.g. addChild('D','P','C',1) then addChild('D','P','C',-1)), the second spec is silently discarded and the first weight wins. epiphany-core's apply_growth (cube.rs:963-975) deliberately errors on exactly this case — its comment reads 'silently keeping the old weight would corrupt rollups' — but the conflicting spec never reaches it because dedup removed it first. Result: consolidation weights differ from what the flow author last specified, producing wrong rolled-up numbers with a successful run report, instead of the loud EdgeWeightConflict the model layer was built to raise. Same weight-blind key is used for global-dimension edges at line 836.

```
let mut seen_edge = std::collections::HashSet::new();
c.edges
    .retain(|e| seen_edge.insert((e.dimension.clone(), e.parent.clone(), e.child.clone())));
```

**Suggested fix:** Include weight in the dedup key (dimension, parent, child, weight). True duplicates still collapse; conflicting weights survive to apply_growth, which rejects them loudly — mirroring how element dedup already includes `kind` so ElementKindConflict still fires.

#### major-18. Sandbox budget does not bound native-builtin CPU or memory: a flow can OOM or pin the whole server
- **Where:** `crates/epiphany-flow/src/run.rs:49`  |  **Category:** security  |  **Found by:** flow

The module doc claims 'a bounded loop-iteration and recursion budget caps runaway scripts deterministically', and MAX_STAGED exists 'so a flow cannot exhaust memory … (which would make the outcome depend on available RAM)'. But boa's loop_iteration_limit counts only interpreter loop constructs; native builtins are unmetered: `'a'.repeat(2**30)` allocates ~1-2 GB per call, `new Array(1e9).fill(0)` allocates ~8 GB, doubling string concatenation reaches terabytes within the 50M-iteration allowance, catastrophic regex backtracking (regress is a backtracking engine — /(a+)+$/) pins a CPU indefinitely with zero loop iterations, and ctx.log lines accumulate in `logs` with no cap. Since epiphany holds the entire OLAP model in this process's memory, one buggy or hostile flow can take down the server (data-plane DoS), and the outcome does depend on available RAM — the exact failure MAX_STAGED was meant to exclude.

```
const LOOP_LIMIT: u64 = 50_000_000;
/// Recursion-depth budget.
const RECURSION_LIMIT: usize = 800;
/// Cap on the total staged changes (elements + edges + cells) a single run may
/// accumulate, so a flow cannot exhaust memory by staging unboundedly
```

**Suggested fix:** Cap logs (count and total bytes) in host_log, and either run flows on a watchdog-supervised worker (kill on wall/CPU/RSS budget at the composition root) or use boa's stack-size limit plus a host-side allocation guard. At minimum correct the module doc so operators do not rely on the budget as an isolation boundary.

#### major-19. parse_csv keeps a UTF-8 BOM in the first header name, silently breaking column lookup in flows
- **Where:** `crates/epiphany-flow/src/csv.rs:38`  |  **Category:** bug  |  **Found by:** flow

No caller strips a byte-order mark (verified: no BOM handling anywhere in crates/ or web/; epiphany-connect and flow_routes feed raw text straight in). A file exported from Excel as 'CSV UTF-8' — which emits a BOM by default — parses with its first column keyed "\u{FEFF}Region" (verified empirically). The flow's `r.Region` is then undefined: ensureElements creates elements literally named 'undefined' (String(undefined) in the prelude), and writeCells coordinates lose that dimension via the non-string-member drop, so cells silently vanish or land on 'undefined' members. The run reports success. This is the single most common real-world CSV provenance and the failure is completely silent.

```
pub fn parse_csv(text: &str) -> Result<Vec<Row>, CsvError> {
    let records = split_records(text, MAX_CSV_ROWS)?;
    let mut iter = records.into_iter();
    let header = match iter.next() {
```

**Suggested fix:** Strip a single leading '\u{FEFF}' at the top of parse_csv: `let text = text.strip_prefix('\u{feff}').unwrap_or(text);`.

#### major-20. save_registry deletes stale dimension files before the new index is durable
- **Where:** `crates/epiphany-persist/src/registry.rs:63`  |  **Category:** bug  |  **Found by:** persist

The module doc claims 'The index is written last and is the authority on load, so a crash between writing dimension bodies and the index leaves a consistent (older) registry.' But the orphan sweep (lines 61-77) runs BEFORE index.toml is renamed into place (lines 88-91). A plain process crash between remove_file and the index rename leaves the OLD index on disk still listing a deleted id; load_registry then fails on `read_to_string(dim_path(dir, ie.id))?` for the missing body. Via the engine's unwrap_or_default this becomes an empty registry (feeding the critical deletion chain above); even fixed to propagate, it blocks the dimension library from loading after an unlucky crash.

```
// Remove orphaned dimension files (ids not in the current set).
let keep: std::collections::BTreeSet<u64> = entries.iter().map(|e| e.id).collect();
if let Ok(read) = std::fs::read_dir(dir) { ... std::fs::remove_file(&path) ... }
...
let tmp = dir.join("index.toml.tmp");
std::fs::write(&tmp, index)?;
std::fs::rename(&tmp, dir.join(INDEX_FILE))?;
```

**Suggested fix:** Reorder: write bodies, rename the new index into place (making it the authority), THEN sweep orphans. Also make load_registry tolerate a listed-but-missing body (skip with a loud log) so one lost file cannot take down the whole registry.

#### major-21. Checkpoint durably truncates the WAL after a snapshot rename that is never flushed on Windows
- **Where:** `crates/epiphany-persist/src/store.rs:324`  |  **Category:** bug  |  **Found by:** persist

checkpoint() relies on the snapshot rename being on disk before the WAL is cleared. On Unix, sync_dir makes the rename durable. On Windows sync_dir is an explicit no-op ('NTFS records the rename in its own metadata journal') and std::fs::rename calls MoveFileExW WITHOUT MOVEFILE_WRITE_THROUGH - the NTFS journal guarantees metadata consistency, not that the rename reaches disk before later writes. The very next statements truncate the WAL and explicitly fsync that truncation (sync_data). So on a power loss shortly after a checkpoint, the durable state can be OLD snapshot + EMPTY WAL: every acknowledged write since the previous checkpoint is silently lost, and each fsync'd set_leaf's durability guarantee ('every acknowledged write survives a crash') is voided. Windows is a first-class platform here (dev environment, Excel add-in product).

```
write_snapshot(&self.dir, &self.model)?;
self.wal.set_len(0)?;
self.wal.seek(SeekFrom::Start(0))?;
self.wal.write_all(&wal::header())?;
self.wal.sync_data()?;
// sync_dir (854-863): #[cfg(not(unix))] { let _ = dir; } // no-op
```

**Suggested fix:** On Windows, after the rename, open the renamed snapshot.model and call sync_all() (FlushFileBuffers flushes the file's MFT record including its name attribute), or perform the replace via a mechanism with write-through semantics. Alternatively make the WAL clear crash-safe against a reverted rename by embedding a checkpoint-generation marker the snapshot and WAL header share.

#### major-22. Kind-converting edits leave a crash window where WAL replay is rejected and the whole server cannot boot
- **Where:** `crates/epiphany-persist/src/store.rs:708`  |  **Category:** bug  |  **Found by:** persist

The block comment (lines 588-591) says reparent/add-child/set-kind 'keep indices stable, so their single post-edit checkpoint suffices'. Indices stay stable, but WRITABILITY does not: set_element_kind re-types an element (leaf->consolidated/string clears/rejects numeric writes), and add_child_element/reparent_element convert a numeric/string parent to a consolidation (cube.rs:654, 689-692). If the WAL holds a SetLeaf/SetString for that element (written since the last checkpoint) and the process crashes inside checkpoint() between the snapshot rename and the durable WAL truncation, recovery replays the pre-edit record onto the post-edit snapshot: model.cube.set_leaf on a now-consolidated element returns Err, Store::open fails with PersistError::Model, and boot.rs:49 (`let store = Store::open(&path)?;`) aborts the ENTIRE server. The operator must hand-delete wal.log (safe, since the checkpoint already folded the writes in, but nothing tells them that).

```
pub fn set_element_kind(&mut self, dimension: &str, element: &str, kind: ElementKind)
    -> Result<(), PersistError> {
    self.model.cube.set_element_kind(dimension, element, kind)?;
    self.checkpoint()
}
// vs reorder_elements (599-603), which checkpoints BEFORE the edit
// replay rejection path: store.rs:158 model.cube.set_leaf(coord, *value)?
```

**Suggested fix:** Treat set_element_kind, add_child_element, and reparent_element like the reindexing ops: checkpoint() before the edit so the WAL is empty when the post-edit snapshot lands. Alternatively, make Store::open tolerate a replay-rejected record after a fresh snapshot (skip with a loud warning) instead of refusing to start.

#### major-23. A failed WAL append poisons the log: later acknowledged writes are silently discarded on recovery
- **Where:** `crates/epiphany-persist/src/store.rs:251`  |  **Category:** bug  |  **Found by:** persist

set_leaf/set_string/set_pin/set_batch write the framed record with one write_all. If write_all (or sync_data) fails partway - ENOSPC is the realistic case - a PARTIAL frame remains in the middle of the WAL, and the store keeps using the same handle: once space is freed, subsequent writes are appended AFTER the garbage, fsynced, and acknowledged Ok. At the next recovery, wal::replay stops at the first bad frame ('an append-only log only ever tears at the tail' no longer holds) and Store::open physically truncates (set_len) every acknowledged record after it - silent loss of committed data. Additionally, set_leaf applies the mutation to the in-memory cube BEFORE the append, so on a failed append memory diverges from the log, and any later checkpoint (e.g. a define op) durably persists a write whose caller was told it failed.

```
self.model.cube.set_leaf(coord, value)?;      // memory mutated first
let framed = wal::encode(&Record::SetLeaf { ... });
self.wal.write_all(&framed)?;                 // partial write leaves a torn frame
if self.sync_on_write {
    self.wal.sync_data()?;
}
// recovery: replay() stops at the bad frame; open() set_len()s the rest away
```

**Suggested fix:** Track the last-known-good WAL offset; on any append error, set_len back to it (restoring the append-only tail invariant) and revert or re-validate the in-memory mutation - or poison the Store so the engine republishes the last durable model, mirroring restore_model.

#### major-24. restore_model cannot undo the durable WAL append inside a failed commit_sandbox; a crash resurrects the aborted commit
- **Where:** `crates/epiphany-engine/src/lib.rs:1189`  |  **Category:** bug  |  **Found by:** engine

Store::commit_sandbox first runs set_batch (store.rs:514), which durably fsyncs the whole batch into the WAL, then clears the deltas and checkpoints (store.rs:525). If that checkpoint fails (disk full/IO error), define_with's error path restores the in-memory model from the last published version and returns Persist - the client is told the commit failed and live readers correctly never see it. But the batch is already durable in the WAL and nothing removes it: if the process crashes before some later definition op happens to checkpoint, recovery replays the batch into the base cube, so the 'failed' sandbox commit is half-applied after restart (base updated, sandbox deltas still present from the older snapshot). The restore comment claims the orphaned mutation 'cannot ride out', which is only true for the in-memory side. The same ack-vs-durable divergence exists for a sync_data failure after a complete WAL append in set_batch/set_pin (client gets an error, record survives recovery).

```
// The op may have mutated the store's in-memory model before
// failing (e.g. a structural edit applied, then the checkpoint's
// I/O failed). Restore it from the last published model so the
// orphaned mutation cannot ride out on the next successful commit:
writer
    .store
    .restore_model(state.published.load().model.clone());
```

**Suggested fix:** On the restore path, also truncate the WAL back to its pre-op offset (or immediately re-checkpoint from the restored model) so durable state matches the reported outcome; alternatively make commit_sandbox checkpoint-first like the reindexing edits so its WAL unit is never left straddling a failure.

#### major-25. Commit versions restart at 0 every boot: cross-restart ABA defeats the optimistic base-version check
- **Where:** `crates/epiphany-engine/src/lib.rs:306`  |  **Category:** concurrency  |  **Found by:** engine, concurrency-durability-lens

from_stores publishes every cube at version 0 and the shared commit IdGen restarts at 1 each boot (the field doc at lib.rs:284-287 explicitly notes 'the commit IdGen restarts each boot' - which is why dimension ids got their own persisted counter, but commit versions did not). Version numbers therefore repeat across restarts: a client (Excel add-in or web session) holding base_version N from before a restart will usually get a spurious Conflict, but once the new counter passes N again for that cube, its CAS can succeed against a state that is not the one it read (classic ABA) - a lost update the client believed was CAS-protected. Every version-keyed artifact (view cache keys, WS gap detection, client ETags) is likewise exposed to aliasing across restarts. ADR-0001 requires deterministic version ordering but never addresses version durability, so this is an unhandled gap rather than a documented decision.

```
let published = ArcSwap::from_pointee(Published {
    version: 0,
    model: store.model().clone(),
});
let state = Arc::new(CubeState {
    writer: Mutex::new(Writer { store, version: 0 }),
```

**Suggested fix:** Persist a high-water commit version (e.g. in the snapshot header or a per-data-dir counter file) and seed the IdGen past it on boot, exactly as next_dim_id already does for dimension ids.

#### major-26. Cross-cube rule references bypass the reader's security on the referenced cube
- **Where:** `crates/epiphany-calc/src/eval.rs:238`  |  **Category:** security  |  **Found by:** security-crate

The element deny mask is consulted only when 'ordinal == self.mask_target' (the queried cube). A rule in cube A referencing cube B (cross-cube references are a core feature, e.g. 'Sales.Revenue = Units * FX!Rate') pulls B's leaves through the same resolver with no mask for B and no cube-level check for B. Consequences: (1) a caller whose element ACL denies (B, Employee, CEO) still reads CEO-derived values through any cube whose rules reference B — the exact cross-cube leak class ADR-0033 closed for dimension reads is open for cell values; (2) put_rules (rule_routes.rs:91) requires only Rule:Write on the target cube and never validates access to referenced cubes, so an ADR-0023 'modeler' on cube A (Rule:Write + Cube:Read on A only) can author ['X'] = B!['secret',...] and read any cube's data through A. No ADR documents cross-cube rule reads as executing with the target cube's authority.

```
if ordinal == self.mask_target {
    if let Some(mask) = self.mask {
        if mask.denies_leaf(coord) {
            return Err(CalcError::AccessDenied);
        }
    }
}
```

**Suggested fix:** Build masks per referenced cube (the mask seam already threads through the resolver factory) and deny when the reader lacks Cube:Read on a referenced cube — or, minimally, have put_rules require Cube:Read on every cube the compiled source references, and document the residual element-ACL leak in an ADR.

#### major-27. Deleted flow owner disables element masking for scheduled runs (fail-open)
- **Where:** `crates/epiphany-api/src/authz.rs:227`  |  **Category:** security  |  **Found by:** security-crate

element_mask_for returns None — meaning NO mask, i.e. unrestricted reads — when the username no longer resolves to a principal. The scheduler (scheduler.rs:149-170) runs a flow as its recorded owner and only checks that an owner name is recorded, not that the user still exists. Deleting a user who owns scheduled flows (a routine offboarding action) therefore makes those flows read every referenced cube with element security switched off; a read-only flow's outcome passes authorize_outcome_as trivially (empty write set), so the run succeeds and its output/log persists where Flow:Read holders can see it. This directly contradicts the crate's own doc comment ('Same fail-closed semantics') and ADR-0033's stance that 'an unknown principal ... is treated as fully denied, for defense in depth'.

```
let security = state.security.lock().expect("security mutex");
let principal = security.principal(username)?;
if principal.is_admin {
    return None;
}
```

**Suggested fix:** Distinguish 'no restrictions' from 'unknown principal': return an all-deny sentinel (or make the scheduler refuse to fire a flow whose owner no longer exists, mirroring the existing 'flow has no owner' fail-closed branch).

#### major-28. Grants and element ACLs are never purged on user/group/cube deletion — permissions resurrect on name reuse
- **Where:** `crates/epiphany-security/src/store.rs:445`  |  **Category:** security  |  **Found by:** security-crate

delete_group removes the group from members but leaves every `grants` and `element_acls` row naming it; delete_user likewise (line 347) leaves all rows naming the username; and no API code purges rows keyed by Scope::Cube(name) or the element-ACL cube column when a cube is deleted (no set_grant/set_element_access cleanup exists outside security_routes.rs). 'Dangling grants are never consulted' holds only until the name is reused: hire a new 'ann' after deleting the old one and the new account silently inherits every grant and element-ACL entry of its predecessor; recreate a group name (set_user_groups even auto-registers unknown names, line 381-395) or a cube name and the stale rows spring back to life. ADR-0015 decision 6's tolerate-dangling policy was justified for cube-model objects living in a DIFFERENT artifact; users and groups live in the same security.model as the grants, so purging is trivially possible.

```
pub fn delete_group(&mut self, name: &str) -> Result<bool, SecurityError> {
    let removed = self.groups.remove(name);
    if removed {
        for user in self.users.values_mut() {
            user.groups.remove(name);
        }
        self.save()?;
    }
    Ok(removed)
```

**Suggested fix:** On delete_user/delete_group, strip the principal from every AccessList in `grants` and `element_acls` (and drop now-empty lists) before saving; expose a purge-by-cube helper for the cube-delete endpoint.

#### major-29. WebSocket broadcasts base-write coordinates (cube + element names) to every authenticated user
- **Where:** `crates/epiphany-api/src/ws.rs:45`  |  **Category:** security  |  **Found by:** security-crate, api-auth-http

visible_to delivers every base-write CellsChanged event — carrying the cube name and the full coordinate as dimension→element NAME maps — to all authenticated subscribers; only sandbox events are filtered. A user with no Cube:Read on 'Salaries' and an element ACL denying them 'CEO' still receives {cube:"Salaries", coords:[{Employee:"CEO",...}]} on every write, disclosing the existence of denied members, cube names, and write activity/timing. This defeats the name-suppression guarantee ADR-0015 (deny-the-name in get_cube) and ADR-0033 (union-masked global dimension reads) implement everywhere else on the read surface.

```
fn visible_to(event: &ChangeEvent, username: &str, is_admin: bool) -> bool {
    match event {
        ChangeEvent::CellsChanged {
            owner: Some(owner), ..
        } => is_admin || owner == username,
        _ => true,
    }
}
```

**Suggested fix:** Filter CellsChanged per subscriber by cube_access >= Read, and either strip coords (clients already refetch on any event) or drop events whose coords hit the subscriber's element mask.

#### major-30. put_element_acl skips ADR-0015's validate-on-grant: a typo'd restriction is silently inert
- **Where:** `crates/epiphany-api/src/security_routes.rs:547`  |  **Category:** security  |  **Found by:** security-crate

The element-ACL endpoint stores whatever cube/dimension/element strings the admin sends, with no existence check against the model snapshot. Lookups are exact case-sensitive BTreeMap keys (store.rs:493, authz.rs:241), so 'north' vs 'North' or any typo produces a dangling ACL that never matches — the admin believes the member is restricted while every cube reader keeps full access. Because element security is a restriction list, this misconfiguration fails OPEN (unlike a typo'd per-kind grant, which merely under-grants). ADR-0015 decision 6 explicitly promises the opposite: 'the grant endpoint validates the object exists against the current snapshot before writing'. put_grant's Scope::Cube name is similarly unvalidated (that direction at least fails closed).

```
state
    .security
    .lock()
    .expect("security mutex")
    .set_element_access(&body.cube, &body.dimension, &body.element, &subject, level)
    .map_err(map_security_err)?;
```

**Suggested fix:** Before set_element_access, resolve the cube snapshot, the dimension by name, and the element by name; 404/422 on any miss. Same for put_grant's cube-scoped grants.

#### major-31. Tolerant load silently drops unparseable element-ACL rows — restrictions vanish and are erased by the next save
- **Where:** `crates/epiphany-security/src/store.rs:731`  |  **Category:** security  |  **Found by:** security-crate

from_model_text skips any element_acl row whose level or subject_kind fails to parse. For allow-grants skipping is fail-closed, but element ACLs are deny-by-restriction: skipping the only row(s) for an element removes the AccessList entirely, so the member flips to unrestricted for every cube reader. Since security.model is a model-as-code artifact (hand-editable, version-controllable per ADR-0003/0015), a single token typo like level = "Read" silently unprotects confidential members on the next server start — and the next save() re-serializes from memory, permanently deleting the row with no trace. ADR-0016 d6 blesses tolerant load, but that analysis covered allow-rows where a skip narrows access; the fail-open interaction with the restriction-list semantics was never decided.

```
for row in doc.element_acls {
    if let (Some(level), Some(subject)) = (
        AccessLevel::parse(&row.level),
        subject_from(&row.subject_kind, &row.subject),
    ) {
        element_acls
            .entry((row.cube, row.dimension, row.element))
            .or_default()
            .set(&subject, level);
```

**Suggested fix:** Fail closed for element_acl rows specifically: treat an unparseable row as deny-all for that (cube, dimension, element) (insert an empty-but-present AccessList), or reject the artifact with SecurityError::Format.

#### major-32. Element mask build: O(elements) triple-String allocations and O(total-ACLs) scans under the global security mutex
- **Where:** `crates/epiphany-security/src/store.rs:493`  |  **Category:** performance  |  **Found by:** security-crate

authz.rs::element_mask (built per cell-read/view/query request) loops every element of every ACL-carrying dimension, and each element_readable call allocates three fresh Strings just to probe the BTreeMap; has_element_acls (line 527) linearly scans ALL element-ACL keys with string compares, per dimension, per request. All of this runs while holding state.security — the single mutex every auth gate, login, and grant check in the server contends on. One ACL on a 500k-element dimension means ~1.5M heap allocations per read request, serialized server-wide. ADR-0015's stated performance force is that element security 'must cost nothing when no element rules exist and be O(1) per coordinate component otherwise'.

```
self.element_acls
    .get(&(cube.to_string(), dim.to_string(), element.to_string()))
    .map(|list| list.level_for(&principal.username, &principal.groups))
// and:
pub fn has_element_acls(&self, cube: &str, dim: &str) -> bool {
    self.element_acls
        .keys()
        .any(|(c, d, _)| c == cube && d == dim)
```

**Suggested fix:** Re-key element_acls as BTreeMap<(cube, dim), BTreeMap<element, AccessList>> so has_element_acls is one O(log n) probe and the mask build iterates only the ACL'd elements of that dimension (denying by name lookup into the dimension), with zero per-element allocation; or clone the small per-(cube,dim) ACL slice and drop the mutex before scanning.

#### major-33. WebSocket streams survive logout, password change, session expiry, and privilege revocation
- **Where:** `crates/epiphany-api/src/ws.rs:62`  |  **Category:** security  |  **Found by:** security-crate, api-auth-http

AuthPrincipal is checked only at upgrade time; the pump loop runs until the socket closes and never re-validates the session or re-resolves the principal. Logout (SessionStore::revoke), a password change (revoke_user_except, whose stated ADR-0017 goal is that 'a stolen session elsewhere cannot outlive the change'), an admin reset/delete (revoke_user), and the absolute/idle TTLs all leave established WebSocket connections streaming indefinitely. is_admin is also frozen at upgrade: a demoted admin (or a user whose session was revoked) keeps receiving other users' private sandbox CellsChanged events (visible_to grants admins everything) for the lifetime of the connection. An attacker holding a stolen session keeps a live feed of all change activity even after the victim rotates their password.

```
let receiver = state.events.subscribe();
let username = auth.principal.username;
let is_admin = auth.principal.is_admin;
upgrade.on_upgrade(move |socket| pump(socket, receiver, username, is_admin))
```

**Suggested fix:** In the pump loop, periodically (or per delivered event) re-check state.sessions.lookup(&token, now) and re-resolve is_admin from the live security store, closing the socket when the session is gone; alternatively have SessionStore revocation notify a per-token shutdown channel that pump selects on.

#### major-34. Argon2 verification runs under the global security mutex on a tokio worker: unauthenticated whole-API DoS
- **Where:** `crates/epiphany-api/src/auth.rs:144`  |  **Category:** security  |  **Found by:** api-auth-http

login calls SecurityStore::authenticate while holding the state.security std::sync::Mutex, and authenticate always performs a production-cost Argon2id verify (~50-100ms; a dummy verify even for unknown usernames, by design). Every other request's authorization path (require_admin, cube_level, effective, element_mask - all in authz.rs) locks the same mutex, and the lock is a blocking std mutex acquired inside async handlers. The per-username lockout does not help: an attacker rotating random usernames never trips is_locked, and each request pins the mutex for the full KDF duration while also blocking a tokio worker thread with synchronous CPU work. A handful of concurrent bogus login POSTs therefore stalls every authenticated endpoint on the server, unauthenticated. change_password (line 258) has the same shape. ADR-0017 defers per-IP throttling, but serializing the entire API's authorization behind the KDF is an implementation choice, not part of that decision.

```
let authenticated = state
    .security
    .lock()
    .expect("security mutex")
    .authenticate(&req.username, &req.password);
```

**Suggested fix:** Clone the stored password hash (or the dummy hash) out under the lock, drop the guard, and run the Argon2 verify via tokio::task::spawn_blocking; re-acquire the lock only to build the Principal. Optionally add a small global concurrency cap on in-flight verifies.

#### major-35. Cellset execution has no size cap: any reader can OOM the server with a crossjoin view
- **Where:** `crates/epiphany-api/src/query_routes.rs:709`  |  **Category:** performance  |  **Found by:** api-model-endpoints

execute_adhoc, execute_mdx, and execute_saved_view pass the user-supplied axis specs straight into core execute_view, which eagerly materializes the full row and column tuple crossjoins (query.rs crossjoin(), no cap) and then a dense nrows*ncols Fixed grid (Vec::with_capacity(total)), followed by a per-cell DTO with a String value. Nothing bounds the product: an authenticated user with mere Cube:Read can POST /cubes/{cube}/cellset with two multi-thousand-member dimensions crossjoined per axis (ADR-0032 explicitly targets dimensions with thousands of members) and drive allocation into the tens of GB, aborting or thrashing the whole in-memory server — all cubes, all users. The codebase already recognizes this class of risk: spreading is capped at MAX_SPREAD_LEAVES=200_000 and the view cache refuses to store cellsets over MAX_CACHE_CELLS=1_048_576, but that cache ceiling only skips caching; computation and serialization are unbounded. The 8 MiB body cap does not help since a few axis specs expand combinatorially.

```
|| {
    let resolver = state.cells.resolver_with(snap, sandbox, mask.as_ref());
    execute_view(
        snap.cube(),
        view,
        &*resolver,
        &|d, n| snap.subset(d, n),
        state.evaluator(),
```

**Suggested fix:** Before executing, compute the axis tuple-product size (per-spec member counts are known after resolve) and reject over a hard cap with a 422 (mirroring SPREAD_TOO_LARGE), or add the cap inside execute_view/resolve_axis and map it to a typed error.

#### major-36. Versionless writes race concurrent structural edits: name-to-index resolution can land the write on the wrong element
- **Where:** `crates/epiphany-api/src/routes.rs:223`  |  **Category:** concurrency  |  **Found by:** api-model-endpoints

write_cell (and spread_cells at line 355, and batch_write when the client omits base_version) resolves element names to positional indices against a pinned snapshot, then submits index-addressed CellWrite::Leaf coords via apply_batch(cube, None, ...). An ADR-0036 structural edit (delete/insert/reorder) committed between the resolve and the apply remaps element indices — the engine remaps stored cells and sandbox overlays, but it cannot remap an in-flight write. apply_batch only checks that the index is in range and leaf-kind, so the stale index silently writes a different element: user writes 'North', the value lands on 'South', and the response re-read (which re-resolves the name on a fresh snapshot, line 246) even reports the write as not having taken. Silent wrong-cell data with no conflict error. The optimistic base_version check would catch this, but write_cell and spread_cells hardcode None and offer the client no way to supply one.

```
let (write, sandbox_name) = {
    let snap = snapshot(&state, &cube)?;
    ...
};
let outcome = match &sandbox_name {
    Some(name) => state.engine.sandbox_set_cells(&cube, None, name, &[write]),
    None => state.engine.apply_batch(&cube, None, &[write]),
}
```

**Suggested fix:** Pass the resolving snapshot's version as the base for versionless writes and map the 409 to an internal retry (re-resolve names on the new snapshot), or extend the engine batch API to carry a resolved-at version so apply_batch can reject index-addressed writes older than the last structural remap.

#### major-37. Proportional spread overflows i64 when weights nearly cancel, writing garbage values
- **Where:** `crates/epiphany-core/src/spread.rs:157`  |  **Category:** olap  |  **Found by:** api-model-endpoints

distribute_proportional computes each share as ((t * w as i128) / sum) as i64. The zero-sum fallback triggers only when the weights sum to EXACTLY zero; when large positive and negative leaf values nearly cancel (e.g. leaves +1e12 and -1e12+0.0001, sum = 1 scaled unit), the quotient t*w/sum vastly exceeds i64::MAX and the `as i64` cast silently truncates bits, producing arbitrary garbage shares. The leftover then also exceeds the leaf count, so allocate_remainder stops early and the writes no longer sum to the entered total — breaking the module's stated exactness invariant. Reached directly from POST /cubes/{cube}/cells/spread with method=proportional over any consolidation whose current leaf values include large offsetting positives and negatives (routine in signed financial data). Consequence: committed wrong numbers, no error.

```
let t = total as i128;
let mut out: Vec<i64> = weights
    .iter()
    .map(|&w| ((t * w as i128) / sum) as i64)
    .collect();
```

**Suggested fix:** Keep shares in i128 and check each against i64 range (and check the final leftover bound), returning a typed SpreadError (e.g. SpreadError::Overflow -> 422) instead of casting; alternatively fall back to Equal whenever any share is unrepresentable.

#### major-38. explain_cell leaks values and coordinates from cross-cube referenced cubes without access check or mask
- **Where:** `crates/epiphany-api/src/rule_routes.rs:247`  |  **Category:** security  |  **Found by:** api-workspaces

explain_cell requires Cube:Read only on the path cube and builds an element mask solely for that cube's snapshot. When the target cube's rule references another cube (a supported cross-cube reference, see eval.rs value(ordinal,..)), explain_node recurses into the referenced cube ordinal and records that cube's cell value plus its coordinate member names in the trace inputs. CalcEngine::with_mask is scoped to a single mask_target (eval.rs:238 `if ordinal == self.mask_target`), so the deny mask does not apply to the referenced cube, and no Cube:Read grant on that cube is ever checked. A caller with only Cube:Read on the target can thus read numeric values and member names from a cube they have no grant on (and past element ACLs on that other cube) via the provenance trace.

```
let mask = element_mask(&state, &auth, &snap); // built for target `snap` only
let trace = explain_with(&registry, ordinal, &resolved.indices, depth,
    overlay.as_ref().map(|o| o as &dyn SandboxOverlay), mask.as_ref())
// provenance.rs: inputs.push(explain_node(engine, registry, cell.cube, &abs, remaining-1)?)
```

**Suggested fix:** When explain descends into a cell whose cube differs from the target ordinal, either stop expanding (omit cross-cube inputs) or re-check the caller's Cube:Read + element mask for that cube ordinal before including its value/coords; deny (403) if the caller lacks read access to a referenced cube.

#### major-39. Command connector hangs forever if a grandchild keeps the stdout pipe open after the child exits
- **Where:** `crates/epiphany-connect/src/lib.rs:166`  |  **Category:** concurrency  |  **Found by:** server-connect

The poll loop only enforces the timeout while the direct child is alive. Once the child exits (Ok(Some(status))), the code joins the stdout/stderr reader threads with no deadline. If the admin-defined program spawned a background grandchild (e.g. a script that launches a daemon, or python starting a helper) the pipe write-end stays open, read() never returns EOF, and join() blocks forever. The flow run or preview is stuck permanently: the preview endpoint blocks a tokio worker thread indefinitely, and a scheduled run's spawn_blocking thread leaks with the run ledger showing it running forever. The module doc only acknowledges the timeout-kill limitation for grandchildren; this hang occurs even on a fast, clean exit. Additionally, the try_wait Err arm at line 162 returns without killing the child, orphaning it.

```
    let (out_bytes, overflow) = out_reader.join().unwrap_or((Vec::new(), false));
    let (err_bytes, _) = err_reader.join().unwrap_or((Vec::new(), false));
```

**Suggested fix:** Bound the reader-thread wait by the remaining timeout (e.g. poll a shared AtomicBool/channel with a deadline instead of a blocking join, returning Timeout if the pipes do not reach EOF in time), and kill the child in the try_wait Err arm. Longer term, use process groups / Windows job objects so the whole tree is terminated.

#### major-40. HTTP connector treats 3xx redirect responses as success, silently yielding zero rows
- **Where:** `crates/epiphany-connect/src/http.rs:56`  |  **Category:** bug  |  **Found by:** server-connect

The agent is built with .redirects(0) (correct for SSRF), but in ureq 2.x a redirect response is then returned as Ok - ureq only maps status >= 400 to Error::Status, so map_ureq_error never sees a 301/302. The 3xx body is parsed as CSV/JSON; since redirect bodies are typically empty, parse_output returns Ok(vec![]) (empty output means no rows by design). Consequence: the moment a configured feed URL starts redirecting - the extremely common http->https or trailing-slash 301 - every fetch silently succeeds with zero rows. A flow that ingests-and-replaces then quietly writes nothing or clears data instead of failing loudly. This affects both previews and scheduled runs.

```
        // No redirects: the fetch must only reach the configured (allowlisted)
        // host, so a 3xx cannot steer it to an internal host (SSRF).
        .redirects(0)
        .build();
...
    let response = req.call().map_err(map_ureq_error)?;
```

**Suggested fix:** After req.call(), explicitly reject non-2xx statuses: if response.status() is 3xx (or anything outside 200..300), return ConnectError::HttpStatus with the code and Location header so the operator sees why the fetch produced no data.

#### major-41. body.truncate(2048) panics when byte 2048 is not a UTF-8 char boundary in a remote error body
- **Where:** `crates/epiphany-connect/src/http.rs:79`  |  **Category:** bug  |  **Found by:** server-connect

String::truncate panics if the new length does not lie on a char boundary. The string being truncated is the error-response body of the remote server (into_string()), so any allowlisted endpoint that returns a >2048-byte 4xx/5xx body containing multi-byte characters (unicode quotes in an HTML error page, non-English JSON error messages) can land a multi-byte character straddling byte 2048 and panic the thread. Instead of a clean HttpStatus error surfaced to the user, a preview request's handler task aborts (connection reset / 500) and a scheduled flow run dies with an opaque JoinError-style failure. The panic is triggered by remote input on an error path that exists precisely to report errors cleanly.

```
        ureq::Error::Status(code, response) => {
            let mut body = response.into_string().unwrap_or_default();
            body.truncate(2048);
            ConnectError::HttpStatus { code, body }
        }
```

**Suggested fix:** Truncate on a char boundary: e.g. while !body.is_char_boundary(n) { n -= 1; } body.truncate(n); or truncate the raw bytes and rebuild with String::from_utf8_lossy.

#### major-42. SQL connectors enforce the row cap only after buffering the entire result set in memory
- **Where:** `crates/epiphany-connect/src/sql.rs:191`  |  **Category:** performance  |  **Found by:** server-connect

Both drivers materialize every row before the cap check: tokio-postgres client.query() collects the full result set into Vec<Row>, and the mysql path does the same via conn.query() (lines 348-355). The MAX_CSV_ROWS cap - documented as the memory-exhaustion backstop mirroring the 16 MiB stdout cap - therefore fires only after the memory has already been spent. An admin who points a connection at SELECT * FROM a large table (a plausible mistake the cap exists to catch) can pull gigabytes into the server's heap within the 30s timeout before the 'more than N rows' error is produced, driving an in-memory OLAP server into OOM and taking down every user. The command and HTTP connectors enforce their caps during the read; SQL is the only connector whose limit is enforced after the fact.

```
        let rows = client.query(sql, &[]).await.map_err(sql_err)?;
        if rows.len() > cap {
            return Err(ConnectError::Sql(format!(
                "the query returned more than {cap} rows"
            )));
        }
```

**Suggested fix:** Stream instead of collecting: tokio_postgres::Client::query_raw returns a RowStream - take cap+1 rows and error on overflow; mysql_async offers query_stream / for_each equivalents. This makes the cap an actual memory bound.

#### major-43. A fresh CalcEngine (and memo) per cell read defeats the ADR-0007 per-query memo across a cellset
- **Where:** `crates/epiphany-api/src/calc_factory.rs:174`  |  **Category:** performance  |  **Found by:** olap-core-semantics, olap-query-semantics, perf-lens

ADR-0007 pins 'Results are memoized for the life of one query ... so a cell referenced many times in a cellset is computed once.' CalcCellResolver::value constructs a brand-new CalcEngine (empty memo) for every single value read, so nothing is shared across the cells of one view execution: shared rule inputs, cross-cube scalars, and consolidated inputs are fully recomputed per output cell. Combined with the dense consolidation this multiplies an already-superlinear cost by the cellset size (an N-cell view of a rule that reads a consolidated input recomputes that whole rollup N times). The regression was introduced to make the resolver Sync for parallel grid fill (RefCell memo is not Sync), i.e. parallelism was bought by discarding the memo the ADR mandates.

```
/// evaluator (and memo) is used per value read.
fn value(&self, coord: &[u32]) -> Result<Fixed, QueryError> {
    let engine = match &self.overlay {
        Some(overlay) => CalcEngine::with_overlay(&self.registry, overlay),
        None => CalcEngine::new(&self.registry),
    }
```

**Suggested fix:** Give the resolver a Sync memo (e.g. sharded Mutex<HashMap> or a lock-free map keyed by (scope, ordinal, coord)) or per-worker engines whose memos live for the whole view execution, preserving the bit-identical within-cell reduction order.

#### major-44. Unconstrained rule areas never apply at consolidated cells, so ratio/non-additive rules silently aggregate as sums of ratios at every total
- **Where:** `crates/epiphany-calc/src/compiled.rs:72`  |  **Category:** olap  |  **Found by:** olap-core-semantics

DimPredicate::Any matches leaf members only, and there is no level qualifier (no N:/C: marker exists in the grammar - SelectorKind offers only element/all/leaves/consolidated/children/descendants). Incumbent engines apply an unprefixed rule at both N and C levels, so 'Margin% = Margin/Sales' computes ratio-of-totals at consolidations. Here the same natural rule fires only at leaves and every consolidated cell SUMS the leaf ratios (Total Margin% = 60% + 25% = 85%) - a silently absurd number for the most common class of finance measures. Expressing the correct behavior requires enumerating '{all}' (or every consolidated member) on EVERY otherwise-unconstrained dimension of the area, which nothing prompts the author to do. This default is documented only in a code comment; no ADR pins it, and the leaf-only choice makes the most idiomatic rule text produce wrong totals for any non-additive formula.

```
/// An unconstrained dimension (`Any`) matches only LEAF members, so a rule
/// that leaves a dimension free computes leaf values and lets consolidations
/// roll those up; overriding a consolidated cell requires explicitly naming
/// the consolidated element (an `OneOf` set that includes it).
DimPredicate::Any => cube.dimension(d).element(idx).map(|e| e.kind.is_leaf())
```

**Suggested fix:** Add an explicit level marker to the rule grammar (e.g. 'N:' / 'C:' prefixes or an 'at consolidated' area flag) so an author can state 'compute this formula at consolidated cells too' without enumerating members, and document the leaf-only default in an ADR.

#### major-45. Writes to rule-covered leaves are accepted, stored, and silently shadowed; cell editability and spreading ignore rules entirely
- **Where:** `crates/epiphany-api/src/routes.rs:221`  |  **Category:** olap  |  **Found by:** olap-core-semantics

No write path (write_cell, batch_write, sandbox_set_cells, spread_cells, rule-test fixtures) checks whether the target leaf is covered by a rule area. The write succeeds, the stored value is persisted, and every subsequent read returns the rule value instead - the user's number simply vanishes (write_cell even re-reads and returns the rule value, not what was written), and the shadowed stored value resurrects unpredictably if the rule is later edited or (per the silent compile-failure fallback) dropped. CellDto.editable is derived purely from all_leaf (routes.rs:411/420), so the UI actively invites typing into rule-calculated cells. spread_cells is worse: spread_leaves distributes over ALL contributing leaves including rule-covered ones and reads its proportional basis through the rule-aware resolver, so the entered total is not reproducible on read-back. Incumbent engines reject data writes to rule-calculated cells and skip them during spreading.

```
let outcome = match &sandbox_name {
    Some(name) => state.engine.sandbox_set_cells(&cube, None, name, &[write]),
    None => state.engine.apply_batch(&cube, None, &[write]),
}
// routes.rs:411
editable: resolved.all_leaf,
```

**Suggested fix:** At the API layer (which has the compiled model), reject writes whose coordinate matches a rule area (matching_rule), mark such cells editable:false in DTOs, and exclude rule-covered leaves from spread expansion (rejecting the spread if all leaves are rule-covered).

#### major-46. A single failing cell (e.g. division by an empty cell) aborts the entire cellset instead of yielding a per-cell error value
- **Where:** `crates/epiphany-core/src/query.rs:603`  |  **Category:** olap  |  **Found by:** olap-core-semantics

execute_view propagates the first cell error out of the whole grid fill (serial: `grid.push(cell_at(r, c, &mut scratch)?)`; the parallel path reproduces the same first-error semantics), and Cellset.cells is Vec<Fixed> with no per-cell error representation. Rule division is a hard error (eval.rs arith: Div by zero -> CalcError::DivByZero). Consequence: one ratio rule evaluated at a coordinate whose denominator cell is unpopulated - the default state of any sparse cube slice - turns EVERY view, MDX query, and Excel read that includes that cell into a 422, hiding all the other values. Classic engines render a per-cell error marker and keep the rest of the grid. Zero-suppression cannot save the user because the error fires before suppression.

```
for r in 0..nrows {
    for c in 0..ncols {
        grid.push(cell_at(r, c, &mut scratch)?);
    }
}
// eval.rs:414
ArithOp::Div => { if sb == 0 { return Err(CalcError::DivByZero); }
```

**Suggested fix:** Make the cellset value type carry per-cell outcomes (value | error tag) and degrade DivByZero/Overflow/Cycle to a per-cell error rendered by the API, keeping hard failures only for structural errors (unknown members, coverage).

#### major-47. Sandbox cache scope id is not unique across restarts: one user's what-if cellset can be served for another sandbox
- **Where:** `crates/epiphany-api/src/view_cache.rs:106`  |  **Category:** security  |  **Found by:** olap-query-semantics

The cache scopes sandboxed entries by sandbox.created (an engine IdGen commit id). Sandboxes persist their `created` stamp in the model (text.rs SandboxDoc round-trips it), but the engine's IdGen restarts at 1 on every boot (boot.rs:65 `Engine::from_stores(stores, Arc::new(IdGen::default()))`). A sandbox created before a restart can therefore hold the same `created` id as a sandbox created after it. Two live sandboxes on the same cube with colliding ids — possibly owned by different users, holding different what-if overrides — produce identical `sandbox_scope` key components. If both owners execute the same view shape at the same cube version, the second reader gets a cache hit on the first reader's sandboxed cellset and sees the other user's what-if numbers. This directly breaks ADR-0028's 'two distinct sandboxes never alias' invariant, which the design calls its primary fail-closed risk.

```
// Same scope id the calc memo uses (ADR-0014), so two distinct sandboxes
// never alias and a base read keys as None.
let sandbox_scope = read.sandbox.map(|s| s.created.max(1));
```

**Suggested fix:** Make the scope id restart-unique: seed the engine IdGen past the max id found in loaded models (as next_dim_id already does for dimension ids), or key sandboxed entries on (cube, sandbox name, owner, created) losslessly.

#### major-48. Consolidation tie-break depends on edge-declaration order that serialization does not preserve — values can change across a restart
- **Where:** `crates/epiphany-core/src/dimension.rs:154`  |  **Category:** determinism  |  **Found by:** determinism-lens

ADR-0039 resolves equal-depth multi-parent diamonds by edge-declaration order: leaf_weights (dimension.rs:383-401) does a BFS that enqueues `self.children[node]` in declaration (add_child) order, and the first path to reach a leaf fixes its net weight. But the canonical serializer emits edges via `edges()`, which sorts by (parent, child) index (line 154), and the loader (text.rs:1134-1147) replays `add_child` in that sorted order. So a live dimension whose edges were declared in non-index order (trivially reachable via the dimension-editing API, extend_schema, or flow ensure/addChild calls) has a different declaration order before a restart than after reload from the snapshot. For a diamond where the same leaf is reachable at equal depth with different net weights (e.g. leaf X under both Actual(+1) and Budget(-1) beneath Variance — the canonical weighted-hierarchy shape in this domain), the recorded weight flips (e.g. +1 to -1), silently changing consolidated cell values after a server restart or model export/import with no user edit. This violates the binding identical-inputs/identical-outputs mandate and is invisible to the round-trip tests, which compare serialized bytes (stable, sorted) rather than the reloaded dimension's edge order.

```
/// All consolidation edges as `(parent, child, weight)`, sorted canonically
/// by `(parent, child)` for deterministic, diff-friendly output.
pub fn edges(&self) -> Vec<(u32, u32, i64)> {
    ...
    out.sort_by_key(|&(parent, child, _)| (parent, child));
// dimension.rs:368-369 (leaf_weights doc): "ties (same depth via
// different edges) are broken deterministically by edge-declaration order"
```

**Suggested fix:** Make the persisted order the semantic order: either serialize edges in declaration order (elements are already emitted in definition order, so this stays canonical for identical histories), or normalize the in-memory declaration order to the sorted (parent, child) order at add_child/reparent time so the live model and its round-trip always agree. Add a test that builds a diamond with out-of-index-order edge declarations and unequal net path weights, checkpoints, reloads, and asserts the consolidated value is unchanged.

#### major-49. Failed definitional op can leave the on-disk snapshot ahead of the restored in-memory model; later WAL appends replay onto a different element order
- **Where:** `crates/epiphany-engine/src/lib.rs:1189`  |  **Category:** olap  |  **Found by:** concurrency-durability-lens

define_with's error path restores only the in-memory model from the last published version, but checkpoint() may have already durably renamed the NEW snapshot into place before failing on the WAL-clear step (write_snapshot succeeds, then wal.set_len/write/sync fails). For a reindexing edit (reorder/delete/insert via Store::reorder_elements etc.), disk then holds the POST-edit snapshot while the live server continues on the PRE-edit model the client was told still stands. Subsequent cell commits validate and log coordinates against the pre-edit element order, fsync, and are acknowledged; after a crash, recovery loads the post-edit snapshot and replays those pre-edit indices onto the permuted order — values silently land on the wrong elements (wrong numbers). Even for non-reindexing defines (subset/view/rules), the "failed" definition resurrects after a crash despite the error the client received. The divergence heals only on the next successful checkpoint, which a cell-write-only workload never performs.

```
// The op may have mutated the store's in-memory model before
// failing (e.g. a structural edit applied, then the checkpoint's
// I/O failed). Restore it from the last published model so the
// orphaned mutation cannot ride out on the next successful commit:
writer
    .store
    .restore_model(state.published.load().model.clone());
```

**Suggested fix:** After a checkpoint failure, treat disk state as unknown: either fail-stop the cube's writer until a successful re-checkpoint from the restored model, or immediately retry/force a checkpoint of the restored model before accepting further writes (and surface a loud operator error if that also fails).

#### major-50. No data-directory lock: a second server process on the same data dir corrupts the WAL silently instead of failing cleanly
- **Where:** `crates/epiphany-server/src/boot.rs:49`  |  **Category:** olap  |  **Found by:** concurrency-durability-lens

store.rs:15-18 documents the single-process assumption ("the store does not take an OS file lock, so concurrent processes over the same directory are unsupported") but nothing enforces it: boot::load_or_init and Store::open/create acquire no lock file, and on Windows Rust's std opens files with full share flags, so a second instance (double-started service, foreground run beside the installed service — an easy operator mistake) opens the same wal.log successfully. Each process holds an independent write-mode handle (not O_APPEND) whose cursor starts at its own replay end, so both append at overlapping offsets and overwrite each other's fsynced, acknowledged frames; each checkpoint truncates the WAL under the other's cursor, and snapshot renames are last-writer-wins. The failure mode is silent: the loser's acknowledged writes vanish and the interleaved bytes are quietly truncated as a "torn tail" at the next recovery. No ADR blesses leaving this unguarded — the docs only state the assumption.

```
let mut stores = BTreeMap::new();
for path in dirs {
    let store = Store::open(&path)?;
    stores.insert(store.cube_name().to_string(), store);
}
```

**Suggested fix:** Take an exclusive advisory lock on a <data_dir>/lock file at boot (fs2-style flock / Windows LockFileEx, or an O_EXCL pid file with staleness check) and exit with a clear "data directory is already in use" error, turning silent corruption into a clean failure.

#### major-51. Shared-dimension registry persistence is best-effort and never fsynced, contradicting ADR-0024's fail-closed durable-registry-first rule; a lost persist can resurrect deleted elements
- **Where:** `crates/epiphany-engine/src/lib.rs:455`  |  **Category:** olap  |  **Found by:** concurrency-durability-lens

ADR-0024 states "the registry grow's CAS + durability + publish is one fail-closed critical section (durable registry write happens *before* any cube repack)" and calls the registry "always the durable authority". The implementation swallows every save error (`let _ = save_registry(...)`), and save_registry itself uses fs::write + rename with no fsync anywhere, so even a "successful" save may not survive power loss. Consequences: register_dimension/attach_dimension return success to the API while the durable entry may be silently lost (an unreferenced registered dimension vanishes entirely; a lost attach means future grows/edits silently stop fanning out to that cube). Worst case: edit_dimension applies a structural Delete — registry published, persist silently fails, the element and its cells are durably deleted from every referencing cube — then on restart with_dimensions_dir loads the OLD registry entry and its reconcile pass re-appends the deleted element and its rollup edges into every cube (define_elements is append-only), so the element resurrects with its data gone and consolidations silently change.

```
/// Persist the current registry to `dimensions_dir`, if durable. Best-effort:
/// a write failure is not fatal because every referencing cube already holds
/// its own durable copy of the dimension (the registry reconciles on reload).
fn persist_registry(&self) {
    ...
    let _ = save_registry(dir, &entries);
}
```

**Suggested fix:** Make persist_registry fallible and propagate the error before fanning out (fail-closed, per ADR-0024); fsync the .model temp files and the index before rename (mirror write_snapshot); at minimum log loudly on failure instead of discarding the error.

#### major-52. A corrupt or torn registry silently loads as an empty registry (unwrap_or_default), and save_registry's crash window can produce exactly that
- **Where:** `crates/epiphany-engine/src/lib.rs:336`  |  **Category:** olap  |  **Found by:** concurrency-durability-lens

with_dimensions_dir collapses ANY load error into an empty registry. load_registry hard-fails if index.toml parses but a referenced <id>.model is unreadable — and save_registry creates that state legitimately: it deletes orphaned <id>.model files BEFORE writing the new index.toml (registry.rs:63-77 run before the index rename at 89-91), so a crash between the orphan delete (after delete_dimension) and the index rename leaves the old index referencing a removed file. On the next boot the whole registry silently becomes empty: every shared dimension disappears from the library, all cube reference sets are lost so future grows/edits stop propagating (cubes silently diverge), and next_dim_id reseeds to max_id(0)+1 = 1, re-minting DimensionIds that were already handed out — with element security re-keyed to (DimensionId, element) per ADR-0024, id reuse can attach old semantics to a new dimension. The operator gets no warning at all.

```
let entries = load_registry(&dir).unwrap_or_default();
// registry.rs: orphan delete BEFORE the new index is written
if !keep.contains(&id) {
    let _ = std::fs::remove_file(&path);
}
...
std::fs::rename(&tmp, dir.join(INDEX_FILE))?;
```

**Suggested fix:** In save_registry, delete orphaned .model files only AFTER the new index rename succeeds; in with_dimensions_dir, distinguish "absent" (empty) from "present but unreadable" — fail boot or quarantine the registry with a loud error instead of unwrap_or_default; make load_registry skip (with a warning) an entry whose body is missing rather than failing the whole load.

#### major-53. Synchronous fsync and full-model checkpoints run inline in async handlers while holding the per-cube writer std::sync::Mutex
- **Where:** `crates/epiphany-api/src/routes.rs:223`  |  **Category:** performance  |  **Found by:** concurrency-durability-lens

write_cell/batch_write/spread_cells (and all sandbox/definition routes) call Engine::apply_batch / define_* directly from async handlers. Those take a std::sync::Mutex writer lock and then perform blocking disk I/O under it: at least one WAL fsync per commit, and for every definitional or sandbox op a full checkpoint — serializing the ENTIRE model (all cells) to text plus two fsyncs (store.rs:322-329, write_snapshot). ADR-0013 deliberately routes flow work through spawn_blocking, but the HTTP commit path was not. Each concurrent write to the same cube OS-blocks a tokio worker thread on the mutex + fsync; with the default worker count (= cores), a handful of concurrent writers — or one writer with a large cube checkpoint on a slow disk — stalls the entire runtime, including the supposedly lock-free snapshot reads and health endpoints, which still need a runtime thread. This is the reader-starvation path in practice.

```
let outcome = match &sandbox_name {
    Some(name) => state.engine.sandbox_set_cells(&cube, None, name, &[write]),
    None => state.engine.apply_batch(&cube, None, &[write]),
}
.map_err(map_batch_error)?;
```

**Suggested fix:** Wrap engine mutation calls in tokio::task::spawn_blocking (the engine is Clone + Send, mirroring the scheduler's pattern at scheduler.rs:273), so writer-lock contention and fsync latency occupy blocking-pool threads instead of runtime workers.

#### major-54. Production consolidations enumerate the dense leaf product; sparse paths are never used
- **Where:** `crates/epiphany-calc/src/eval.rs:279`  |  **Category:** performance  |  **Found by:** perf-lens

The server always injects CalcFactory (crates/epiphany-server/src/main.rs:177), so every API read — read_cells, view execution, spread — resolves values through CalcEngine::compute. For any non-leaf coordinate it calls Cube::consolidate_with, which enumerates the full cartesian product of every dimension's descendant leaves (cube.rs:1220-1238, total = product of per-dim leaf counts), even when the cube has no rules at all (PinnedRegistry returns an empty CompiledModel, and compute() never falls back to the sparse Cube::get populated-cell scan). On a sparse cube — the design premise — this is catastrophic: a 4-dim cube with 1,000 leaves per dimension and only 10,000 populated cells turns one 'Total' cell read into ~1e12 recursive value() calls (hours, plus memo growth), where Cube::get would scan 10,000 entries in microseconds. The feeder-driven sparse union scan Cube::consolidate_fed (ADR-0005's performance mechanism) has zero production call sites — its only caller is a unit test (feeders.rs:526). ADR-0005 deliberately keeps the dense path as the correctness truth for rule evaluation, but nothing in the ADRs decides that rule-less cubes must abandon the sparse stored-cell scan, and the view benches validating the 1s cold budget (epiphany-core/benches/view_exec.rs) use a Cube::get-backed resolver production never runs.

```
} else {
    // Consolidate, pulling each contributing leaf back through the
    // resolver so rule-derived leaves are included with correct weights.
    cube.consolidate_with::<CalcError, _>(coord, |lc| self.value(ordinal, lc))
}
```

**Suggested fix:** In CalcEngine::compute, when the target cube's compiled model has no rules (or no rule can fire in the queried region), delegate consolidations to the sparse Cube::get / stored-cell scan. Wire consolidate_fed with the inferred FeederIndex for rule-bearing cubes so rollups scan populated ∪ fed instead of the dense product, keeping the dense path as the validation oracle only. Add a bench that measures the production CalcFactory resolver, not just StoredCells.

#### major-55. Every cube's rules re-parsed and re-compiled on every read request
- **Where:** `crates/epiphany-api/src/calc_factory.rs:224`  |  **Category:** performance  |  **Found by:** perf-lens

CalcFactory::resolver_with calls PinnedRegistry::build on every invocation, which snapshots every cube on the server and re-parses + re-compiles every cube's rule source from text (calc_factory.rs:87-98). resolver_with runs per request on the hottest endpoints: read_cells (routes.rs:186), the post-write re-read in write_cell (routes.rs:245), spread (routes.rs:331), the flow reader, and every view-cache miss. So a single-cell GET on a server with 50 rule-bearing cubes tokenizes, parses, and compiles 50 rule documents before reading one value — cost that grows with model size and rule size, paid per request, for an artifact that is immutable per (cube, version). The mandate and ADR-0007 are built around 'compiled rules', but there is no compiled-rules cache at all.

```
    ) -> Box<dyn CellResolver + Sync> {
        let registry = PinnedRegistry::build(&self.engine);
...
        let models = snaps
            .iter()
            .map(|s| {
                rules::parse(&s.rules().source)
                    .ok()
                    .and_then(|doc| compile(s.cube(), &cr, &doc, s.version()).ok())
```

**Suggested fix:** Cache CompiledModel per (cube, version) — e.g. a small ArcSwap/Mutex<HashMap<(String, u64), Arc<CompiledModel>>> in CalcFactory, invalidated implicitly by version keying like the view cache — and only compile the cubes actually reachable from the target cube's rules instead of all cubes.

#### major-56. Whole-cube deep clone per write batch for validation that needs no clone
- **Where:** `crates/epiphany-persist/src/store.rs:281`  |  **Category:** performance  |  **Found by:** perf-lens

Store::set_batch clones the entire cube (both cell HashMaps, all dimensions, the string pool) on every batch commit just to validate the writes, then throws the trial away after adopting it. Validation in set_leaf/set_string is purely coordinate-local (rank check, element-kind checks) and does not depend on other writes in the batch, so the clone buys nothing that a read-only validation pass would not. Engine::apply_batch then makes a second full clone to publish (engine/src/lib.rs:821, model: writer.store.model().clone()) — that one is the ADR-0001-documented copy-on-write publish, but the trial clone silently doubles it. Net effect: committing a B-cell batch to an N-cell cube costs O(N) time and ~2-3x cube memory transiently, under the writer lock; bulk-loading 100M cells in 10k-cell batches performs ~10,000 full-cube clones (O(N²/B) total), making the 1M cells/sec/core budget unreachable as the cube grows. sandbox_set_cells repeats the same full-cube clone per what-if write batch (store.rs:459) purely for coordinate validation.

```
// 1. Validate + apply to a throwaway clone; abort the whole batch on error.
let mut trial = self.model.cube.clone();
for (index, write) in writes.iter().enumerate() {
    let applied = match write {
        CellWrite::Leaf { coord, value } => trial.set_leaf(coord, *value),
        CellWrite::Str { coord, value } => trial.set_string(coord, value),
    };
```

**Suggested fix:** Replace the trial clone with a validate-only pass (the same kind/rank checks set_leaf/set_string perform, without mutation), then apply directly to the live cube after the WAL append — or record applied deltas for rollback. The publish clone remains ADR-0001's documented structural-sharing follow-up; removing the redundant validation clone halves the current cost immediately.

#### major-57. Blanket 401 handling rewrites login and change-password failures as "session expired"
- **Where:** `web/src/api/client.ts:149`  |  **Category:** bug  |  **Found by:** web-core

request() maps EVERY 401 to the uniform expired-session error before looking at the endpoint. But the server returns 401 for a wrong password at POST /auth/login (crates/epiphany-api/src/auth.rs:167, "invalid credentials") and for a wrong current password at POST /auth/password (auth.rs:269, "current password is incorrect"). So a user who mistypes their password on the sign-in screen is told "Your session has expired. Please sign in again." (Login.tsx renders err.message verbatim), and a user who typos the current password in the change-password form gets the same false message while throwSessionExpired() also wipes the still-valid in-memory bearer token (subsequent requests silently fall back to the cookie). The server's accurate, client-safe messages never reach the user on the two most common auth failure paths, including during the forced first-login rotation.

```
function throwSessionExpired(): never {
  setToken(null)
  throw new ApiError('Your session has expired. Please sign in again.', 401)
}
...
  if (response.status === 401) throwSessionExpired()
```

**Suggested fix:** Only treat a 401 as session expiry when a session plausibly existed and the request is not an auth endpoint: e.g. exempt /api/v1/auth/login and /api/v1/auth/password from the expired-session mapping (fall through to the normal error-envelope parsing so the server's message surfaces), or have login()/changePassword() bypass the shared 401 shortcut.

#### major-58. Session expiry mid-use leaves a dead UI: nothing returns the app to the Login screen
- **Where:** `web/src/api/client.ts:133`  |  **Category:** bug  |  **Found by:** web-core

Server sessions have both an absolute TTL and an idle timeout (epiphany-api/src/session.rs), so mid-session expiry is routine. When it happens, throwSessionExpired() clears the module-level token and throws, but no code anywhere subscribes to that condition: App.tsx keeps its `session` state non-null (it only resets via the explicit sign-out callback, CubeApp.tsx:647), and a grep of web/src shows no session-expired event or handler. The result is a zombie UI: every click in every workspace shows "Your session has expired. Please sign in again." as a panel-local error, the WS badge flips to offline, and the Login screen never appears. The user has to know to hard-reload the page; unsaved rule/flow/view edits in dirty editors are stranded with no way to re-authenticate in place.

```
function throwSessionExpired(): never {
  setToken(null)
  throw new ApiError('Your session has expired. Please sign in again.', 401)
}
```

**Suggested fix:** Give the client an app-level hook: e.g. a registerSessionExpiredHandler(cb) called from throwSessionExpired (or dispatch a window CustomEvent). App.tsx subscribes and does setSession(null) so the Login screen re-appears immediately on the first expired request; optionally preserve the in-progress route so re-login restores it.

#### major-59. CellsetGrid can commit an in-progress edit to the wrong cell after a live refetch
- **Where:** `web/src/components/CellsetGrid.tsx:116`  |  **Category:** concurrency  |  **Found by:** web-components

Rows are keyed by array index (`<tr key={r}>`, line 90) and each editable cell is an uncontrolled input keyed only by its displayed value. ViewWorkspace re-executes the view on every WebSocket reloadSignal (ViewWorkspace.tsx:156-159) and on every commit (onChanged), replacing `cellset` while the user may still be typing. With zero-suppression (or any remote structural change) the tuple at position (r,c) changes; if the old and new cell values are equal (very common: blank cells key to ''), React preserves the DOM input including the user's typed text, and the eventual blur calls commit(r,c,...) whose coordFor(r,c) resolves against the NEW cellset — writing the typed number to a different coordinate than the one the user was editing. Even without reorder, a remote value change remounts the input and silently discards the in-progress edit. The sibling PivotGrid documents exactly why positional/name keys are unsafe (tupleKey comment, PivotGrid.tsx:72-76) but CellsetGrid ignores that lesson.

```
<input
  key={cell.value ?? ''}
  aria-label={cellLabel(r, c)}
  defaultValue={cell.value ?? ''}
  ...
  onBlur={(e) =>
    void commit(r, c, cell.value ?? '', e.currentTarget.value.trim())
  }
```

**Suggested fix:** Key rows/cells by their tuple identity (join tuple member names, as PivotGrid does) instead of index, and capture the coordinate for a commit at focus/render time (bind coordFor(r,c)'s result into the closure) rather than re-resolving indices against whatever cellset is current at blur. Consider deferring the reloadSignal re-run while an input inside the grid has focus.

#### major-60. Shift-click range selection in MemberTable is dead code: onChange immediately overwrites the range
- **Where:** `web/src/components/MemberTable.tsx:622`  |  **Category:** bug  |  **Found by:** web-components

A shift-click on a row checkbox fires BOTH handlers: React dispatches the click event (range select via onRowSelect(...,true)) and then the change event (single toggle via onRowSelect(...,false)) for the same native click. Both calls run in one batch against the same stale `selected` prop, each building `next = new Set(selected)` and calling onSelectedChange; the second (single-toggle) call wins, so the range computed by the first call is discarded. Net effect: Shift-range multi-select — an explicit ADR-0032 v1 deliverable ("controlled multi-select ... checkbox + click + Shift-range") — always behaves as a plain single toggle. Users assembling large member sets must click every row individually.

```
<input
  type="checkbox"
  aria-label={`Select ${r.name}`}
  checked={!!isSel}
  onChange={() => onRowSelect(r.path, r.name, false)}
  onClick={(e) => {
    if (e.shiftKey) onRowSelect(r.path, r.name, true)
  }}
/>
```

**Suggested fix:** Handle selection in one place: read e.nativeEvent/shiftKey inside onChange (React passes the click-derived event for checkboxes) or capture shift state in onClick into a ref that onChange consults, so exactly one onSelectedChange fires per gesture. Add a unit test for shift-range.

#### major-61. Explain panel wipes the modeler's picked coordinate and open trace on every remote cell write
- **Where:** `web/src/components/RulesWorkspace.tsx:324`  |  **Category:** bug  |  **Found by:** web-components

The reset effect's deps include `reloadSignal`, and the comment says it should reset "when the cube ... changes". reloadSignal bumps on EVERY cells_changed/objects_changed from any user or tab (CubeApp WebSocket handler), and the parent also unconditionally calls setDetail(d) on each reload (line 71), producing a new `initial` object either way. So while a modeler is mid-analysis in the Explain panel, any data entry anywhere on the server resets their coordinate pickers to the first element of every dimension and clears the trace they were reading. On an active multi-user server the explain workflow is effectively unusable — the exact 'user state lost on WebSocket-triggered refetch' failure the parent component's own editor-buffer logic (lines 62-88) carefully avoids for rule source.

```
// Reset the picker when the cube (and thus its dimensions) changes.
useEffect(() => {
  setCoord(initial)
  setTrace(null)
  setError(null)
}, [initial, reloadSignal])
```

**Suggested fix:** Drop reloadSignal from the deps and key the reset on the cube name (or a dimension-shape fingerprint) instead of the `initial` object identity; merge-preserve the user's coord entries that still exist after a model change.

#### major-62. WebSocket never reconnects; UI claims "Offline - reconnecting" while live updates are permanently dead
- **Where:** `web/src/components/CubeApp.tsx:624`  |  **Category:** bug  |  **Found by:** web-components

connectWs (api/client.ts:1327-1338) opens one socket with no retry logic, and CubeApp's onclose/onerror only flip the badge to 'offline'. After any transient drop (server restart, laptop sleep, proxy idle timeout) reloadSignal never bumps again for the rest of the session: pivot grids, views, the explorer tree, and feeder diagnostics silently serve stale data in a multi-user system whose design leans on cells_changed/objects_changed for consistency (e.g. FlowsWorkspace/JobsWorkspace rely on it to detect remote deletes and disable a lost-update Save). The tooltip actively misleads: 'Offline - reconnecting' (line 814) when nothing reconnects.

```
const socket = connectWs((event) => {
  if (event.type === 'cells_changed' || event.type === 'objects_changed') {
    setReload((n) => n + 1)
  }
})
socket.onopen = () => setConn('live')
socket.onclose = () => setConn('offline')
socket.onerror = () => setConn('offline')
```

**Suggested fix:** Add exponential-backoff reconnect in the effect (re-create the socket on close unless unmounting), bump reloadSignal once on successful reconnect to resync, and only show 'reconnecting' while a retry is actually scheduled.

#### major-63. PivotGrid renders and fetches the full row-by-column cartesian product with no virtualization or memoization
- **Where:** `web/src/components/PivotGrid.tsx:652`  |  **Category:** performance  |  **Found by:** web-components

refresh() materializes one Coord object per (rowTuple x colTuple) cell and POSTs them all in a single readCells call, and the render path emits a plain <table> with one CellView (containing an <input> for editable cells) per cell. 'Expand all' on a dimension with a few thousand members yields tens of thousands of coords in one request and tens of thousands of DOM nodes. CellView is not memoized and receives fresh closures every render, so ANY PivotGrid state change re-renders every cell — including each keystroke in the Save-view name input (line 1132) and the MDX dialog textarea (line 1187), which are controlled state in this same component. This is the product's defining surface under an explicit ultra-performance mandate; ADR-0020 sanctioned a grid dependency or virtualization helper for exactly this, and the in-house useVirtualRows hook (ADR-0032) exists but is wired only into MemberTable.

```
const coords: Coord[] = []
for (const rt of rowTuples) {
  for (const ct of colTuples) {
    coords.push(coordFor(rt, ct))
  }
}
...
const fetched = await readCells(cube, coords)
```

**Suggested fix:** Wrap CellView in React.memo with stable per-cell callbacks, move the Save-view and MDX dialogs into child components so their keystrokes don't re-render the grid, apply useVirtualRows to the pivot body above a row threshold, and fetch cells for the visible window (or chunk readCells) instead of the full cross product.

#### major-64. ADR-0020 persona-gated shell not implemented: business users get full modeler chrome and raw MDX
- **Where:** `web/src/components/modelExplorerTree.ts:404`  |  **Category:** smell  |  **Found by:** web-components

ADR-0020 (Accepted, 'model locked; realized across W0-W5') mandates a three-persona shell resolved from the security lattice: a business user 'lands directly in a Views / data-entry workspace; the sidebar shows only their Views, Subsets, and the Sandbox switcher. No Rules, Flows, or Dimensions chrome.' The implementation gates only on a single is_admin flag (App.tsx:108, rootNodes(isAdmin)): every non-admin — including pure data-entry users — sees the full modeler tree (Cubes with Rules & feeders, the global Dimensions namespace, Flows, Schedules), the 'New dimension/flow/schedule' create commands in the palette, and the pivot toolbar's 'Show MDX' dialog with an editable, executable MDX textarea (PivotGrid.tsx:888-900). This is the progressive-disclosure north-star ('a casual business/data-entry user must never be forced to see modeler or admin machinery, write MDX/rules/flows') leaking wholesale into the casual path.

```
export function rootNodes(isAdmin: boolean): Node[] {
  const roots: Node[] = [
    { id: 'root:cubes', label: 'Cubes', ... },
    { id: 'root:dimensions', label: 'Dimensions', ... },
    { id: 'root:flows', label: 'Flows', ... },
    { id: 'root:schedules', label: 'Schedules', ... },
  ]
```

**Suggested fix:** Derive a persona (business/modeler/admin) from the existing grant lattice (e.g. any dimension/rule/flow write grant => modeler) in /me or client-side from listGrants, and gate the tree roots, palette create commands, and the Show MDX affordance on it, per ADR-0020 section 3. If this is deliberately deferred, record the deferral in the ADR's status.

#### major-65. WebView2 uses default user-data folder next to Excel.exe - Connect login fails on standard installs
- **Where:** `excel-addin/src/ConfiguratorForm.cs:60`  |  **Category:** bug  |  **Found by:** excel-addin

EnsureCoreWebView2Async() is called with no CoreWebView2Environment and no user-data folder. WebView2's documented default is to create the user data folder next to the HOST PROCESS executable, i.e. '<office dir>\EXCEL.EXE.WebView2' under Program Files, which a non-elevated user cannot create. On a standard Office install the WebView2 creation throws access-denied, the catch shows the misleading text 'WebView2 runtime missing?', and the entire Connect/login flow is dead - users can never sign in. This is the canonical WebView2-in-Office pitfall; Excel-DNA's own WebView2 samples pass an explicit userDataFolder under %LOCALAPPDATA% for exactly this reason. Because ADR-0022 says Excel is absent from CI and in-Excel behavior is only load-tested by the user, this ships broken until someone tries it on a locked-down machine.

```
await _web.EnsureCoreWebView2Async();
_web.CoreWebView2.WebMessageReceived += OnWebMessage;
...
catch (Exception e)
{
    _statusLabel.Text = "Could not start the embedded browser (WebView2 runtime missing?): " + e.Message;
}
```

**Suggested fix:** Create the environment explicitly: var env = await CoreWebView2Environment.CreateAsync(null, Path.Combine(Environment.GetFolderPath(SpecialFolder.LocalApplicationData), "Epiphany", "WebView2")); await _web.EnsureCoreWebView2Async(env); Also stop attributing every failure to a missing runtime.

#### major-66. Async UDF identity key has no separators and omits sandbox/server - colliding cells silently show the wrong value
- **Where:** `excel-addin/src/Functions.cs:39`  |  **Category:** bug  |  **Found by:** excel-addin

ExcelAsyncUtil.Run deduplicates and delivers results by (functionName, parameters) identity. The key here is built by concatenating cube + coord pairs with EMPTY separators (both string appends are "" and string.Join uses ""). Distinct requests therefore collide: cube 'Sales' with coord AB=X and cube 'SalesA' with coord B=X both produce key 'SalesAB=X'; coords {A=B, C=D} and {A=BC=D} (members may legally contain '=' since ParseCoord splits on the first '=') both produce 'A=BC=D'. When two cells collide, one cell silently receives the other cell's number - wrong figures in a finance workbook with no error indication. The key also omits Client.Sandbox and BaseUrl, so a read still in flight when the user switches the sandbox box (or reconnects to another server) and recalcs is matched by identity and delivers the old sandbox's/server's value into a cell the user believes shows the new context.

```
var key = cube + "" + string.Join("", coord.OrderBy(kv => kv.Key).Select(kv => kv.Key + "=" + kv.Value));
return ExcelAsyncUtil.Run("EPIPHANY.READ", new object[] { key }, () =>
```

**Suggested fix:** Pass unambiguous identity parameters instead of a hand-rolled string, e.g. new object[] { AddIn.Client.BaseUrl, AddIn.Client.Sandbox ?? "", cube, string.Join("", coord.OrderBy(...).Select(kv => kv.Key + "" + kv.Value)) } - use non-printable separators so no legal name can collide.

#### major-67. CommitSelection blocks Excel's main thread on network I/O with GetAwaiter().GetResult()
- **Where:** `excel-addin/src/CommitService.cs:68`  |  **Category:** concurrency  |  **Found by:** excel-addin

OnCommit is a ribbon callback and runs on Excel's main STA/UI thread. BatchWriteAsync(...).GetAwaiter().GetResult() synchronously blocks that thread on an HTTP round trip for up to the HttpClient's 30-second timeout. Against a slow, saturated, or unreachable server the whole Excel process freezes ('Not Responding'), message pumping stops, and the user has no cancel option - exactly the sync-over-async pattern ADR-0022's read path was designed to avoid. Blocking an STA thread without pumping also risks deadlock if anything in the completion path ever needs to marshal back to it.

```
var applied = AddIn.Client.BatchWriteAsync(prompt.Cube, writes).GetAwaiter().GetResult();
MessageBox.Show($"Committed {applied} cell(s) to \"{prompt.Cube}\".", "Epiphany",
    MessageBoxButtons.OK, MessageBoxIcon.Information);
```

**Suggested fix:** Make the commit path async: fire the POST on a background task (or make CommitSelection async void with await), keep the UI responsive with a small progress dialog, and show the result via a marshaled callback (Control.Invoke or ExcelAsyncUtil.QueueAsMacro as ADR-0022 point 4 already prescribes).

#### major-68. Each EPIPHANY.READ cell issues its own HTTP POST and blocks a thread-pool thread - promised coalescing layer is missing
- **Where:** `excel-addin/src/Functions.cs:44`  |  **Category:** performance  |  **Found by:** excel-addin

ADR-0022 point 3 mandates 'A small coalescing layer batches the cell reads issued in one recalc into one cells/read POST'. The implementation instead calls ReadCellAsync (a single-coordinate wrapper over ReadCellsAsync) per cell, and does so with GetAwaiter().GetResult() inside the ExcelAsyncUtil.Run delegate, which runs on a ThreadPool thread. A workbook with N READ formulas produces N HTTP POSTs and pins N pool threads in a blocking wait; the HttpClient completions that would release them also need pool threads, so once the pool's min-thread count is exceeded the pool injects threads at ~1/s and a few hundred formulas turn a recalc into a multi-minute starvation stall. Users with report-sized sheets will experience Excel hanging on every full recalc.

```
var value = AddIn.Client!.ReadCellAsync(cube, coord).GetAwaiter().GetResult();
return ToCell(value);
```

**Suggested fix:** Implement the ADR's coalescing layer: collect coordinates arriving within one recalc tick (e.g. a short debounce or ExcelAsyncUtil.Observe-based batcher), issue one ReadCellsAsync per cube, and complete each cell from the shared response. At minimum, replace the blocking wait with a truly async continuation so pool threads are not parked.

### MINOR (81)

#### minor-1. String pool never reclaims overwritten or deleted values and stores every string twice
- **Where:** `crates/epiphany-core/src/cube.rs:181`  |  **Category:** performance  |  **Found by:** core-storage

StringPool::intern keeps two full copies of every distinct string (one Box<str> in by_id, an independent cloned Box<str> as the ids key), and nothing ever removes entries: overwriting a string cell with a new value, clearing cells, delete_element, and retype_cells_for_kind all orphan pool entries permanently. Monotonic growth is documented (ADR-0006), but a long-running server whose ETL flows repeatedly write churning text (timestamps, per-run status strings) grows memory without bound until restart - a reload compacts because the pool is rebuilt from live cells, which shows the retained data is pure garbage. The doubled storage also works against the memory-efficiency mandate for large distinct-string sets.

```
let id = self.by_id.len() as u32;
let boxed: Box<str> = value.into();
self.by_id.push(boxed.clone());
self.ids.insert(boxed, id);
```

**Suggested fix:** Key the ids map by the id (hashing through by_id) or an Rc/offset scheme to store each string once, and add a compaction pass (e.g. during snapshot or when orphan count exceeds a threshold) that rebuilds the pool from live string cells.

#### minor-2. spread_leaves panics on a target coordinate longer than the cube rank
- **Where:** `crates/epiphany-core/src/spread.rs:66`  |  **Category:** bug  |  **Found by:** core-storage

spread_leaves iterates target.iter().enumerate() and calls cube.dimension(d), which indexes self.dimensions[i] directly (cube.rs lines 409-411) and panics for d >= rank. Every other coordinate-taking core entry point (set_leaf, get, set_string) validates rank via check_coord and returns ModelError::RankMismatch; this public seam instead crashes the calling thread on an over-long coordinate, and silently produces short-rank write coordinates for an under-long one. If any API caller ever forgets to pre-validate, a malformed spread request becomes a panic instead of a 4xx.

```
for (d, &idx) in target.iter().enumerate() {
    let weights = cube
        .dimension(d)
        .leaf_weights(idx)
```

**Suggested fix:** Validate target.len() == cube.rank() at the top of spread_leaves and return SpreadError::Read(QueryError::Model(ModelError::RankMismatch{..})) on mismatch.

#### minor-3. Alias reverse index diverges from persisted state when one element holds the same alias text under two alias attributes
- **Where:** `crates/epiphany-core/src/dimension.rs:233`  |  **Category:** determinism  |  **Found by:** core-storage

set_attribute's previous-alias cleanup removes the old text from alias_to_element whenever the element owned it, without checking whether another Alias attribute of the same element still carries that text (a legal state, since the uniqueness check only rejects other elements). Setting Alias1="Foo" and Alias2="Foo", then reassigning Alias1="Bar", makes resolve("Foo") return None in memory - yet reindex_names (triggered by any reorder/insert/delete) and a text save/load rebuild the map from attribute values and make "Foo" resolve again. The observable resolution of a name therefore flips across a restart or an unrelated structural edit, breaking the same-state-same-answer determinism contract for subset member resolution.

```
if let Some(prev) = prev_alias {
    if prev != *alias && self.alias_to_element.get(&prev) == Some(&element) {
        self.alias_to_element.remove(&prev);
    }
}
```

**Suggested fix:** Before removing prev from alias_to_element, scan the element's other Alias-kind attribute values for the same text and keep the mapping if any still holds it (or key the reverse index by (alias_text) -> set of (element, attr) so removal is exact).

#### minor-4. cell_entries/string_cell_entries expose hash-map iteration order with no documented ordering contract
- **Where:** `crates/epiphany-core/src/cube.rs:1021`  |  **Category:** smell  |  **Found by:** core-storage

The public cell iterators yield entries in FxHash HashMap order, which depends on insertion/removal history and hash-of-key (native-endian via the default Hasher::write_u64 path), so two logically identical cubes reached by different write orders enumerate differently. Current consumers all defensively sort (text.rs sorts before serializing, epiphany-calc feeders collect into a BTreeSet, engine tests sort), but nothing in the doc comment warns that the order is unspecified - under the project's strict no-observable-unordered-iteration mandate this is a loaded trap for the next consumer (e.g. a WAL or streaming API) that forgets to sort and silently breaks canonical-bytes guarantees.

```
/// Iterate populated numeric leaf cells as `(coordinate, value)`.
pub fn cell_entries(&self) -> impl Iterator<Item = (Vec<u32>, Fixed)> + '_ {
    self.cells
        .entries(&self.layout, self.rank())
        .map(|(coord, &value)| (coord, value))
}
```

**Suggested fix:** Document explicitly that iteration order is unspecified and must be sorted before any observable output, or provide a sorted_cell_entries() helper and steer observable consumers to it.

#### minor-5. Canonical serialization sorts View.context, so the observable context order changes across a restart
- **Where:** `crates/epiphany-core/src/text.rs:410`  |  **Category:** determinism  |  **Found by:** core-query-model

view_doc sorts the context by dimension name before writing, while build_view preserves document order and the API (view_from_body, query_routes.rs:424-428) preserves the author's request order in the in-memory View. Cellset::context is echoed verbatim from view.context (query.rs:656) and GET /views echoes it too (view_dto), so the same saved view returns context entries in author order until the server restarts, and in dimension-sorted order afterwards - an observable ordering that is not stable, contrary to the determinism mandate, and an object-level round-trip loss (Model -> text -> Model is not identity even though text -> Model -> text is). Clients that render context slicers in echoed order silently reorder after a restart.

```
context.sort_by(|a, b| a.dimension.cmp(&b.dimension));
```

**Suggested fix:** Stop sorting in view_doc (a Vec round-trips canonically in author order, so parse->serialize stays byte-identical), or normalize the order at the definition boundary (sort in view_from_body) so memory and disk always agree.

#### minor-6. Alias used as a context member silently disables editability and the overlaid flag in cellset responses
- **Where:** `crates/epiphany-api/src/query_routes.rs:943`  |  **Category:** bug  |  **Found by:** core-query-model

execute_view resolves context members with Dimension::resolve (query.rs:533), which accepts aliases, so a view whose context pins a member by its alias executes fine. But cellset_dto derives per-cell metadata with Dimension::index_of, which does NOT resolve aliases: leaf_of returns false for the alias name, making context_leaf false and therefore every cell editable=false; cell_coord_indices (line 933) likewise fails, so overlaid is never set for sandbox overrides. The result is a fully valid, correctly-valued cellset whose grid is silently read-only and never shows what-if markers, purely because the context used an alias instead of the primary name - the two lookup functions disagree about which names are valid.

```
cube.dimensions()
    .iter()
    .find(|d| d.name() == dim_name)
    .and_then(|d| d.index_of(member).and_then(|i| d.element(i).ok()))
    .map(|el| el.kind.is_leaf())
    .unwrap_or(false)
```

**Suggested fix:** Use Dimension::resolve (name-then-alias) everywhere cellset_dto and cell_coord_indices map member names back to indices, matching the resolution used at execution time.

#### minor-7. Deleting an element leaves dangling references in subsets, views, and rule tests, hard-failing dependent queries
- **Where:** `crates/epiphany-core/src/query.rs:1357`  |  **Category:** smell  |  **Found by:** core-query-model

Model::delete_element carefully remaps sandbox overrides (index-keyed) but does nothing for the name-keyed objects: a static subset listing the deleted member, a view with it inline or in context, or a rule test fixture naming it all remain in the model. The next execution of any view touching that subset fails wholesale with QueryError::UnknownMember - the entire view/dashboard errors rather than skipping the vanished member - and there is no cleanup, warning, or dry-run report at delete time (ADR-0036's dry-run only counts dropped cells). Operators deleting one obsolete member can break every saved report that referenced it, discovering the breakage only when users' views 404 at execution.

```
pub fn delete_element(&mut self, dimension: &str, element: &str) -> Result<(), ModelError> {
    let d = self.dimension_index(dimension);
    let old_names = d.map(|d| self.member_names(d));
    self.cube.delete_element(dimension, element)?;
    if let (Some(d), Some(old_names)) = (d, old_names) {
        let to_new = self.permutation_by_name(d, &old_names);
        self.remap_sandboxes_for_dimension(d, &to_new);
```

**Suggested fix:** On delete, either prune the member from static subsets / inline axis specs (and reject or report deletes that would empty a view's context), or have execution treat a non-resolving static member as skipped-with-report instead of failing the whole cellset; at minimum surface affected objects in the delete dry-run.

#### minor-8. Unchecked usize product can wrap and defeat the INPUT_EXPANSION_CAP soundness guard
- **Where:** `crates/epiphany-calc/src/feeders.rs:390`  |  **Category:** bug  |  **Found by:** calc

input_potent computes the cartesian size as an unchecked usize product; the workspace release profile does not enable overflow-checks, so the product wraps silently. The per-dim leaf lists are individually cheap to build (hundreds of entries each), so a reference pinning consolidated elements across many dimensions - e.g. a share-of-grand-total rule value['Region':'Total','Prod':'Total',...] on an 8-dim cube with 256-leaf rollups gives 256^8 = 2^64 which wraps to exactly 0 - makes `total > INPUT_EXPANSION_CAP` false and cartesian_any(total=0) return false: the input is declared not potent and the target is silently under-fed, inverting the cap's documented sound-over-feed direction. area_leaf_coords (line 317) and cartesian_any (line 401) share the same unchecked product.

```
let total: usize = per_dim.iter().map(|v| v.len()).product();
if total > INPUT_EXPANSION_CAP {
    return true;
}
cartesian_any(&per_dim, |coord| potent.contains(coord))
```

**Suggested fix:** Use checked_mul with early-exit: fold with usize::checked_mul and treat overflow as exceeding the cap (return true/potent in input_potent; error or cap in area_leaf_coords). Since INPUT_EXPANSION_CAP is 4096, short-circuit as soon as the running product exceeds it.

#### minor-9. Explain traces omit IF-condition inputs and evaluate untaken-branch inputs
- **Where:** `crates/epiphany-calc/src/provenance.rs:90`  |  **Category:** smell  |  **Found by:** calc

explain_node reuses feeders::collect_cells, which deliberately skips cells inside IF conditions (sound for feeding, wrong for provenance): for a rule IF value['M':'Flag'] > 0 THEN a ELSE b, the trace lists a and b but never Flag, so a user cannot see why the branch was chosen even though the evaluator consulted it - contradicting the module contract ("the input cells consulted"). Conversely, the trace force-evaluates cells from BOTH branches, so if a cell referenced only in the untaken branch fails under its own rule (e.g. DivByZero or Cycle), explain of a perfectly readable cell returns CALC_ERROR instead of a trace.

```
// feeders.rs collect_cells:
CExpr::If {
    cond: _,
    then,
    otherwise,
} => {
// provenance.rs:90:
collect_cells(&rule.expr, &mut cells);
```

**Suggested fix:** Give provenance its own cell collector that includes condition cells (optionally tagged as condition inputs), and tolerate per-input evaluation errors in the trace (mark the input as erroring) instead of failing the whole explain.

#### minor-10. Memo key allocates a boxed coordinate on every value() call, including hits
- **Where:** `crates/epiphany-calc/src/eval.rs:197`  |  **Category:** performance  |  **Found by:** calc, perf-lens

CalcEngine::value builds (scope_id, ordinal, coord.to_vec().into_boxed_slice()) before the memo lookup, so every single cell pull - including each of the potentially millions of leaves streamed through consolidate_with for one rollup, and every memo hit - pays a heap allocation plus a re-hash of the full coordinate. Under the project's ultra-performance/memory mandate this is a real hot-path cost: a dense rollup of N leaves performs N allocations purely for memo bookkeeping (and the API layer currently discards the memo per value read, making them pure waste).

```
pub fn value(&self, ordinal: u32, coord: &[u32]) -> Result<Fixed, CalcError> {
    let key = (self.scope_id, ordinal, coord.to_vec().into_boxed_slice());
    {
        let mut memo = self.memo.borrow_mut();
        match memo.get(&key) {
```

**Suggested fix:** Avoid allocating on lookup: pack the coordinate into the cube's existing packed integer key (the store already has one) keyed per (scope, ordinal), or use a borrowed-key lookup pattern (e.g. a nested map per (scope, ordinal) keyed by a hash-consed coord, or hashbrown's entry_ref) and only allocate on first insert.

#### minor-11. CalcView::string_value bypasses the engine's element deny mask
- **Where:** `crates/epiphany-calc/src/eval.rs:371`  |  **Category:** security  |  **Found by:** calc

CalcView is the crate's public CellResolver ("for execute_view / read_cells") over a CalcEngine that may carry an ADR-0015 element deny mask. Numeric reads route through engine.value, which enforces the mask at the cell terminal, but string_value goes straight to cube.get_string with no mask check, so a denied element's string cells are readable through any consumer wired per CalcView's own documentation. The production API avoids this only because CalcCellResolver in epiphany-api re-implements string_value with an explicit mask.denies check - duplicated enforcement logic that the crate-level type silently lacks, a security trap for the next caller of the public API.

```
fn string_value(&self, coord: &[u32]) -> Result<Option<String>, QueryError> {
    // Rules are numeric for M4; string cells pass through to stored values.
    let cube = self.engine.registry.cube(self.ordinal)
        .ok_or(QueryError::Calc { ... })?;
    Ok(cube.get_string(coord)?.map(str::to_string))
}
```

**Suggested fix:** Enforce the engine's mask in CalcView::string_value (engine has the mask and mask_target fields in the same module), and have epiphany-api's CalcCellResolver delegate to it so element-security enforcement lives in exactly one place.

#### minor-12. run_rule_tests cannot run any model whose rules contain cross-cube references
- **Where:** `crates/epiphany-calc/src/testing.rs:136`  |  **Category:** bug  |  **Found by:** calc, olap-core-semantics

run_rule_tests compiles the model's rules against SingleCube::new(&model.cube), so any rule with a cross-cube reference ('FX'![...]) fails compilation with UnknownCube and the entire test run errors (TestRunError::Compile -> REST 422 RULE_TEST_ERROR), even though the same source compiles and evaluates fine in production via the multi-cube PinnedRegistry. The evaluation registry (TestRegistry) likewise only exposes ordinal 0. Result: the model-testing framework - the tool meant to guard rule correctness - is unusable on exactly the models with the riskiest rules, and the failure reads like the rules are broken rather than the runner being single-cube.

```
let doc = parse(&model.rules.source)?;
let compiled = compile(&model.cube, &SingleCube::new(&model.cube), &doc, 0)?;
```

**Suggested fix:** Accept a multi-cube registry (the engine already builds PinnedRegistry) so cross-cube rules compile and evaluate against pinned sibling cubes during tests, or at minimum map UnknownCube to a targeted error explaining the single-cube test limitation.

#### minor-13. validate_feeders aborts on the first evaluation error, hiding the whole diagnostic report
- **Where:** `crates/epiphany-calc/src/feeders.rs:276`  |  **Category:** smell  |  **Found by:** calc

Both diagnostic loops propagate any CalcError with `?`, so a single rule target that evaluates to DivByZero or a Cycle anywhere in the enumerated leaf space turns the entire /feeders/diagnostics response into a CALC_ERROR 422. Under/over-feed information for all the healthy rules is withheld exactly when a model has problems - the situation in which an operator most needs the report. The two failure classes (a rule that errors vs. a rule that under-feeds) are independent and should be reported side by side.

```
for target in area_leaf_coords(cube, &rule.area) {
    if engine.value(ordinal, &target)? != Fixed::ZERO && !index.contains(&target) {
        under.insert(target);
    }
}
```

**Suggested fix:** Catch per-target evaluation errors and accumulate them into the diagnostics (e.g. an `erroring: Vec<(coord, error)>` list on FeederDiagnostics) instead of failing the whole validation, so under/over-feed results remain available alongside the error report.

#### minor-14. Member-path validation is shallow: middle segments and .Members qualifiers are silently ignored
- **Where:** `crates/epiphany-mdx/src/eval.rs:144`  |  **Category:** bug  |  **Found by:** mdx

resolve_member validates only path[0] against the dimension name and resolves only the last segment, so `[Region].[Nowhere].[North]` evaluates as if it were `[North]` - a typo'd or wrong intermediate member is never reported, and a user who writes `[Region].[Total].[North]` believing it scopes North under Total gets no validation. dimension_ref is inconsistent the other way: it checks only the LAST segment, so `[Bogus].[Region].Members` passes while `[Sales].[Region].[North]` (same cube-qualified style, member form) fails with DimensionMismatch. Subset authors get silently-accepted garbage in one form and a confusing rejection in the other.

```
if r.path.len() >= 2 && r.path[0] != dim.name() {
    return Err(MdxEvalError::DimensionMismatch { ... });
}
let name = r.name();
dim.resolve(name)
...
fn dimension_ref(dim: &Dimension, r: &MemberRef) -> Result<(), MdxEvalError> {
    let named = r.name();
```

**Suggested fix:** Validate every path segment (dimension qualifier + optional ancestor chain via the hierarchy, or reject paths longer than 2), and make dimension_ref apply the same first-segment rule as resolve_member.

#### minor-15. `<>` and `NOT =` disagree on missing attribute values
- **Where:** `crates/epiphany-mdx/src/eval.rs:314`  |  **Category:** bug  |  **Found by:** mdx

compare_vals returns false for any comparison involving a Missing value ('as with SQL NULL semantics'), but Predicate::Not (line 281) does plain boolean negation. So for members that lack the attribute, `Filter(s, Properties("x") <> "y")` EXCLUDES them while the logically equivalent `Filter(s, NOT Properties("x") = "y")` INCLUDES them. In SQL, NOT(NULL = 'y') is still NULL and the row is filtered out, so the comment's SQL claim breaks under negation. Users get different subset membership from two natural spellings of "not equal", and the divergence only shows up on sparse attribute data.

```
// A missing attribute makes the comparison false (the member is filtered
// out), as with SQL NULL semantics.
(Val::Missing, _) | (_, Val::Missing) => Ok(false),
...
Predicate::Not(p) => Ok(!eval_predicate(dim, element, p)?),
```

**Suggested fix:** Pick one semantics and document it: either propagate a three-valued 'unknown' through NOT/AND/OR so both spellings exclude missing values, or define Ne as the complement of Eq (missing <> x is true) so the two forms agree.

#### minor-16. children_of / descendants_of materialize and sort the entire edge list on every evaluation
- **Where:** `crates/epiphany-mdx/src/eval.rs:172`  |  **Category:** performance  |  **Found by:** mdx, perf-lens

children_of calls dim.edges(), which allocates a Vec of ALL edges in the dimension and sorts it O(E log E), then filters for a single parent; descendants_of likewise rebuilds a full adjacency BTreeMap from a fresh sorted edge list on every call, even for a one-node subtree. Dynamic subsets re-resolve at execute time (ADR-0011), so every view execution over a large dimension (ADR-0032 targets scalable member tables) pays a full edge sort per `.Children`/`.Descendants` node. Core already stores children[parent] per parent but does not expose it. Against the 'ultra performance' mandate this is repeated avoidable O(E log E) work in the hot query path.

```
let kids: Vec<u32> = dim
    .edges()
    .into_iter()
    .filter(|&(p, _, _)| p == parent)
...
let mut adjacency: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
for (parent, child, _) in dim.edges() {
```

**Suggested fix:** Expose a per-parent children accessor on Dimension (children sorted by child index to keep the canonical order) and use it directly; for descendants, traverse via that accessor instead of rebuilding an adjacency map per call.

#### minor-17. Bare attribute in a predicate reports the error on the wrong token
- **Where:** `crates/epiphany-mdx/src/parser.rs:548`  |  **Category:** smell  |  **Found by:** mdx

parse_property_operand consumes the whole dotted path first and only then checks that the final segment is `Properties`; on failure it calls self.unexpected(), which describes and spans the NEXT token. For the very likely input `Filter([R].Members, Code = "N")` (bare attribute, a common TM1/SSAS habit) the error is "unexpected `=`, expected `.Properties(\"Attr\")`" with the span on `=` - the editor squiggle lands on the operator instead of `Code`, sending the user to the wrong place. The lens item 'error spans pointing at wrong tokens' applies: the span of the consumed path is discarded.

```
let mut last = self.expect_name("a member reference")?;
while self.peek() == Some(&Tok::Dot) && matches!(self.peek2(), Some(Tok::Name { .. })) {
    self.bump(); // '.'
    last = self.expect_name("a member reference")?;
}
if !last.eq_ignore_ascii_case("properties") {
    return Err(self.unexpected("`.Properties(\"Attr\")`"));
```

**Suggested fix:** Record the span of the first (or last) consumed name token and attach the UnexpectedToken error to it, with `found` set to the path text rather than the following operator.

#### minor-18. Documented grammar allows zero axes but the parser requires one, with a misleading failure
- **Where:** `crates/epiphany-mdx/src/parser.rs:160`  |  **Category:** smell  |  **Found by:** mdx

The module grammar (line 9) reads `query := 'SELECT' ( axis ( ',' axis )* )? 'FROM' member` - axes optional, matching standard MDX where `SELECT FROM [Cube]` is a valid slicer-only query. The implementation unconditionally parses at least one axis. Worse, because keywords are not reserved, `SELECT FROM [Sales]` consumes `FROM` as a bare member name and then fails with "unexpected `[Sales]`, expected `ON`" - the error names the wrong token and the wrong cause. Users writing the documented (and standard) form get a baffling diagnostic instead of either support or a clear rejection.

```
let mut axes: Vec<(AxisName, SetExpr)> = Vec::new();
loop {
    let set = self.parse_set()?;
    self.expect_keyword("on", "`ON`")?;
```

**Suggested fix:** Either support zero axes (peek for `from` before the loop) to match the doc, or fix the grammar comment and special-case a bare `FROM` right after SELECT to produce a clear 'a query needs at least one axis' error.

#### minor-19. ORDER's plain ASC/DESC are silently identical to BASC/BDESC, contradicting the AST doc
- **Where:** `crates/epiphany-mdx/src/eval.rs:243`  |  **Category:** smell  |  **Found by:** mdx

order_set collapses all four directions into a flat key sort (`ascending = matches!(dir, Asc | BAsc)`), so the hierarchy-breaking B-forms are accepted but change nothing. In real MDX, plain ASC/DESC sort within hierarchy groups (children stay under their parents) while BASC/BDESC flatten - a user porting a TM1/SSAS subset that relies on `Order(..., ASC)` keeping parent grouping gets a silently flattened order with no error. The in-code comment acknowledges the collapse, but ast.rs:54-55 still documents 'the plain forms preserve hierarchy' - the doc and the shipped semantics disagree, and offering two spellings that do the same thing hides the deviation from users.

```
/// Stable sort by an attribute key. The `B`-prefixed (hierarchy-breaking) and
/// plain directions are treated alike here: our subsets are flat member lists,
/// so both produce a flat key sort with the input order as the tie-break.
...
let ascending = matches!(dir, OrderDir::Asc | OrderDir::BAsc);
```

**Suggested fix:** Either implement hierarchy-preserving plain ORDER (sort siblings within the hierarchized input structure) or reject/deprecate the plain forms so the accepted syntax reflects actual semantics; at minimum fix the ast.rs OrderDir doc.

#### minor-20. Every bare CR is silently deleted from unquoted CSV content; CR-only files parse to zero rows
- **Where:** `crates/epiphany-flow/src/csv.rs:108`  |  **Category:** bug  |  **Found by:** flow

The comment says 'fold CRLF' but the branch skips ANY '\r' in unquoted content, not just one preceding '\n'. Verified: `A,B\r1,2\r3,4\r` (classic-Mac CR line endings) yields zero rows — every record is folded into one header row ['A','B1','23','4'] — and a lone CR inside a field (`foo\rbar`) is silently deleted, yielding 'foobar'. Both are silent data alterations; the CR-only case is total silent data loss (flow reads 0 rows, run 'succeeds'). Quoted fields meanwhile preserve '\r' verbatim, so behavior is also inconsistent between quoted and unquoted content.

```
} else if c == '\r' {
    i += 1; // fold CRLF
}
```

**Suggested fix:** Only skip '\r' when the next char is '\n' (true CRLF folding); optionally treat a bare '\r' as a record terminator like '\n', and otherwise keep it as field content.

#### minor-21. A stray quote mid-field flips the CSV scanner into quoted mode, swallowing commas and newlines
- **Where:** `crates/epiphany-flow/src/csv.rs:75`  |  **Category:** bug  |  **Found by:** flow

The `c == '"'` branch triggers at any position, not only at field start. An unquoted field containing a quote character — inch marks (`size 5" x 2,10`), unbalanced quotes from hand-edited data — enters the quoted-field loop, which consumes commas and newlines until the next quote anywhere later in the file (silently merging fields and rows into one value) or errors 'unterminated quoted field' at the wrong line. Verified: `says "hi" ok` parses to 'says hi ok' (quotes silently removed). Lenient parsers (e.g. the rust csv crate) instead treat a quote appearing mid-field as a literal character.

```
if c == '"' {
    started = true;
    i += 1;
    // Quoted field: copy until the closing quote, doubling `""` to `"`.
```

**Suggested fix:** Enter quoted mode only when the quote is the first character of the field (field.is_empty() and at field start); otherwise push '"' as a literal character.

#### minor-22. skip_template miscounts interpolation depth when '}' or '`' appear inside strings within ${...}
- **Where:** `crates/epiphany-flow/src/strip.rs:253`  |  **Category:** bug  |  **Found by:** flow

The template scanner tracks `${`/`}` depth but does not skip nested string/template literals inside the interpolation. A '}' inside a quoted string inside `${...}` decrements depth, so a subsequent '`' inside the interpolation ends the template early and the rest of the file is mis-lexed. Verified with `const s = `x${ "}" + "`" }y`;` followed by `const z: number = 1;`: the annotation on z is not stripped (everything after the early close is consumed as a phantom string), producing a boa parse error on valid TS; in unluckier alignments the mis-lexed tail can cause real code to be blanked as a phantom annotation. Rare input, but a lexer-desync class of failure.

```
if c == '$' && self.peek(1) == Some('{') {
    depth += 1;
    self.i += 2;
    continue;
}
if depth > 0 && c == '}' {
    depth -= 1;
}
```

**Suggested fix:** While depth > 0, recursively skip nested '\''/'"' strings, comments, and nested templates instead of scanning raw characters, mirroring the main run() loop.

#### minor-23. ctx.param() reads through the prototype chain, unlike ctx.input()
- **Where:** `crates/epiphany-flow/src/run.rs:920`  |  **Category:** smell  |  **Found by:** flow

ctx.input(name) guards with Object.prototype.hasOwnProperty.call (line 916), but ctx.param(name) is a bare `__params[name]` index. A flow probing `if (ctx.param('toString'))` or any Object.prototype key gets an inherited function instead of undefined, so 'is this param set?' checks are wrongly truthy for a handful of magic names. Deterministic, but an inconsistent and surprising edge in the host API surface.

```
param: function (name) { return __params[name]; },
```

**Suggested fix:** Mirror the input() guard: return Object.prototype.hasOwnProperty.call(__params, name) ? __params[name] : undefined; (or build __params with Object.create(null)).

#### minor-24. Duplicate CSV header columns silently collapse when rows cross into JS (last column wins)
- **Where:** `crates/epiphany-flow/src/run.rs:790`  |  **Category:** smell  |  **Found by:** flow

Row is deliberately Vec<(String,String)> 'in column order' and parse_csv faithfully produces both entries for a duplicated header (verified: `A,A\n1,2` yields [('A','1'),('A','2')]). But rows_to_json_value inserts into a serde_json::Map keyed by name, so the second 'A' silently overwrites the first before the flow ever sees it — a whole column of data becomes invisible with no warning. Duplicate headers are common in exported spreadsheets.

```
let mut m = serde_json::Map::new();
for (k, v) in row {
    m.insert(k.clone(), serde_json::Value::String(v.clone()));
}
```

**Suggested fix:** Either reject duplicate header names in parse_csv with a loud CsvError, or disambiguate on conversion (A, A_2, ...) so no column silently disappears.

#### minor-25. Ledger compaction tie-break iterates HashMap order, so the retained set is not strictly deterministic
- **Where:** `crates/epiphany-flow/src/ledger.rs:273`  |  **Category:** determinism  |  **Found by:** flow

enforce_retention builds `latest` from self.latest.values() (HashMap iteration order) and computes each job's best success from that unsorted Vec using `>=` as the tie-break. Two successful records for the same (cube, job) with equal fire_millis but different ids (e.g. a manual job run and a scheduled firing landing on the same millisecond) resolve to whichever the HashMap yields last, so which run id survives compaction — observable via the REST recent-runs views — can differ between identical replays. Narrow window, but the project mandate is no unordered iteration anywhere observable, and the sort at line 282 happens only after best_success is computed.

```
if r.is_job && r.state == RunState::Succeeded {
    let entry = best_success
        .entry((r.cube.clone(), r.target.clone()))
        .or_insert((0, String::new()));
    if r.fire_millis >= entry.0 {
        *entry = (r.fire_millis, r.id.clone());
    }
}
```

**Suggested fix:** Sort `latest` (fire_millis, id) before computing best_success, or make the tie-break total: replace the entry when (fire_millis, id) is strictly greater than the stored (fire_millis, id) pair.

#### minor-26. Recovery cannot distinguish a torn tail from mid-log corruption and destructively truncates everything after the first bad frame
- **Where:** `crates/epiphany-persist/src/store.rs:179`  |  **Category:** smell  |  **Found by:** persist

wal::replay stops at the first CRC/decode failure and Store::open immediately set_len()s the file to good_len. For a genuine torn tail (the ADR-0002 case) that is correct, but for mid-log corruption - one flipped bit from bit rot or a bad sector, with many intact, fsync-acknowledged frames after it - recovery silently discards all of them AND physically erases them, destroying the evidence an operator (or a smarter tool) could have used. With fsync-per-write on, a bad CRC that is followed by further well-formed frames is provably corruption, not a tear, and deserves at least a loud warning before any truncation.

```
// Drop any torn tail, then position at the end for new appends.
let mut file = OpenOptions::new().write(true).open(&wal_path)?;
file.set_len(replay.good_len)?;
file.seek(SeekFrom::End(0))?;
```

**Suggested fix:** In replay, scan past a bad frame to detect whether additional valid frames follow; if so, treat it as corruption: preserve the original file (rename to wal.log.corrupt) and surface a warning/error instead of silently set_len-ing acknowledged records away.

#### minor-27. Audit log wipes its entire history on a corrupt header and truncates it on mid-log corruption, despite being non-reconstructible compliance data
- **Where:** `crates/epiphany-security/src/audit.rs:206`  |  **Category:** security  |  **Found by:** persist

ADR-0010 makes the audit stream 'primary data, not a cache' that 'is not reconstructible', and allows discarding only 'a corrupt or truncated audit tail'. The implementation goes further: a header that fails the magic/version check causes set_len(0) - the whole audit history is destroyed in place with no copy preserved (and no logging in this module); and a single bad CRC mid-file causes set_len(good_len), permanently deleting every later record. An attacker (or an accident) that damages the first 8 bytes of audit.log erases the complete compliance trail on the next restart, silently. Non-gating startup (per the ADR) does not require destroying the bytes.

```
None => {
    // New, or a missing/corrupt header: reset to a clean header.
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header())?;
    file.sync_data()?;
    (Vec::new(), 0)
}
```

**Suggested fix:** Before re-initializing or truncating, preserve the damaged file (rename to audit.log.corrupt-<n>) and emit a prominent log/audit event; recovery stays non-gating while the forensic record survives.

#### minor-28. Audit sequence numbers restart at 0 after a compaction that retains zero records
- **Where:** `crates/epiphany-security/src/audit.rs:200`  |  **Category:** bug  |  **Found by:** persist

On reopen, next_seq is derived solely from the recovered records' max seq. RetentionPolicy.max_age_millis can age out EVERY record (rewrite_file then persists an empty, header-only file); after a restart the log recovers zero records and next_seq becomes 0, so new entries reuse sequence numbers 0,1,2,... already issued to earlier (possibly exported or quoted) audit records. This breaks ADR-0010's 'monotonic sequence number' contract and the module's own promise that 'Sequence numbers stay monotonic across a compaction'; downstream consumers keying on seq see duplicates.

```
let next = records.iter().map(|r| r.seq).max().map_or(0, |m| m + 1);
...
let mut retained: Vec<AuditRecord> = self.records[start..].to_vec();
if let Some(cut) = age_cut {
    retained.retain(|r| r.timestamp_millis >= cut);
}
```

**Suggested fix:** Persist next_seq independently of the records - e.g. a small trailer/side file, or always retain the newest record during compaction (never compact to empty), so the sequence base survives a restart.

#### minor-29. No automatic checkpoint: the WAL grows unboundedly and recovery reads it whole into memory
- **Where:** `crates/epiphany-persist/src/store.rs:154`  |  **Category:** performance  |  **Found by:** persist

Cell writes only ever append; checkpoints happen solely on structural/definition edits or the explicit full-persist command (engine.checkpoint is called only from boot migration and the API command). A workload of pure data entry - the common OLAP case - never checkpoints, so wal.log grows without bound, and Store::open does fs::read of the entire file into one Vec plus record-by-record replay through set_leaf. A long-lived write-heavy server accumulates a multi-GB WAL, making restart slow and spiking memory at the worst moment (recovery). ADR-0002 chose 'periodic binary snapshots' and defers fsync-cadence tuning to Phase 8, but no periodic or size-based trigger exists at all.

```
let wal = if wal_path.exists() && fs::metadata(&wal_path)?.len() >= wal::WAL_HEADER_LEN {
    let bytes = fs::read(&wal_path)?;
    let replay = wal::replay(&bytes).map_err(|e| PersistError::Corrupt(e.to_string()))?;
```

**Suggested fix:** Add a size-threshold auto-checkpoint in the engine's write path (e.g. checkpoint when the WAL exceeds N MB, configurable), and/or stream the WAL during replay instead of fs::read-ing it whole.

#### minor-30. edit_dimension returns the last fanned-out cube's CommitOutcome, not the requested cube's
- **Where:** `crates/epiphany-engine/src/lib.rs:584`  |  **Category:** bug  |  **Found by:** engine

For a registry-backed dimension, the fan-out loop iterates referencing cubes in sorted order and keeps only the last outcome, which is then returned to the caller. Editing 'Region' on CubeA when referrers are [CubeA, CubeB] returns CubeB's new version. The API forwards outcome.version to the client (dimension_routes.rs:526) as the edited cube's new version, so client-side base-version bookkeeping and version-keyed caches for the requested cube are fed another cube's version - producing spurious 409 conflicts on the next optimistic write and confusing WS gap detection.

```
let mut last = None;
for referrer in snapshot.referencing(id) {
    if self.has_cube(&referrer) {
        last = Some(self.apply_dimension_edit_to_cube(&referrer, dim_name, edit)?);
    }
}
```

**Suggested fix:** Track and return the outcome for the cube the caller named (falling back to the direct edit only when it is not a referrer), independent of iteration order.

#### minor-31. Rules are validated outside the writer lock and committed with base=None (TOCTOU)
- **Where:** `crates/epiphany-api/src/rule_routes.rs:99`  |  **Category:** concurrency  |  **Found by:** engine

The save-rules route compiles the source against a lock-free snapshot, then calls engine.define_rules(cube, None, ...) - no base version, no lock held across validate+commit. A concurrent structural edit (e.g. edit_dimension Delete of an element the rules reference) between the compile and the commit stores a ruleset that no longer compiles; subsequent rule-aware reads on the cube error until an admin fixes the rules. The engine's define_rules explicitly stores source verbatim and delegates validation to the caller (engine lib.rs:889-897), so the engine offers no way to close this window.

```
compile_source(&state.engine, &cube, &body.source)
    .map_err(|e| map_validate(e, &body.source))?;
let outcome = state
    .engine
    .define_rules(&cube, None, body.source.clone())
```

**Suggested fix:** Pass the validated snapshot's version as the base for define_rules (retrying validation on Conflict), or extend the engine's define seam to run a validation closure under the writer lock.

#### minor-32. Writer-mutex poisoning turns every later write into a panic with no recovery path
- **Where:** `crates/epiphany-engine/src/lib.rs:795`  |  **Category:** concurrency  |  **Found by:** engine

All engine locks are taken with .expect("... mutex poisoned") (writer at lib.rs:795/1168/1221, topology at 1111, dim_topology at 399/427/472/551/628/675/707). A single panic while holding a cube's writer lock (an allocation failure during the whole-model clone, or a bug in a core op) poisons the mutex, after which every write to that cube panics in its handler instead of returning a clean BatchError - the cube is write-bricked until restart with confusing symptoms, and if dim_topology poisons, all dimension operations and create_cube_with_refs are bricked server-wide. Fail-closed on a possibly-inconsistent store is defensible, but the machinery to recover safely already exists: published is only updated on success, and restore_model can resynchronize the store from it.

```
let mut writer = state.writer.lock().expect("writer mutex poisoned");
```

**Suggested fix:** Handle PoisonError deliberately: recover the guard, restore_model from the last published version (the known-good state), clear the poison, and return a structured BatchError so operators see errors rather than cascading panics.

#### minor-33. Change-feed events are broadcast after commit outside the writer lock: reordering and silent drops vs commit visibility
- **Where:** `crates/epiphany-api/src/routes.rs:52`  |  **Category:** concurrency  |  **Found by:** engine

ws.rs promises 'Every committed write or batch broadcasts exactly one event' and clients use the per-cube version for gap detection, but events are sent from the handler after the engine commit returns, with no ordering tie to publication: two concurrent commits to the same cube (versions N then N+1) can broadcast in the order N+1 then N, and a handler task that dies between commit and send drops the event entirely. The fan-out paths (dimension_routes.rs:387-396, 507-514) instead re-read engine.version(cube) after the fact, which can skip intermediate versions or repeat the same version for multiple edits. Clients relying on monotonic versions may treat a reordered event as a gap (spurious refetch) or stay stale after a dropped one. The engine offers no commit-ordered notification seam, so the API cannot do better today.

```
pub(crate) fn broadcast_with_version(state: &AppState, cube: &str, version: u64) {
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.to_string(),
        version,
    });
}
```

**Suggested fix:** Emit change events from inside the engine's commit path (while the writer lock still serializes the cube) into the broadcast channel, so per-cube event order provably matches version order and no committed version can be skipped.

#### minor-34. grow_dimension/edit_dimension hold the global dim_topology lock across serial per-cube disk checkpoints
- **Where:** `crates/epiphany-engine/src/lib.rs:516`  |  **Category:** performance  |  **Found by:** engine

The fan-out loops run under the single dim_topology mutex and, for each referencing cube, take its writer lock and perform a full snapshot rewrite to disk (define_elements -> extend_schema -> checkpoint serializes the entire model text and fsyncs; edit_dimension's reindexing ops checkpoint twice per cube). With many referencing cubes or large cubes this holds the one global dimension lock for the sum of all those disk writes, stalling every other dimension operation, promotion, and create_cube_with_refs server-wide (reads stay lock-free). The lock ordering itself is deliberate (ADR-0024), but holding it across N sequential fsyncs is a growth trap the ADR does not require.

```
self.persist_registry();
for cube in snapshot.referencing(id) {
    if self.has_cube(&cube) {
        self.define_elements(&cube, None, &els, &edgs)?;
    }
}
```

**Suggested fix:** Shrink the critical section: publish the registry generation under dim_topology, then perform the per-cube materialization outside it (per-cube writer locks already serialize each cube), guarding against interleaved registry mutations with a generation check or a narrower per-dimension lock.

#### minor-35. Best-effort registry persistence silently loses a newly registered, not-yet-referenced dimension
- **Where:** `crates/epiphany-engine/src/lib.rs:455`  |  **Category:** smell  |  **Found by:** engine

persist_registry discards the save_registry error on the grounds that 'every referencing cube already holds its own durable copy of the dimension'. That justification does not hold for the register_dimension / register_dimension_def / promote_cube_dimension paths where the dimension has zero referencing cubes (or promote, where the id mapping itself lives only in the registry): if the write fails, the caller still receives a minted DimensionId, but after a restart the dimension (or the cube-to-id backing) is gone with no log or error anywhere - flows resolving it by name (dimension_id_by_name) start failing mysteriously. There is also no operator-visible signal that the durable registry is lagging the in-memory one.

```
/// Persist the current registry to `dimensions_dir`, if durable. Best-effort:
/// a write failure is not fatal because every referencing cube already holds
/// its own durable copy of the dimension (the registry reconciles on reload).
...
let _ = save_registry(dir, &entries);
```

**Suggested fix:** Propagate the save error from register/promote (where the registry is the only durable home of the new identity), or at minimum log it and retry on the next mutation instead of discarding it.

#### minor-36. Revoking the last subject on an element ACL silently opens the member to everyone
- **Where:** `crates/epiphany-security/src/store.rs:545`  |  **Category:** security  |  **Found by:** security-crate

set_element_access with AccessLevel::None removes the subject, and when the list empties the whole ACL entry is deleted — flipping the member from 'only X may read' to 'every cube reader may read'. An admin whose intent is 'remove Bob's access to this sensitive member' gets the exact opposite when Bob is the last grantee, with no warning from the API ('none revokes' is all the surface says). This is a consequence of the ADR-0015 restriction-list model rather than a spec violation, but it is a loaded transition with no guard, no distinct audit shape, and no UI/API signal that the removal unrestricts rather than denies.

```
let list = self.element_acls.entry(key.clone()).or_default();
list.set(subject, level);
if list.is_empty() {
    self.element_acls.remove(&key);
}
self.save()
```

**Suggested fix:** Return/audit a distinct 'element unrestricted' outcome when the last entry is removed, and have the admin UI require an explicit confirm; alternatively support an empty-but-present ACL meaning deny-all-non-admin.

#### minor-37. Mutate-then-save: a failed persist leaves memory and disk divergent (failed revokes un-revoke on restart)
- **Where:** `crates/epiphany-security/src/store.rs:578`  |  **Category:** bug  |  **Found by:** security-crate

Every mutator (set_grant, set_element_access, set_user_groups, delete_user, ...) mutates in-memory state first and then calls save(); on an I/O failure the caller gets an error but the mutation stays live. A revocation reported as failed is thus enforced until restart, then silently reverts on reload of the stale artifact — or, worse, the 'failed' change is silently persisted wholesale by the next unrelated successful save. Either way the operator's mental model (error = nothing happened) is wrong for security state.

```
let list = self.grants.entry(key.clone()).or_default();
list.set(subject, level);
if list.is_empty() {
    self.grants.remove(&key);
}
self.save()
```

**Suggested fix:** Stage the mutation (clone the affected map or serialize the would-be state), write the artifact, and only commit to memory on successful rename — or roll back the in-memory change when save() errs.

#### minor-38. Password change endpoint has no rate limiting and no audit record on failed attempts
- **Where:** `crates/epiphany-api/src/auth.rs:253`  |  **Category:** security  |  **Found by:** api-auth-http

change_password verifies attacker-supplied current_password with no LoginGuard integration and emits an audit record only on success (the ? on the map_err path returns before the audit call at line 284). An attacker holding a hijacked session token can brute-force the account's real password online - the one credential that survives the session revocation that a password change is supposed to provide - completely invisibly to an operator reviewing the audit log, and with no lockout ever tripping. This undercuts ADR-0017's lockout, which guards only /auth/login.

```
.map_err(|e| match e {
    SecurityError::IncorrectPassword => {
        ApiError::unauthorized("current password is incorrect")
    }
    // The strength-policy reason is client-safe (no password material).
    SecurityError::WeakPassword(_) => ApiError::bad_request(e.to_string()),
    _ => ApiError::internal(),
})?;
```

**Suggested fix:** Route IncorrectPassword through login_guard.record_failure(username) / is_locked like login does, and emit an AuditAction::UserChange (or dedicated action) record with allowed=false before returning the 401.

#### minor-39. Failed logins retain attacker-controlled usernames of unbounded size in memory and the audit log
- **Where:** `crates/epiphany-api/src/auth.rs:165`  |  **Category:** security  |  **Found by:** api-auth-http

On every authentication miss the raw req.username - bounded only by the 8 MiB body cap and never length-validated - is inserted into the LoginGuard HashMap (retained for the 15-minute lockout window before prune drops it) and appended to the audit log (kept in the in-memory records Vec and fsynced to disk, subject only to the retention record count). An unauthenticated client sending large usernames can grow server memory by hundreds of MB to tens of GB over a lockout window and bloat audit.log, with no credential required. The Argon2 serialization (see the mutex finding) throttles the rate but does not bound the per-request size.

```
state
    .login_guard
    .lock()
    .expect("login guard mutex")
    .record_failure(&req.username, now);
audit(&state, &req.username, AuditAction::Login, None, false);
```

**Suggested fix:** Reject login requests whose username (and password) exceed a sane bound (e.g. 256 bytes) with 400 before touching the guard, the hasher, or the audit log; usernames that long can never authenticate anyway.

#### minor-40. Mutation handlers look up objects before authorization, leaking cube/view/subset existence (including private objects)
- **Where:** `crates/epiphany-api/src/query_routes.rs:553`  |  **Category:** security  |  **Found by:** api-model-endpoints

replace_view (553-557), delete_view (587-590), replace_subset (231-235), and delete_subset (266-269) call snapshot() and resolve the named object BEFORE require_cube_access, while their GET/LIST/CREATE siblings authorize first. Consequences: (1) a caller with NO access to the cube can distinguish 404 'no such view/subset' from 403, enumerating cube names and view/subset names inside cubes they may not read; (2) the lookup uses snap.view()/snap.subset() without the visibility filter, so a non-owner with cube Write receives 403 'you do not own this object' for a PRIVATE view/subset versus 404 for a missing one — revealing existence that visible_view/visible_subset deliberately hides behind 404 on the GET path. The same snapshot-before-authz ordering leaks cube existence from sandbox_routes handlers (e.g. list_sandboxes 112-113, commit_sandbox 209-210).

```
let snap = snapshot(&state, &cube)?;
let existing = snap
    .view(&name)
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "UNKNOWN_VIEW", "no such view"))?;
require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
```

**Suggested fix:** Authorize first in every mutating handler (matching the GET siblings), and use the visibility-filtered lookup (visible_view/visible_subset semantics) so a private object a caller cannot see 404s uniformly before the ownership check.

#### minor-41. Create-cube-by-reference materializes a shared dimension without any Dimension permission or element-ACL union check
- **Where:** `crates/epiphany-api/src/model_routes.rs:368`  |  **Category:** security  |  **Found by:** api-model-endpoints

create_cube gates only on require_manage_cubes (server admin OR a global Cube:Admin grant). A dimension spec with "ref": <id> materializes the full registry dimension — every member, edge, and attribute value — into the new cube, which has no element ACLs, and the creator can read it all via GET /cubes/{new}. This bypasses two fail-closed protections that guard every other path to those names: GET /dimensions/{id} masks members by the union of every referencing cube's element ACLs (ADR-0033, denied_registry_elements), and promote_dimension explicitly denies element-restricted callers because promotion 'would otherwise launder element names hidden from them' (dimension_routes.rs:556). A non-server-admin holding global Cube:Admin who is element-restricted on cube A (and lacks Dimension:Read) can thus enumerate A's hidden member names, edges, and attribute values by creating a throwaway cube referencing the dimension id.

```
for d in &body.dimensions {
    match d.reference {
        Some(id) => dims.push(CubeDimensionSpec::Ref(DimensionId(id))),
        None => dims.push(CubeDimensionSpec::Inline(build_dimension_def(
```

**Suggested fix:** When any dimension spec carries a ref, additionally require global Dimension:Read (or Write) and deny the create if denied_registry_elements over the dimension's current referencing cubes is non-empty for the caller, mirroring the promote guard.

#### minor-42. batch_write silently drops the optimistic base_version when a sandbox is selected
- **Where:** `crates/epiphany-api/src/routes.rs:279`  |  **Category:** bug  |  **Found by:** api-model-endpoints

When the X-Epiphany-Sandbox header is present, batch_write routes to sandbox_set_cells(&cube, None, ...) and req.base_version is silently discarded rather than honored or rejected. A client (e.g. the Excel add-in) that always sends base_version for conflict detection gets last-writer-wins semantics inside a sandbox with no indication its concurrency guard was ignored — two sessions staging into the same sandbox can silently clobber each other's what-if overrides. commit_sandbox honors base_version, so the write path is inconsistent with the commit path.

```
let outcome = match &sandbox_name {
    Some(name) => state.engine.sandbox_set_cells(&cube, None, name, &writes),
    None => state.engine.apply_batch(&cube, req.base_version, &writes),
}
```

**Suggested fix:** Pass req.base_version through to sandbox_set_cells (the engine's define path already supports a base check), or return a 422 stating base_version is unsupported for sandboxed batches so the client's expectation is never silently voided.

#### minor-43. require_element_write_indices fails open when an element index does not resolve
- **Where:** `crates/epiphany-api/src/authz.rs:374`  |  **Category:** security  |  **Found by:** api-model-endpoints

In the element-security gate used by spread_cells and commit_sandbox, a coordinate component whose index cannot be resolved to an element (dim.element(idx) returns Err) is treated as NOT denied (Err(_) => false) — the write passes the security check and is left for the engine to accept or reject. Every other branch of this module is written fail-closed (unknown principal => denied; unknown member on read => 403), and the function's own doc says it re-checks 'against the live store' precisely because state may have moved since staging. Today the coords come from the same snapshot so the Err arm is near-unreachable, but any future divergence (e.g. checking against a different snapshot than the one that produced the indices) silently skips the ACL check for that component instead of denying.

```
match dim.element(idx) {
    Ok(el) => !security.element_writable(&p, cube, dim.name(), &el.name),
    Err(_) => false,
}
```

**Suggested fix:** Treat an unresolvable index as denied (Err(_) => true), matching the module's fail-closed convention; the engine's later range validation still gives the caller a precise 422 when appropriate.

#### minor-44. Subset/view replace and delete probe object existence before the write-access gate
- **Where:** `crates/epiphany-api/src/query_routes.rs:231`  |  **Category:** security  |  **Found by:** api-workspaces

replace_subset, delete_subset, replace_view and delete_view take the snapshot and look up the existing object (returning 404 if absent) BEFORE calling require_cube_access(Write). A logged-in caller with no grant on the cube therefore receives 404 when the subset/view does not exist versus 403 when it does, disclosing the existence (and names) of cubes, subsets, and views to principals who have no read or write access to them. create_subset/get_subset correctly gate access first.

```
let snap = snapshot(&state, &cube)?;
let existing = snap.subset(&dim, &name)
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "UNKNOWN_SUBSET", "no such subset"))?;
require_cube_access(&state, &auth, &cube, AccessLevel::Write)?; // gate runs after existence probe
```

**Suggested fix:** Call require_cube_access(Write) before the snapshot/existence lookup in replace_subset, delete_subset, replace_view, and delete_view so an unauthorized caller always gets 403 regardless of whether the object exists.

#### minor-45. Flow run fetches global connection rows without Connection:Read
- **Where:** `crates/epiphany-api/src/flow_routes.rs:209`  |  **Category:** security  |  **Found by:** api-workspaces

resolve_flow_inputs (Global binding) and the legacy connection path in run_flow_handler resolve a global connection by name from the automation store and call fetch_connection_rows with only the connector runtime gates applied; the run itself is gated only by Flow:Write. There is no require_kind_access(Connection, Read) check, so a Flow:Write holder who lacks Connection:Read can fetch any global connection's output rows (e.g. a SQL query result or HTTP body) and surface them via ctx.log. ADR-0035 states authoring/running a flow must not grant data access the principal lacks; connection output is such data, and Connection:Read exists to gate the connection surface.

```
FlowInputBinding::Global => {
    let conn = { store.automation().connections.get(&input.name).cloned()
        .ok_or_else(|| ApiError::unprocessable("UNKNOWN_CONNECTION", ...))? };
    fetch_connection_rows(state, &conn)? // no Connection:Read gate
```

**Suggested fix:** Before fetching a global connection referenced by a flow input or the legacy `connection` field, require the run principal to hold Connection:Read (require_kind_access(Connection, None, Read)); local/inline connections are the runner's own definition and remain unaffected.

#### minor-46. Unrecognized EPIPHANY_TLS value silently disables TLS (fail-open to plaintext)
- **Where:** `crates/epiphany-server/src/config.rs:187`  |  **Category:** security  |  **Found by:** server-connect

Any value outside the accepted set (on/self-signed/1/true/yes) sets tls_self_signed = false with no diagnostic. An operator who sets EPIPHANY_TLS=enabled, =require, or =https believes HTTPS is on but the server serves plaintext HTTP, exposing session cookies and data if bound beyond loopback; the only signal is the scheme in one startup log line (plus the non-loopback warning). Note the value grammar is also inconsistent across the binary: EPIPHANY_OPEN_BROWSER is case-sensitive 1|true|yes (line 114), and EPIPHANY_ENABLE_*_CONNECTORS in main.rs accepts only 1|true - three different boolean grammars invite exactly this kind of typo.

```
        if let Some(v) = vars.get("EPIPHANY_TLS") {
            config.tls_self_signed = matches!(
                v.to_ascii_lowercase().as_str(),
                "on" | "self-signed" | "1" | "true" | "yes"
            );
        }
```

**Suggested fix:** Treat an unrecognized EPIPHANY_TLS value as a hard startup error (or at minimum a tracing::error), and unify one boolean-env grammar (accepting an explicit off set) across all EPIPHANY_* flags.

#### minor-47. secure_cookies derives from wanting TLS, not from actually serving TLS
- **Where:** `crates/epiphany-server/src/main.rs:242`  |  **Category:** security  |  **Found by:** server-connect

secure_cookies: config.wants_tls() is wrong in both directions. (1) On a build without the tls feature with EPIPHANY_TLS=on, the ADR-0019 fallback serves plain HTTP (main.rs lines 294-297) but the session cookie is still marked Secure - modern browsers refuse to store a Secure cookie set over an insecure non-localhost origin, so every login silently fails (the warning log never mentions cookies). (2) In the deployment DEPLOYMENT.md itself recommends - TLS terminated at a reverse proxy - the backend serves HTTP, so the session cookie for a public HTTPS site ships without the Secure attribute (RG-12 gap) and there is no override knob to force it on.

```
        secure_cookies: config.wants_tls(),
...
        #[cfg(not(feature = "tls"))]
        tracing::warn!(
            "TLS was requested (EPIPHANY_TLS*) but this build lacks the `tls` feature; serving plain HTTP"
        );
```

**Suggested fix:** Compute secure_cookies from the mode actually being served (false on the cfg(not(feature = "tls")) fallback path), and add an explicit EPIPHANY_SECURE_COOKIES override for TLS-terminating-proxy deployments.

#### minor-48. Malformed EPIPHANY_* values are silently ignored, including EPIPHANY_BIND=localhost:8080
- **Where:** `crates/epiphany-server/src/config.rs:104`  |  **Category:** bug  |  **Found by:** server-connect

Every env parse uses .and_then(|v| v.parse().ok()) and silently keeps the default on failure. SocketAddr does not parse hostnames, so the very plausible EPIPHANY_BIND=localhost:8080 (or a value with a typo'd port) silently binds 127.0.0.1:8080; likewise garbage in EPIPHANY_SESSION_TTL_SECS, EPIPHANY_LOGIN_MAX_FAILURES, etc. reverts to defaults with no diagnostic. The direction is fail-safe for bind (loopback), but operators get a server configured differently from what they wrote, and for the security knobs (lockout, TTL) the configured hardening quietly does not apply.

```
        if let Some(addr) = vars.get("EPIPHANY_BIND").and_then(|v| v.parse().ok()) {
            config.bind_addr = addr;
        }
```

**Suggested fix:** Keep from_map pure but have it return (Config, Vec<ParseWarning>) - or validate in from_env - and log a tracing::warn (or fail startup) for every EPIPHANY_* value that was present but unparseable. Consider resolving hostnames for EPIPHANY_BIND via ToSocketAddrs.

#### minor-49. .env.example documents env vars the server never reads (EPIPHANY_BIND_ADDR, EPIPHANY_DETERMINISTIC, EPIPHANY_SEED)
- **Where:** `.env.example:4`  |  **Category:** smell  |  **Found by:** server-connect

The example file names EPIPHANY_BIND_ADDR, but config.rs reads EPIPHANY_BIND (DEPLOYMENT.md has the correct name) - an operator copying the example gets a silently ignored bind setting (compounded by the silent-fallback behavior above). It also advertises EPIPHANY_DETERMINISTIC=1 and EPIPHANY_SEED as a deterministic server mode; I verified by grep that no code anywhere reads either variable (the only occurrence in the repo is this file), so setting them does nothing. (Positive side-effect of that verification: there is no deterministic/fixed-key path into self-signed cert generation - tls.rs always uses rcgen/ring OS randomness.)

```
# EPIPHANY_BIND_ADDR=127.0.0.1:8080
...
# Run the server in deterministic mode (fixed clock/RNG/seed) for tests:
# EPIPHANY_DETERMINISTIC=1
# EPIPHANY_SEED=20200101
```

**Suggested fix:** Fix EPIPHANY_BIND_ADDR to EPIPHANY_BIND and delete (or clearly mark as unimplemented) the EPIPHANY_DETERMINISTIC / EPIPHANY_SEED entries so nobody expects - or someday naively implements - a deterministic RNG feeding TLS key generation.

#### minor-50. timeout_ms=0 means 'run forever' for the command connector but '30s default' for HTTP/SQL
- **Where:** `crates/epiphany-connect/src/lib.rs:153`  |  **Category:** smell  |  **Found by:** server-connect

The same CommandSpec/HttpSpec/SqlSpec field has opposite semantics across connectors: http.rs (lines 36-40) and sql.rs (lines 66-70) coerce 0 to a 30s default 'so a misconfigured connection cannot hang', while the command connector treats 0 as no timeout at all - the poll loop never expires and the process runs unbounded. The REST boundary coerces 0 to 30s (connection_routes.rs line 377), so this only arises from a hand-edited model (a trusted boundary per ADR-0012), but it contradicts ADR-0012 decision 6.4 ('a timeout kills an overrunning process') and combines badly with the reader-join hang: a zero-timeout command has no backstop whatsoever.

```
                if spec.timeout_ms != 0 && start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
```

**Suggested fix:** Give the command connector the same 0-means-default-30s rule as the HTTP and SQL connectors (a shared const in epiphany-connect), so no spec value can produce an unbounded process.

#### minor-51. Feeder inference under-feeds rules whose input is a rule-overridden consolidated cell, contradicting the ADR-0005 soundness claim
- **Where:** `crates/epiphany-calc/src/feeders.rs:199`  |  **Category:** olap  |  **Found by:** olap-core-semantics

The fixpoint's 'potent' set is seeded from stored leaves and grows only via fed leaf targets; input_potent expands a consolidated input to its contributing leaves and checks those against 'potent'. A consolidation-override rule (e.g. ['Region':'Total','Measure':'Sales'] = 1000) is skipped by inference (no leaf targets) and never enters 'potent', so a downstream rule reading that overridden consolidated cell (Net = value['Region':'Total','Measure':'Sales']) is judged not potent and NOT fed even though its true value is non-zero everywhere. That rule is classified analyzable (same-cube input), so it is neither fed nor reported opaque - a silent under-feed, exactly what ADR-0005 claims can never happen for an analyzable rule ('it never under-feeds an analyzable rule'). Harmless today only because no read path consumes feeders; the day consolidate_fed is wired into reads this becomes silently-low totals. validate_feeders would flag it, but only if someone runs diagnostics.

```
let mut potent: BTreeSet<Vec<u32>> = cube.cell_entries().map(|(coord, _)| coord).collect();
// input_potent, line 394:
cartesian_any(&per_dim, |coord| potent.contains(coord))
```

**Suggested fix:** Treat an input whose expanded region intersects any rule area (especially a consolidation-override area) as potent, or feed the reading rule's whole target area when an input is covered by an override rule.

#### minor-52. Explain traces list both IF branches as inputs and omit cells read by the condition
- **Where:** `crates/epiphany-calc/src/provenance.rs:90`  |  **Category:** olap  |  **Found by:** olap-core-semantics

explain_node uses feeders::collect_cells to enumerate a firing rule's inputs. collect_cells recurses into then/otherwise but deliberately skips CCond (feeders.rs:343-352, 'cond: _'), which is sound for feeding but wrong for provenance: the trace shows input cells from the branch that was NOT taken (with values that did not contribute), and hides the cells the condition actually read to decide the branch. For a conditional rule ('IF value[Flag] > 0 THEN value[A] ELSE value[B]') a user auditing a number sees A and B as inputs and never sees Flag - the one cell that determined the outcome. The module doc claims 'the input cells consulted', which this does not deliver for conditionals.

```
let mut cells = Vec::new();
collect_cells(&rule.expr, &mut cells);
// feeders.rs collect_cells:
CExpr::If { cond: _, then, otherwise } => {
    collect_cells(then, out); ...
```

**Suggested fix:** Give provenance its own input walk that evaluates the condition, collects the condition's cells, and recurses only into the taken branch (or tags untaken-branch inputs as such).

#### minor-53. Feeder diagnostics abort wholesale on the first rule evaluation error (e.g. DivByZero) instead of reporting
- **Where:** `crates/epiphany-calc/src/feeders.rs:276`  |  **Category:** bug  |  **Found by:** olap-core-semantics

validate_feeders evaluates every leaf target of every rule with `engine.value(ordinal, &target)?` - any CalcError (division by an unpopulated cell is the canonical case for unguarded ratio rules, also Cycle/Overflow) propagates and fails the whole validation, so GET /feeders/diagnostics returns an error instead of a diagnostics report for exactly the models that most need diagnosing. It also materializes the full dense cartesian target area up front (area_leaf_coords, with an unchecked usize product at line 317 and a Vec of every coordinate), so a broad rule area over large dimensions exhausts memory before validating anything - unlike spread.rs, which caps and saturates the same product.

```
for target in area_leaf_coords(cube, &rule.area) {
    if engine.value(ordinal, &target)? != Fixed::ZERO && !index.contains(&target) {
        under.insert(target);
    }
}
```

**Suggested fix:** Record per-target evaluation errors as a third diagnostics bucket (rule, coord, error) instead of propagating; stream the area enumeration instead of collecting it, with a saturating product and a documented cap like MAX_SPREAD_LEAVES.

#### minor-54. MDX .Children/.Descendants return element-index order, discarding the authored rollup order
- **Where:** `crates/epiphany-mdx/src/eval.rs:171`  |  **Category:** olap  |  **Found by:** olap-query-semantics

The evaluator derives children from Dimension::edges(), which sorts canonically by (parent, child index). Dimension actually preserves the authored child order per parent (children[parent], exposed via Dimension::children_of at dimension.rs:421), but MDX ignores it. In established MDX/TM1 semantics, .Children returns members in hierarchy order — the order children were attached to the consolidation. Consequence: for a dimension whose elements were created in one order (e.g. alphabetically by an import flow) but whose rollup was authored in another (Jan..Dec under Year), a dynamic subset `[Year].Children` orders report rows by element creation index (Apr, Aug, Dec, ...) instead of the authored month order, with no way for the modeler to fix it short of reordering the whole element table. Using children_of would be equally deterministic.

```
fn children_of(dim: &Dimension, parent: u32) -> Vec<u32> {
    let kids: Vec<u32> = dim
        .edges()
        .into_iter()
        .filter(|&(p, _, _)| p == parent)
        .map(|(_, child, _)| child)
        .collect();
    dedup(kids)
}
```

**Suggested fix:** Base children_of (and the Descendants adjacency map) on Dimension::children_of's authored edge order rather than the canonically sorted edges(); both are deterministic, only one matches OLAP hierarchy-order expectations.

#### minor-55. MDX empty-set axis is rejected with 422 instead of producing an empty axis
- **Where:** `crates/epiphany-api/src/query_routes.rs:785`  |  **Category:** olap  |  **Found by:** olap-query-semantics

The parser accepts `{}` on an axis (parser.rs test `select_accepts_nary_crossjoin_axis_and_empty_axis`), and core execute_view treats an empty member list as a valid empty cellset. But view_from_mdx cannot determine a dimension for an empty set (axis_dimension returns None), so `SELECT ... , {} ON ROWS FROM [C]` — and any Crossjoin with an empty-set component — fails with 422 MDX_EVAL_ERROR ('cannot determine the dimension for an axis set'). Standard MDX empty-set propagation yields an empty axis and zero cells, not an error. The endpoint accepts the syntax at parse time and then errors at lowering, which is confusing for clients that legitimately construct degenerate queries.

```
for component in flatten_crossjoin(set) {
    let name = axis_dimension(component).ok_or_else(|| {
        ApiError::unprocessable(
            "MDX_EVAL_ERROR",
            "cannot determine the dimension for an axis set; qualify members as [Dim].[Member]",
        )
    })?;
```

**Suggested fix:** Treat a component that evaluates to an empty set with no determinable dimension as an empty axis (skip it, or emit an AxisSpec::Members with empty members for a dimension chosen from a sibling component), returning an empty cellset per MDX empty-set semantics.

#### minor-56. Cellset endpoint reports string cells as numeric zeros with kind hardcoded to "numeric"
- **Where:** `crates/epiphany-api/src/query_routes.rs:1011`  |  **Category:** olap  |  **Found by:** olap-query-semantics

execute_view fills the grid only through CellResolver::value (Fixed); string_value is never consulted, and cellset_dto stamps every cell kind: "numeric". A view whose tuple addresses a String (S) element returns value "0", kind "numeric", editable true — indistinguishable from a genuine numeric zero — while the single-cell read endpoint (routes.rs read_one) correctly returns kind "string" with the text for the same coordinate. The web pivot works around it by using batch cell reads, and PivotGrid.tsx:298 contains a `cell.kind === 'string'` check that can never fire against this endpoint. Any API consumer using /views/{name}/execute, /cellset, or /mdx on a cube with comment/text measures silently receives fabricated numeric zeros where text values exist.

```
CellsetCellDto {
    value: Some(value.to_string()),
    kind: "numeric",
    editable: context_leaf
        && row_leaf.get(r).copied().unwrap_or(false)
        && col_leaf.get(c).copied().unwrap_or(false),
```

**Suggested fix:** In cellset_dto, detect tuples whose resolved coordinate contains a String element (the kinds are already resolved for member_dto) and emit kind "string" with the string_value (or value: None), matching the single-cell read contract; document the cellset schema in openapi.rs.

#### minor-57. View cache is bounded by entry count only; version-keyed churn can pin hundreds of megabytes of dead cellsets
- **Where:** `crates/epiphany-api/src/view_cache.rs:43`  |  **Category:** performance  |  **Found by:** olap-query-semantics

The cache admits cellsets up to 1,048,576 cells per entry and holds up to 256 saved-view entries (plus the ad-hoc pool), with no byte accounting. Because every commit mints a new version and the version is in the key, a dashboard alternating writes and reads of one large view accumulates a dead superseded copy per version — each ~8 MB of Fixed values plus tuple-name vectors for a 1M-cell view — and superseded entries are only reclaimed by LRU when the pool hits its entry cap. Worst case is multi-gigabyte residency (256 x ~8+ MB) for a server whose mandate is strict memory efficiency and whose cell store is CI-bounded to 24 bytes/cell. ADR-0028 decision 5 bounds entries, not bytes, and asserts 'one giant view cannot consume the budget' — but 256 near-ceiling entries can.

```
pub const DEFAULT_ENTRIES: usize = 256;
...
/// Cellsets larger than this are not cached (computed fresh, stored nothing), so
/// one very large view cannot dominate the cache's memory.
const MAX_CACHE_CELLS: usize = 1 << 20; // 1,048,576
```

**Suggested fix:** Track approximate bytes per entry (cells.len() * size_of::<Fixed>() plus tuple-name lengths) and evict by a byte budget as well as entry count; additionally, drop superseded-version entries for the same (cube, shape, scope) eagerly on insert since they can never be hit again.

#### minor-58. element_mask rebuilds an O(all elements) deny mask per request while holding the global security mutex
- **Where:** `crates/epiphany-api/src/authz.rs:241`  |  **Category:** performance  |  **Found by:** olap-query-semantics, perf-lens

Every cellset execution, cell read, member listing, and preview for a non-admin on a cube with any element ACL iterates every element of every dimension, calling security.element_readable per element, all under the state.security mutex. With ADR-0032-scale member tables (100k+ elements) this is hundreds of thousands of ACL evaluations per request, serialized across all requests by the mutex — a hot-path throughput trap precisely for the security-conscious deployments the scoped cache tier was designed to serve. The mask result depends only on (principal's effective groups, cube ACLs, dimension membership version), all of which change far less often than reads occur.

```
if security.has_element_acls(cube_name, dim.name()) {
    for (idx, el) in dim.iter_elements().enumerate() {
        if !security.element_readable(&principal, cube_name, dim.name(), &el.name) {
            dim_denied.push(idx as u32);
            any = true;
        }
    }
}
```

**Suggested fix:** Memoize the computed ElementMask keyed on (principal-relevant ACL revision, cube version) — a small per-user cache invalidated by a security-store generation counter — or at minimum resolve the principal's group set once and evaluate ACL rules per rule rather than per element name.

#### minor-59. View execution resolves private subsets without a visibility check, letting any reader enumerate another user's private subset
- **Where:** `crates/epiphany-api/src/query_routes.rs:724`  |  **Category:** security  |  **Found by:** olap-query-semantics

The direct subset endpoints enforce visibility (visible_subset: public OR owner OR admin), but the execute paths do not: execute_adhoc_view passes `|d, n| snap.subset(d, n)` (and Model::execute uses self.subset) with no can_read filter. Any principal with Cube:Read can POST an ad-hoc view spec whose axis is `{type: "subset", subset: "<name>"}` and read the private subset's full resolved membership back out of the cellset's row_tuples — bypassing the Private visibility contract ('Visible only to its owner'). Element security still applies (denied members are dropped), so cell values are safe; only the private object's member selection leaks, but that selection is exactly what Private is meant to hide.

```
execute_view(
    snap.cube(),
    view,
    &*resolver,
    &|d, n| snap.subset(d, n),
    state.evaluator(),
    mask.as_ref(),
)
```

**Suggested fix:** Wrap the subset_lookup closures in execute_adhoc_view and execute_saved_view (and validate_view on create) with the same can_read(principal, owner, visibility) predicate the subset endpoints use, returning UnknownSubset for an invisible subset so existence does not leak.

#### minor-60. Crash-recovery Interrupted records are appended to the durable run ledger in HashMap iteration order
- **Where:** `crates/epiphany-flow/src/ledger.rs:321`  |  **Category:** determinism  |  **Found by:** determinism-lens

RunLedger::recover_interrupted iterates `self.latest.values()` — `latest` is a std HashMap<String, usize> (line 150) with RandomState, so its iteration order differs on every process run. Each interrupted run gets an Interrupted record appended (and fsync'd) to the on-disk ledger file in that order. Two identical crash recoveries (same ledger bytes on open, same set of active runs) therefore produce differently-ordered durable ledger files, breaking the mandate's stable-ordering requirement for stored bytes and making any byte-level recovery test flaky. REST views re-sort (latest_records), so the user-visible impact is limited to the persisted artifact, but the file is the durable record of runs.

```
let interrupted: Vec<RunRecord> = self
    .latest
    .values()
    .map(|&i| &self.records[i])
    .filter(|r| r.state.is_active())
    ...
for record in interrupted {
    self.append(record)?;
```

**Suggested fix:** Sort the interrupted set before appending — e.g. by (fire_millis, id), the same total order enforce_retention and latest_records use — or make `latest` a BTreeMap<String, usize>, which fixes this and the retention tie-break in one change.

#### minor-61. Run-ledger retention tie-break for equal fire_millis resolved by HashMap iteration order
- **Where:** `crates/epiphany-flow/src/ledger.rs:258`  |  **Category:** determinism  |  **Found by:** determinism-lens

enforce_retention builds `latest` from `self.latest.values()` (RandomState hash order) and then selects each job's protected latest-successful run with `if r.fire_millis >= entry.0` — a >= comparison whose winner on a fire_millis tie is whichever record the hash iteration visits last. Ties between distinct run ids of the same job at the same fire_millis are reachable: a manual kick uses id `manual:{name}:{fire_millis}` (job_routes.rs:211) while the scheduler uses `sched:{cube}:{job}:{fire_millis}`, and under a frozen ManualClock (the determinism harness) a kick and a timer firing share fire_millis. When both tied runs fall outside the newest-max window, which one survives compaction — in the durable ledger file and in subsequent /runs REST listings and last-succeeded queries — is nondeterministic across identical runs.

```
let mut latest: Vec<RunRecord> = self
    .latest
    .values()
    .map(|&i| self.records[i].clone())
    .collect();
...
    if r.fire_millis >= entry.0 {
        *entry = (r.fire_millis, r.id.clone());
```

**Suggested fix:** Break the fire_millis tie with a total order on the run id, e.g. `if (r.fire_millis, &r.id) > (entry.0, &entry.1)` (or sort `latest` by (fire_millis, id) before the scan). Switching `latest` to a BTreeMap also removes the order dependence at the source.

#### minor-62. Slug-migration collision winner decided by filesystem read_dir order
- **Where:** `crates/epiphany-server/src/boot.rs:107`  |  **Category:** determinism  |  **Found by:** determinism-lens

migrate_cube_dirs collects candidate cube folders straight from std::fs::read_dir without sorting (unlike load_or_init, which sorts at line 42) and renames each to slug(cube name) in that order. When two distinct legacy folders slug to the same target and neither already sits at the canonical name (e.g. 'My Cube!' and 'MY CUBE?'), the first folder visited wins the canonical slug directory and the second is skipped with a warning. read_dir order is unspecified and platform/filesystem-dependent, so which cube ends up at the canonical on-disk location — and which one load_or_init subsequently keys/overwrites when display names also collide — differs across runs and platforms for identical on-disk inputs. Narrow (legacy layouts with slug collisions only), but it is a boot-time decision about durable layout made in unordered iteration.

```
let dirs: Vec<std::path::PathBuf> = entries
    .filter_map(Result::ok)
    .map(|e| e.path())
    .filter(|p| p.join("snapshot.model").is_file())
    .collect();

for path in dirs {
```

**Suggested fix:** Add `dirs.sort();` after collecting (matching load_or_init line 42) so the collision winner is the lexicographically-first folder on every platform and run.

#### minor-63. Boot reconcile of registry-backed cubes swallows errors, leaving a lagging cube silently divergent
- **Where:** `crates/epiphany-engine/src/lib.rs:362`  |  **Category:** smell  |  **Found by:** concurrency-durability-lens

with_dimensions_dir's reconcile pass — the mechanism ADR-0024 relies on to bring a cube that missed a fan-out before a crash back into lockstep with the registry — discards the result of define_elements entirely (`let _ =`). If reconcile fails (e.g. a kind conflict because the cube's copy genuinely diverged, or a checkpoint I/O error), the cube silently stays behind the registry generation: its members and rollups differ from every sibling cube referencing the same shared dimension, and nothing is logged, so neither the operator nor a later fan-out ever learns the two are out of sync.

```
for (cube, els, edgs) in reconcile {
    if self.has_cube(&cube) {
        let _ = self.define_elements(&cube, None, &els, &edgs);
    }
}
```

**Suggested fix:** Propagate or at least record reconcile failures (return them from with_dimensions_dir for the server to log, or track a per-cube 'lagging' flag surfaced in the API) so divergence between a cube and its shared dimension is visible instead of silent.

#### minor-64. A lagged WebSocket subscriber is silently skipped past dropped change events, so a slow client can stay stale indefinitely
- **Where:** `crates/epiphany-api/src/ws.rs:91`  |  **Category:** concurrency  |  **Found by:** concurrency-durability-lens

The change stream uses a bounded broadcast channel (capacity 256, main.rs:174); when a slow consumer lags, pump swallows RecvError::Lagged and continues without telling the client anything. The module comment justifies this with "clients refetch on any event", but a Lagged drop delivers NO event: if a write burst overflows the buffer and then the system goes quiet (the common end-of-load pattern), the dropped notifications were the last ones, the client never receives any event to trigger a refetch, and its grid silently shows stale numbers until the next unrelated commit. The bounded channel is the right call for memory (no unbounded old-snapshot retention — verified), but the lag path needs a resync signal.

```
// A slow client may miss events; keep the connection (clients refetch).
Err(broadcast::error::RecvError::Lagged(_)) => {}
```

**Suggested fix:** On Lagged, send the client a synthetic resync event (e.g. a Hello or a new `Resync` variant) so it knows to refetch everything it displays; that keeps the bounded channel while restoring the 'clients refetch on any event' invariant.

#### minor-65. Interactive what-if write rewrites the entire model snapshot to TOML per write
- **Where:** `crates/epiphany-persist/src/store.rs:481`  |  **Category:** performance  |  **Found by:** perf-lens

Every sandbox cell write ends in checkpoint(), which serializes the whole model — every populated base cell rendered as member-name strings, sorted, TOML-encoded — and fsyncs it (write_snapshot, store.rs:833-848; text.rs:1228-1250). Typing 20 what-if values into a grid therefore rewrites the full N-cell snapshot 20 times; at even 1M base cells that is hundreds of MB of TOML I/O per editing session, on an interactive path. ADR-0014 explicitly records the per-sandbox delta file as a 'later scaling optimization', so this is documented debt rather than a bug — flagged because it is the single worst latency cliff a business user will hit (what-if entry is the polished persona surface), and it compounds with the full-cube validation clone in the same call (store.rs:459).

```
for write in writes {
    if let CellWrite::Leaf { coord, value } = write {
        sb.cells.insert(coord.clone(), *value);
    }
}
sb.updated = updated;
self.checkpoint()
```

**Suggested fix:** Prioritize the ADR-0014 deferral: log sandbox overrides to the WAL (a name- or index-addressed SetSandboxCell record) or a per-sandbox delta file, checkpointing only on commit/discard. That makes what-if entry O(batch) like base writes.

#### minor-66. WebSocket fanout re-serializes and clones the full batch coordinate list per subscriber
- **Where:** `crates/epiphany-api/src/ws.rs:103`  |  **Category:** performance  |  **Found by:** perf-lens

CellsChanged events carry the complete coords list of a batch (routes.rs:283-287: one CoordMap of dimension-name -> member-name strings per written cell; an 8 MiB body limit allows ~100k-cell batches). tokio broadcast clones the whole event per subscriber, and send_event runs serde_json::to_string per subscriber per event — so one large Excel/flow batch with 50 connected dashboard clients costs 50 deep clones plus 50 JSON serializations of a multi-megabyte payload. The module doc itself says clients refetch on any event and do not rely on the payload, so most of this work is wasted; large events also sit in the 256-slot broadcast buffer, pinning memory.

```
async fn send_event(socket: &mut WebSocket, event: &ChangeEvent) -> Result<(), axum::Error> {
    let json = serde_json::to_string(event).unwrap_or_default();
    socket.send(Message::Text(json.into())).await
}
```

**Suggested fix:** Serialize each event once at send time and broadcast an Arc<str> (Message::Text accepts shared bytes), and cap or drop the coords list for large batches (e.g. send count + version only above a threshold) since clients refetch anyway.

#### minor-67. Sandbox 'overlaid' flag re-resolves every cell's full coordinate from member names
- **Where:** `crates/epiphany-api/src/query_routes.rs:993`  |  **Category:** performance  |  **Found by:** perf-lens

When a sandbox is active, cellset_dto calls cell_coord_indices for every cell of the grid: for each of R×C cells it re-walks row/col dimension name lists (linear position scans), re-hashes every member-name string through index_of, and allocates a fresh Vec<u32>. The same row tuple is re-resolved ncols times and the same column tuple nrows times. For a 100k-cell sandboxed view that is ~100k Vec allocations and ~rank×100k string hash lookups on every read — including view-cache hits, since the DTO transform runs on the hit path by design (ADR-0028 decision 1).

```
let overlaid = sandbox.is_some_and(|sb| {
    cs.row_tuples
        .get(r)
        .zip(cs.column_tuples.get(c))
        .and_then(|(rt, ct)| {
            cell_coord_indices(
                cube,
                &cs.row_dimensions,
                rt,
```

**Suggested fix:** Precompute the partial index tuple once per row tuple and once per column tuple (plus the context once), then merge the two precomputed halves per cell — O(R+C) resolutions instead of O(R×C), with no per-cell allocation.

#### minor-68. computeHeaderSpans groups tuple prefixes with a space join, which collides for member names containing spaces
- **Where:** `web/src/model/tree.ts:99`  |  **Category:** bug  |  **Found by:** web-core

Header-span run detection builds the group key by joining member keys/names with a plain space. Element names may legitimately contain spaces (only control characters are rejected server-side, model_routes.rs:309), so two adjacent but distinct tuples can produce identical prefix strings — e.g. ("North America", "Sales") and ("North", "America Sales") both join to "North America Sales" — and get merged into a single header cell spanning both columns/rows in the executed-cellset grid (the no-key path). The user then reads numbers against the wrong header label. The file itself already reserves U+0001 as the tuple separator precisely because it cannot appear in a name (comment at line 179), but this function does not use it.

```
  const prefixKey = (tuple: ReadonlyArray<{ name: string; key?: string }>, level: number): string =>
    tuple
      .slice(0, level + 1)
      .map((m) => m.key ?? m.name)
      .join(' ')
```

**Suggested fix:** Join with the reserved U+0001 tuple separator (or PATH_SEP-adjacent control character) instead of ' ', matching the convention documented at tree.ts:179-181.

#### minor-69. batchWrite is dead code and never exposes the server's optimistic-concurrency base_version
- **Where:** `web/src/api/client.ts:245`  |  **Category:** smell  |  **Found by:** web-core

The server's batch endpoint supports optimistic concurrency: BatchWriteRequest carries an optional base_version and rejects the commit with 409 if the cube moved on (dto.rs:131-138, routes.rs:280). The client's batchWrite neither accepts nor sends base_version, so that protection is unreachable from the web UI — and in fact no component imports batchWrite at all (PivotGrid/CellsetGrid use only writeCell/spreadCells), so the web client has no atomic multi-cell write path and all concurrent edits are silently last-writer-wins. Either the client function is untested dead weight, or a future caller will adopt it and unknowingly forgo the conflict detection the server was built to provide.

```
export async function batchWrite(
  cube: string,
  writes: { coord: Coord; value: string }[],
): Promise<BatchResult> {
  return request<BatchResult>('POST', `/api/v1/cubes/${encodeURIComponent(cube)}/cells/batch`, {
    writes,
  })
}
```

**Suggested fix:** Add an optional baseVersion parameter that is forwarded as base_version (callers already hold CellsetDto.version), and either wire batchWrite into the grid's multi-cell operations or delete the export until it has a caller.

#### minor-70. Session bootstrap treats any getMe failure (network/5xx) as "not signed in"
- **Where:** `web/src/App.tsx:41`  |  **Category:** bug  |  **Found by:** web-core

The one-time bootstrap swallows every rejection identically: a genuine 401 (no cookie session) and a transient network error or server 5xx during page load both leave session null and render the Login screen. A user with a perfectly valid HttpOnly cookie session who reloads during a blip is silently bounced to Login and re-enters credentials needlessly (and the client can't distinguish this from expiry because ApiError carries the status that is being ignored). The comment acknowledges the conflation but the ApiError.status field exists precisely to branch on it.

```
      // No active session (401 / ApiError / network): treat as "not signed in",
      // leave session null, and show no error banner.
      .catch(() => {})
```

**Suggested fix:** Branch on err instanceof ApiError && err.status === 401 to show Login; for network/5xx failures show a lightweight retry state (or retry getMe once) instead of discarding a live cookie session.

#### minor-71. request() offers no AbortSignal or timeout, so in-flight reads can neither be cancelled nor time out
- **Where:** `web/src/api/client.ts:138`  |  **Category:** smell  |  **Found by:** web-core

None of the ~70 API functions accept an AbortSignal, and request() sets no timeout, so a caller switching cubes/views cannot cancel a superseded read — components can only emulate cancellation with `cancelled` booleans (and not all do: ViewWorkspace.run() has no sequencing, so a WS-triggered re-run overlapping a manual run can resolve out of order and commit the older cellset). A stalled connection also hangs the fetch indefinitely, leaving busy spinners stuck forever with no error surfaced. For a live multi-user tool whose grids refetch on every WebSocket change event, abandoned in-flight requests also keep consuming server work and bandwidth.

```
async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const response = await fetch(path, {
    method,
    headers: authHeaders(body !== undefined),
    credentials: 'same-origin',
    body: body === undefined ? undefined : JSON.stringify(body),
  })
```

**Suggested fix:** Thread an optional { signal?: AbortSignal } through request() and the read-path helpers (readCells, executeAdhoc, executeView, executeMdx, getCube); components then pass an AbortController aborted in effect cleanup, which both fixes out-of-order overwrites and frees server work for abandoned reads. Consider an AbortSignal.timeout default for reads.

#### minor-72. buildElementTree eagerly materializes every DAG path, exponential under stacked alternate rollups
- **Where:** `web/src/model/tree.ts:52`  |  **Category:** performance  |  **Found by:** web-core

buildElementTree expands the whole dimension into one node per path occurrence up front (the per-path multiplication is documented as intended for display), unlike flattenForest which the pivot uses lazily, expanding only opened nodes. With alternate rollups the number of paths can grow combinatorially (k stacked diamond patterns yield 2^k occurrences of the leaves), and the function is called eagerly on every dimension load in DimensionEditor.tsx:208, MemberTable.tsx:151, and for every dimension of a cube in modelExplorerTree.ts:197 — so a large dimension with layered alternate hierarchies can freeze the main thread building tree nodes for occurrences the user will never expand. This conflicts with the project's ultra-performance mandate even though the per-occurrence display itself is by design.

```
  const build = (name: string, parentPath: string, ancestry: Set<string>): TreeNode => {
    const path = parentPath ? `${parentPath}/${name}` : name
    const next = new Set(ancestry).add(name)
    const children = (childrenOf.get(name) ?? [])
      .filter((child) => !next.has(child))
      .map((child) => build(child, path, next))
```

**Suggested fix:** Build children lazily (compute a node's children on first expansion, as flattenForest already does for the pivot), or cap eager materialization with a node budget and fall back to on-demand expansion beyond it.

#### minor-73. Stale cube-load failure can paint an error banner over a successfully loaded cube
- **Where:** `web/src/components/PivotGrid.tsx:248`  |  **Category:** concurrency  |  **Found by:** web-components

The cube-load effect's .then guards on `cancelled`, but the .catch does not. Switching cubes (or retrying) while a slow getCube for the previous cube is in flight lets that request's late rejection call setError after the new cube's effect already cleared error and loaded detail. Because `detail` is set, the render falls through to the inline error <p role="alert"> (line 911), showing a stale, wrong-cube failure message over a healthy grid until the next successful refresh clears it.

```
getCube(cube)
  .then((loaded) => {
    if (cancelled) return
    ...
  })
  .catch((err: unknown) =>
    setError(err instanceof Error ? err.message : 'Failed to load cube'),
  )
```

**Suggested fix:** Mirror the .then guard: `if (!cancelled) setError(...)` in the catch.

#### minor-74. CSV import target-cube detail effect has no cancellation: mapping UI can show the wrong cube's dimensions
- **Where:** `web/src/components/FlowsWorkspace.tsx:117`  |  **Category:** concurrency  |  **Found by:** web-components

Unlike the neighboring effects (which all use a `live` flag), the getCube(importCube) effect applies whichever response resolves last. Picking cube A (slow) then cube B (fast) leaves `detail` = A while `importCube` = B, so ImportPanel offers cube A's dimensions for column mapping and fixed members while the header says 'into B'; the subsequent importCsv against B then fails (or maps to wrong members if names coincide across cubes).

```
useEffect(() => {
  if (!importCube) {
    setDetail(null)
    return
  }
  getCube(importCube)
    .then(setDetail)
    .catch(() => setDetail(null))
}, [importCube, reloadSignal])
```

**Suggested fix:** Add the same `let live = true` cancellation used by the other effects in this file, or store {cube, detail} together and render ImportPanel only when detail.cube === importCube.

#### minor-75. Debounced MDX/rules/flow previews have no response sequencing; an older response can overwrite a newer one
- **Where:** `web/src/components/SubsetEditor.tsx:50`  |  **Category:** concurrency  |  **Found by:** web-components

The 300ms debounce clears the pending timer but not in-flight requests: once previewMdx has been dispatched, a subsequent edit fires a new request, and if the older one resolves last, setPreview paints member counts/results for stale MDX against the current text ('Resolves to N members' for an expression the user no longer has). The same fire-and-forget pattern (debounce without a generation guard) is in RulesWorkspace.tsx:91-104 (previewRules) and FlowsWorkspace.tsx:210-223 (previewFlow), where a stale 'Valid'/error verdict can gate the Save button against the wrong source. PivotGrid's refreshGen shows the codebase already has the right pattern for this.

```
const handle = setTimeout(() => {
  previewMdx(cube, dimension.name, mdx)
    .then((members) => {
      setPreview(members)
      setPreviewError(null)
    })
```

**Suggested fix:** Capture a generation counter (or the mdx string) when dispatching and ignore the response if it is no longer current — the refreshGen pattern from PivotGrid.tsx:204 applies directly to all three preview effects.

#### minor-76. MemberTable ARIA table structure is broken by unrole'd wrapper divs, and aria-sort usage contradicts the documented guideline
- **Where:** `web/src/components/MemberTable.tsx:592`  |  **Category:** smell  |  **Found by:** web-components

The role="table" scroll container's body rows sit inside two generic <div>s (mtable__body and the translateY spacer) with no role. WAI-ARIA requires role=table to own rows (optionally via rowgroup); interposed generic containers break the row/columnheader association in the accessibility tree, undermining exactly the APG aria-rowcount/aria-rowindex wiring ADR-0032 calls 'mandatory ... or a future WCAG audit fails'. Separately, ariaSort() (lines 301-302) sets aria-sort="none" on every unsorted header (and on all headers when sorted by model order), while docs/UI_UX_GUIDELINES.md ('Make sorting obvious, accessible...') explicitly says to apply aria-sort only to the single sorted header and remove the attribute rather than set "none".

```
<div className="mtable__body" style={{ height: virtual.totalHeight }}>
  <div style={{ transform: `translateY(${virtual.offsetTop}px)` }}>
    ...
    <div ... role="row" aria-rowindex={absIndex + 2} ...>
```

**Suggested fix:** Give the two wrapper divs role="presentation" (or make the inner one role="rowgroup" and hang the spacer off padding), and change ariaSort to return undefined for unsorted headers so the attribute is omitted.

#### minor-77. SandboxBar unconditionally bumps the global reload on every mount, cascading refetches app-wide
- **Where:** `web/src/components/SandboxBar.tsx:34`  |  **Category:** performance  |  **Found by:** web-components

On mount (every cube-tab open; it is keyed by cube), load() always calls apply(want) — even for the default 'Base data' with no persisted sandbox — and apply() unconditionally calls onChange(), which is CubeApp's bumpReload. One reload bump makes ModelExplorer re-run the loader of every expanded tree node, PivotGrid re-fetch its full cellset (in addition to its own initial refresh, i.e. a double fetch of the whole grid on open), and RulesWorkspace/FlowsWorkspace/JobsWorkspace re-list. Simply opening a cube tab therefore triggers an app-wide refetch storm unrelated to any actual change — against the memory/perf mandate and needless server load in multi-user sessions.

```
const apply = useCallback(
  (name: string) => {
    setActive(name)
    setActiveSandbox(name === BASE ? null : name)
    if (name === BASE) localStorage.removeItem(storageKey)
    else localStorage.setItem(storageKey, name)
    onChange()
  },
```

**Suggested fix:** Only call onChange() when the applied sandbox actually differs from the current one (track previous value, or skip onChange during the initial load when resolving to BASE).

#### minor-78. Silent 'last column is Value' fallback and silent blank-coordinate drops in write-back
- **Where:** `excel-addin/src/CommitService.cs:103`  |  **Category:** bug  |  **Found by:** excel-addin

If the selected header row contains no 'Value' header, BuildWrites silently assumes the last column holds values (line 103), excludes that header from coordinates, and commits. Combined with silently skipping blank coordinate cells (line 120: 'if (string.IsNullOrEmpty(member)) continue;'), a mis-selected range (off-by-one column, missing header, a stray blank cell) is reinterpreted rather than rejected. I verified the server backstop in crates/epiphany-api/src/resolve.rs (coord.len() != cube.rank() is rejected), so most mistakes abort the whole batch with a confusing rank error - but when the guessed coordinate count happens to equal the cube's rank (e.g. low-rank cubes), wrong values are written transactionally and reported as 'Committed N cell(s)'. A write-back path should fail fast on ambiguous input, not guess.

```
int valueCol = Array.FindLastIndex(headers, h => h.Equals("Value", StringComparison.OrdinalIgnoreCase));
if (valueCol < 0) valueCol = cols - 1; // fall back to the last column
```

**Suggested fix:** Require an explicit 'Value' header (error otherwise, matching the README's contract), and treat a blank coordinate cell in a data row as an error for that commit instead of silently dropping the dimension.

#### minor-79. Multi-area selections commit only the first area, silently
- **Where:** `excel-addin/src/CommitService.cs:88`  |  **Category:** bug  |  **Found by:** excel-addin

ReadSelection uses Range.Value2 on the raw Selection. For a multi-area selection (Ctrl-click discontiguous blocks - common when users exclude rows), COM returns Value2 of the FIRST area only. The remaining areas are silently ignored, so the user believes the whole selection was committed when only part of it was; the only clue is the count in the success dialog. This undermines the 'whole selection becomes ONE transactional POST' contract in the file's own doc comment.

```
dynamic app = ExcelDnaUtil.Application;
dynamic selection = app.Selection;
int rows = (int)selection.Rows.Count;
int cols = (int)selection.Columns.Count;
if (rows < 2 || cols < 2) return null;
// Value2 of a multi-cell range is a 1-based object[,].
return (object[,])selection.Value2;
```

**Suggested fix:** Check selection.Areas.Count and either iterate all areas into one batch (reusing the first area's header) or refuse multi-area selections with a clear message.

#### minor-80. TOCTOU on AddIn.Client between null-check and background use surfaces a raw NullReferenceException in cells
- **Where:** `excel-addin/src/Functions.cs:44`  |  **Category:** concurrency  |  **Found by:** excel-addin

Read null-checks AddIn.Client on the calc thread (line 23) but dereferences it later with the null-forgiving operator on a ThreadPool thread. AddIn.Client is a plain static mutated from the UI thread (Sign out sets it null, Connect replaces it) with no synchronization or volatility. If the user signs out while reads are in flight, the lambda throws NullReferenceException and the cell displays '#EPIPHANY: Object reference not set to an instance of an object.' - a raw runtime message, contradicting the ribbon's stated 'errors surface as plain dialogs, never raw stack traces' contract. The same unsynchronized cross-thread pattern applies to Client.Sandbox written from OnSandboxChanged while pool threads read it.

```
var value = AddIn.Client!.ReadCellAsync(cube, coord).GetAwaiter().GetResult();
```

**Suggested fix:** Capture the client once on entry (var client = AddIn.Client; if (client is null) return ...;) and use the captured reference inside the lambda, so sign-out mid-flight yields the friendly 'not connected' message instead of an NRE.

#### minor-81. Any bare string postMessage from the server origin is accepted as the auth token; http origins allowed
- **Where:** `excel-addin/src/ConfiguratorForm.cs:104`  |  **Category:** security  |  **Found by:** excel-addin

OnWebMessage treats ANY JSON string message from the configured origin as the session token: it is immediately DPAPI-persisted, installed on the shared client, and the form closes announcing 'Connected.' Any same-origin script that posts an unrelated string (an analytics shim, a future feature of the React app, third-party JS served from the same origin) silently corrupts the stored credential - every subsequent EPIPHANY.READ then fails with 401s until the user re-connects, and garbage is persisted to disk as a 'token'. Additionally OriginMatches only compares scheme/host/port with no https requirement, so a user who types http:// sends the bearer token in cleartext with no warning. The typed envelope { type: 'epiphany-auth', token } already exists - the bare-string fallback undermines it.

```
if (doc.RootElement.ValueKind == JsonValueKind.String)
    token = doc.RootElement.GetString();
else if (doc.RootElement.TryGetProperty("type", out var t)
         && t.GetString() == "epiphany-auth"
         && doc.RootElement.TryGetProperty("token", out var tok))
    token = tok.GetString();
```

**Suggested fix:** Accept only the typed 'epiphany-auth' envelope, and either require https for non-loopback hosts or show an explicit cleartext warning when the base URL is http.
