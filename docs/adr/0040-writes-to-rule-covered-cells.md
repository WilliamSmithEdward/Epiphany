# ADR-0040: Writes to rule-covered cells

- **Status:** Accepted
- **Date:** 2026-07-02
- **Deciders:** Maintainer
- **Phase:** Post-roadmap (data-entry hardening, continues ADR-0021 and ADR-0029)

## Context

A leaf cell can be *covered* by a calculation rule: a rule area (ADR-0007) whose
target includes the leaf's coordinate, so every read returns the rule's computed
value. Feeders and the rule evaluator make that value authoritative on the read
path — a covered leaf is a formula cell, not a stored one.

The write path never learned this. `write_cell`, `batch_write`,
`sandbox_set_cells`, and `spread_cells`
(`crates/epiphany-api/src/routes.rs`) accept and persist a stored value into a
rule-covered leaf, but the next read returns the *rule* value, so the user's
entered number silently disappears. `write_cell` even re-reads after the commit
and returns the rule value, not what was written — a value the caller can never
observe. Spreading is worse: `spread_cells` distributes the entered total across
*all* contributing leaves, including rule-covered ones, so the total is not
reproducible on read-back (the covered leaves ignore their written share). And
`CellDto.editable` / `CellsetCellDto.editable` are derived purely from
leaf-ness (`routes.rs`, `query_routes.rs::cellset_dto`), so the UI actively
invites the user to type into a cell whose value it will then overwrite from a
rule.

Forces:

- **No silent data loss.** A write that cannot take effect must fail loudly, not
  vanish. This is the same fail-loud posture the rest of the system took in the
  2026-07 hardening pass (a stored value the engine will never surface is data
  loss from the user's point of view).
- **Display must match enforcement.** The `editable` flag the client reads to
  decide whether to offer an input must agree with what the write path will
  accept. A cell shown as editable that rejects the write is a UX trap; a cell
  shown read-only that would accept a write is confusing. One source of truth.
- **Established multidimensional behavior.** Incumbent in-memory OLAP servers
  reject data writes to rule-calculated cells and skip them during spreading.
  Users coming from those tools expect the same, and it is the safe default.
- **The compiled model is already at hand.** The API resolves reads through a
  `PinnedRegistry` of every cube's `CompiledModel` (`calc_factory.rs`), which
  exposes `matching_rule(cube, coord) -> Option<RuleId>` — the exact,
  cross-cube, precedence-ordered coverage test. No new machinery is needed to
  answer "is this coordinate rule-covered."
- **The cellset DTO is hot.** `cellset_dto` runs on view-cache hits (ADR-0028),
  so a per-cell coverage check must be cheap and must not defeat the cache.

## Decision

**Reject** a direct write whose coordinate is covered by a rule, rather than
silently shadowing it. Spreading **excludes** rule-covered leaves. The
`editable` flag reflects coverage, so the client never offers the write in the
first place. Concretely:

1. **Coverage test.** A leaf coordinate is *rule-covered* iff
   `CompiledModel::matching_rule(cube, coord)` is `Some` for the cube's compiled
   rules in the pinned registry. This is the same authority the read path uses,
   so display, write enforcement, and spread exclusion cannot diverge. Only leaf
   coordinates are tested on the write path (a consolidated coordinate is
   already not directly writable; spreading is how one enters at a consolidation,
   ADR-0029).

2. **Write paths reject covered leaves.** `write_cell`, `batch_write`, and
   `sandbox_set_cells` reject a write to a rule-covered leaf with a typed
   `422 Unprocessable Entity`, error code **`RULE_COVERED_CELL`**, message
   naming the coordinate and the covering rule id. A batch is all-or-nothing: if
   any cell in the batch is rule-covered the whole batch is rejected and nothing
   is written (consistent with the existing element-security batch semantics,
   ADR-0029 point 7). The check runs against the same pinned snapshot the write
   commits against, so it cannot race a concurrent rule change.

3. **The rejection applies in sandboxes too.** A what-if overlay is consulted
   *beneath* the rules (ADR-0014): a rule-covered leaf reads as the rule value
   even with a sandbox override present, so a sandbox write to a covered leaf is
   just as futile as a base write. `sandbox_set_cells` rejects it identically,
   keeping base and what-if behavior the same.

4. **Spreading excludes covered leaves.** `spread_leaves`' expansion (ADR-0029)
   drops any contributing leaf that is rule-covered before distributing, so the
   entered total is spread only across the leaves that can actually hold it and
   is therefore reproducible on read-back. If **every** contributing leaf is
   rule-covered, the spread is rejected with `RULE_COVERED_CELL` (there is
   nowhere to put the value) rather than writing nothing and reporting success.
   Because coverage lives in the compiled model (API layer), the exclusion is
   applied where the API expands the spread target into leaf coordinates, not
   inside the pure core engine (which has no rules) — the covered set is passed
   in, mirroring the injected `read_leaf` reader the core spread already takes.

5. **`editable` reflects coverage in both DTOs.** `read_one` (`CellDto`) and
   `cellset_dto` (`CellsetCellDto`) set `editable: false` for a rule-covered
   leaf, in addition to the existing leaf-ness test. The two paths share one
   coverage helper so the flag is computed identically. A covered cell therefore
   renders read-only in the pivot grid and the client never offers an input for
   it.

6. **Keep the DTO hot path cheap (ADR-0028).** `cellset_dto` computes coverage
   only when the target cube actually has compiled rules (the common no-rules
   cube skips the test entirely and keeps the pure leaf-ness flag), and only for
   leaf cells (a consolidated cell is already `editable: false`). The compiled
   model is the one already built for the read; no extra compile or snapshot is
   taken, and the cache is not defeated (coverage is a function of the cube
   version already in the cache key, ADR-0028).

## Alternatives considered

- **Document silent shadowing as intended.** Keep persisting the stored value
  and let the rule win on read. Rejected: it is indistinguishable from data loss
  — the user's number is accepted and then never observable — and it makes
  spreading totals non-reproducible. No planning tool should silently discard an
  entered number.
- **Store the write and let it "show through" where no rule applies.** Persist
  the value so it surfaces if the rule is later deleted. Rejected: it hides a
  latent value that reappears on an unrelated model edit (a surprise months
  later), and it still reads wrong in the meantime. If the user wants the leaf to
  be writable, the fix is to narrow the rule, not to stash a shadow value.
- **Reject only in the UI (editable flag), allow the API.** Rejected: the API is
  a public surface (Excel add-in, scripts, flows write cells too). Enforcement
  must be at the write path; the flag is an ergonomic mirror of it, not the
  gate.
- **Compute coverage inside the core engine.** Rejected: `epiphany-core` is
  rules-free by design (calc lives in `epiphany-calc`, reached through the
  injected resolver seam, ADR-0011). Coverage is threaded from the API's
  compiled model, exactly as spreading already injects its value reader.

## Consequences

- A write to a rule-calculated cell now fails with a clear, typed 422 instead of
  silently vanishing; the entered value is never lost-without-trace. Spreads
  land only on real leaves and reproduce their total on read-back.
- Display and enforcement share one coverage helper and one authority
  (`matching_rule` over the pinned compiled model), so a cell shown editable is
  writable and vice versa — no UX traps and no drift between the read grid and
  the write path.
- New surface: a `RULE_COVERED_CELL` API error code and a small coverage helper
  in the API layer; the spread expander gains a covered-leaf exclusion input. No
  new core/calc dependency and no change to the pure spread engine's contract
  (the covered set is injected like the value reader). No new third-party
  dependency.
- Cost: the write paths and the cellset DTO do an `O(rules)` area-match per leaf
  coordinate. It is skipped entirely for cubes with no rules and for
  consolidated cells, and reuses the compiled model already built for the read,
  so the view-cache hot path stays cheap.
- Validated by `tower::oneshot` integration tests
  (`crates/epiphany-api/tests/`): a direct write, a batch, a sandbox write, and
  an all-covered spread to a rule-covered leaf each return 422
  `RULE_COVERED_CELL`; a spread across a mix of covered and uncovered leaves
  writes only the uncovered ones and reads back the entered total; and the
  cellset/`read_one` DTOs report `editable: false` for a covered cell and
  `true` for an uncovered leaf in the same cube.
- Revisitable: a future "hold" feature (temporarily freezing a rule so a leaf
  can be overridden) or an explicit per-rule "allow manual override" flag would
  relax the rejection deliberately; both are out of scope here and neither is
  needed for the safe default.
