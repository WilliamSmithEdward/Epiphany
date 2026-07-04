# Follow-ups

Tracking for work deliberately left out of the 2026-07 review-remediation branch
(`review-fixes-2026-07-02`). Everything in the review's finding set and every
deferred item raised during remediation was implemented; the entries below are
genuinely out of the current architecture's scope or are deliberate judgments
worth revisiting, each with the reason it was not done now.

## Architectural (needs a design/ADR decision, not a bug)

- **OS-level flow resource isolation (CPU / RSS).** Flows run on the embedded
  `boa` engine in-process (ADR-0004, single self-contained binary). We enforce a
  deterministic loop/recursion budget AND (new) a wall-clock deadline watchdog
  that aborts a wedged run and frees the caller with a clean 503. But `boa`
  cannot be preempted, so an aborted run's thread is detached and keeps consuming
  CPU until it unwinds, and there is no hard memory cap. True CPU/RSS reclamation
  requires running each flow in a subprocess (or WASM sandbox), which contradicts
  the single-binary design. Revisit only if flow abuse becomes a real operational
  problem; it would be its own ADR (process/WASM isolation vs. the single-binary
  mandate).

- **Rule scope markers — feeder interaction (ADR-0041).** `C:`-scoped rules
  target consolidated coordinates, which have no leaf feeders, so a `C:` rule
  contributes no leaf targets to feeder inference (consistent, documented). If a
  future rule form needs a `C:` rule to *also* drive feeding, that is a separate
  feeder-model extension.

## Deliberate judgments worth a second look

- **Change-feed event granularity.** The commit-ordered change feed (via the
  engine `CommitObserver`) reports `{cube, version, sandbox}` only, so object
  edits now surface to clients as `cells_changed` rather than `objects_changed`.
  This is behavior-preserving (clients refetch on either) and security-tightening
  (object-edit notifications now respect cube-read). If a client ever needs to
  distinguish the two, thread an event-kind through the ~17 engine commit sites.

- **StringPool compaction is opt-in.** Monotonic pool growth remains the default
  (ADR-0006); `Cube::compact_string_pool()` reclaims orphans on demand. A
  checkpoint-time auto-compaction threshold could be wired if long-running churn
  is observed.

- **`personaFromGrants` (web) is now unused at runtime.** The shell reads the
  authoritative server persona from `auth/me`; the client-side derivation helper
  is retained (exported + unit-tested) as a fallback/utility but no longer drives
  the UI. Remove it and its test if it stays unused.

## Requires infrastructure not present here

- **Excel add-in in-Excel load test.** The add-in is statically build-verified
  (Excel is absent from CI, per ADR-0022). The read-coalescing / `Observe` async
  path and the WebView2 configurator still need a manual load test in a real
  Excel session before release.

- **Cross-platform flow-tree kill test (`epiphany-connect`).** The Unix
  process-group kill and Windows `taskkill /T` paths are implemented; the
  process-group test is `#[cfg(not(windows))]` and runs on Unix CI (the dev host
  is Windows).
