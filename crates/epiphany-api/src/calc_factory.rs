//! The rule-aware value resolver factory and a pinned multi-cube registry.
//!
//! [`CalcFactory`] implements the engine's `CellResolverFactory` seam: it builds a
//! [`PinnedRegistry`] whose TARGET cube is the caller's pinned read snapshot (so a
//! cellset's axes, reported version, and values are one MVCC version, ADR-0001),
//! compiles each cube's rules once, and hands back a resolver that overlays
//! rule-derived values and reuses one calc memo across the cellset's cells
//! (ADR-0007). The composition root injects it; tests inject the engine's
//! `StoredCellsFactory` instead. [`PinnedRegistry`] is also the eval-time registry
//! the explain and diagnostics endpoints build (over fresh snapshots, since they
//! are not bound to a reader's pinned version).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::thread::ThreadId;

use std::str::FromStr;

use epiphany_calc::rules::RuleParseError;
use epiphany_calc::{
    compile, rules, safe_fed_set, AssertionFailure, CalcEngine, CalcError, CalcMemo, CompileError,
    CompiledModel, CubeRegistry, EvalRegistry, FedGate, SandboxOverlay, TestOutcome, TestRunError,
};
use epiphany_core::{CellResolver, Cube, ElementMask, Fixed, Model, QueryError, RuleTest, Sandbox};
use epiphany_engine::{CellResolverFactory, Engine, ReadSnapshot};

/// Why validating a rule source against the live model failed.
pub(crate) enum ValidateError {
    /// The source did not parse.
    Parse(RuleParseError),
    /// The source parsed but did not compile against the model.
    Compile(CompileError),
    /// The target cube does not exist.
    UnknownCube(String),
}

/// Parse and compile `source` for `target_cube` against the engine's current
/// cubes (so cross-cube references resolve), without storing anything. Used to
/// validate a rule definition before persisting it.
pub(crate) fn compile_source(
    engine: &Engine,
    target_cube: &str,
    source: &str,
) -> Result<(), ValidateError> {
    let names = engine.cube_names();
    let snaps: Vec<ReadSnapshot> = names.iter().filter_map(|n| engine.snapshot(n)).collect();
    let target = engine
        .snapshot(target_cube)
        .ok_or_else(|| ValidateError::UnknownCube(target_cube.to_string()))?;
    let cr = SnapCubes {
        snaps: &snaps,
        names: &names,
    };
    let doc = rules::parse(source).map_err(ValidateError::Parse)?;
    compile(target.cube(), &cr, &doc, target.version())
        .map(|_| ())
        .map_err(ValidateError::Compile)
}

/// A compile-time cube registry over a set of pinned snapshots (name -> ordinal,
/// ordinal -> cube), used while compiling cross-cube references.
struct SnapCubes<'a> {
    snaps: &'a [ReadSnapshot],
    names: &'a [String],
}

impl CubeRegistry for SnapCubes<'_> {
    fn ordinal(&self, name: &str) -> Option<u32> {
        self.names.iter().position(|n| n == name).map(|i| i as u32)
    }
    fn cube(&self, ordinal: u32) -> Option<&Cube> {
        self.snaps.get(ordinal as usize).map(|s| s.cube())
    }
}

/// An eval-time registry: every cube's pinned snapshot plus its compiled rules,
/// captured together so a query (including cross-cube reads) is consistent.
///
/// Each cube's `models` entry is `Ok(model)` when its rule source compiles
/// against the pinned model, or `Err(message)` when it does not. A compile
/// failure is NOT silently degraded to a rule-less cube (which would revert every
/// rule-derived cell to stored/aggregated values): the evaluator consults
/// [`EvalRegistry::compile_error`] and fails the read loud instead (see
/// [`CalcEngine::compute`]). Rules are validated at define time, but a later
/// structural edit (element delete/rename) can invalidate a previously-valid
/// rule with no re-validation, which is exactly the case this guards.
pub(crate) struct PinnedRegistry {
    snaps: Vec<ReadSnapshot>,
    models: Vec<Result<CompiledModel, String>>,
    names: Vec<String>,
    /// Per-cube sparse-consolidation license (ADR-0005), indexed by ordinal:
    /// `Some(fed)` when the completeness gate ([`safe_fed_set`]) proved the sparse
    /// [`Cube::consolidate_fed`] byte-identical to the dense path for that cube,
    /// carrying the complete fed leaf set; `None` when the cube must use the dense
    /// path. Computed ONCE at build time (not per cell). Surfaced through
    /// [`EvalRegistry::fed_set`], which the evaluator consults only for a base read
    /// (no overlay, no mask) — the gate's proof holds for the stored data + rules;
    /// an overlay or mask is handled by the evaluator declining the sparse path.
    fed: Vec<Option<Vec<Box<[u32]>>>>,
}

impl PinnedRegistry {
    /// Snapshot every cube FRESH and compile its rules once. Used by the explain
    /// and diagnostics endpoints, which are not bound to a caller's pinned read
    /// snapshot. For the MVCC read path (a cellset execution keyed on a pinned
    /// version) use [`build_pinned`](Self::build_pinned) so the target cube's
    /// values come from the reader's snapshot, not a re-snapshot.
    pub(crate) fn build(engine: &Engine) -> Self {
        let names = engine.cube_names();
        let snaps = names.iter().filter_map(|n| engine.snapshot(n)).collect();
        Self::from_snaps(snaps)
    }

    /// Build a registry whose TARGET cube is the caller's pinned `snapshot`, so a
    /// cellset's values come from the exact version its axes and reported version
    /// were resolved against (MVCC read isolation, ADR-0001). A commit landing
    /// between `engine.snapshot()` and here must not change the target's values.
    ///
    /// The other cubes (needed only for cross-cube rule references) are snapshot
    /// fresh: a cross-cube dependency is not part of the target cube's own MVCC
    /// version (per-cube versions), so pinning them to the reader's target
    /// snapshot is neither possible nor more correct; a fresh sibling snapshot is
    /// the same one `build` would use. The target's pinned snapshot is always
    /// present (even if the cube was dropped concurrently), so the target read is
    /// always consistent and `ordinal_of(target)` always resolves.
    pub(crate) fn build_pinned(engine: &Engine, snapshot: &ReadSnapshot) -> Self {
        let target_name = snapshot.cube().name();
        // Fresh snapshots of the SIBLING cubes (skipping the target and any cube
        // dropped between listing and snapshotting), plus the pinned target.
        let mut snaps: Vec<ReadSnapshot> = engine
            .cube_names()
            .iter()
            .filter(|n| n.as_str() != target_name)
            .filter_map(|n| engine.snapshot(n))
            .collect();
        snaps.push(snapshot.clone());
        Self::from_snaps(snaps)
    }

    /// Compile every captured snapshot's rules once, recording a per-cube
    /// `Ok(model)` or `Err(rendered error)`. Names/ordinals are taken from the
    /// snapshots themselves, so a cube dropped mid-capture never leaves a hole.
    /// Shared by [`build`](Self::build) and [`build_pinned`](Self::build_pinned).
    fn from_snaps(snaps: Vec<ReadSnapshot>) -> Self {
        let names: Vec<String> = snaps.iter().map(|s| s.cube().name().to_string()).collect();
        let cr = SnapCubes {
            snaps: &snaps,
            names: &names,
        };
        let models = snaps
            .iter()
            .map(|s| compile_snapshot(&cr, s))
            .collect::<Vec<_>>();
        // Build the registry with the sparse path DISABLED (`fed` all-`None`), then
        // compute the per-cube completeness gate against it. The gate's dense-truth
        // validation reads values through a `CalcEngine` over `&self`; with `fed`
        // still `None`, `fed_set` returns `None`, so that validation runs on the
        // always-correct dense path — it must not consult the fed set it is deciding.
        let count = snaps.len();
        let mut registry = Self {
            snaps,
            models,
            names,
            fed: vec![None; count],
        };
        registry.fed = (0..count as u32)
            .map(|ord| match safe_fed_set(&registry, ord) {
                // Provably byte-identical to dense: license the sparse path.
                Ok(FedGate::Safe(fed)) => Some(fed),
                // Any doubt (opaque/under-fed/erroring rules, no model) or a gate
                // evaluation error: stay on the dense path. Dense is always correct,
                // so a gate failure only forgoes an optimization, never a value.
                Ok(FedGate::Dense(_)) | Err(_) => None,
            })
            .collect();
        registry
    }

    /// The ordinal of a cube by name.
    pub(crate) fn ordinal_of(&self, name: &str) -> Option<u32> {
        self.names.iter().position(|n| n == name).map(|i| i as u32)
    }
}

/// Parse and compile one cube's rule source against the pinned cube set, mapping
/// any parse/compile failure to a rendered message (so the read path can fail
/// loud rather than silently drop the rules). An empty/valid source compiles to
/// an `Ok` model (possibly with no rules).
fn compile_snapshot(cr: &SnapCubes<'_>, snap: &ReadSnapshot) -> Result<CompiledModel, String> {
    let doc = rules::parse(&snap.rules().source).map_err(|e| e.to_string())?;
    compile(snap.cube(), cr, &doc, snap.version()).map_err(|e| e.to_string())
}

impl EvalRegistry for PinnedRegistry {
    fn cube(&self, ordinal: u32) -> Option<&Cube> {
        self.snaps.get(ordinal as usize).map(|s| s.cube())
    }
    fn compiled(&self, ordinal: u32) -> Option<&CompiledModel> {
        self.models
            .get(ordinal as usize)
            .and_then(|m| m.as_ref().ok())
    }
    fn ordinal(&self, name: &str) -> Option<u32> {
        self.ordinal_of(name)
    }
    fn compile_error(&self, ordinal: u32) -> Option<&str> {
        self.models
            .get(ordinal as usize)
            .and_then(|m| m.as_ref().err())
            .map(String::as_str)
    }
    fn fed_set(&self, ordinal: u32) -> Option<&[Box<[u32]>]> {
        self.fed.get(ordinal as usize).and_then(|f| f.as_deref())
    }
}

/// An eval registry for one rule-test run (A6/CA4): the target cube is a caller-
/// supplied clone with the test's fixtures applied, while every OTHER cube (and all
/// compiled rules, including cross-cube references) comes from the pinned
/// [`PinnedRegistry`]. This is what lets a cross-cube rule test actually run:
/// calc's built-in `run_rule_tests` compiles the model against a `SingleCube` and
/// returns a `CrossCube` limitation error for any `'Other'![...]` reference, but the
/// production model compiles fine against the full multi-cube registry. Fixtures
/// change only stored leaf VALUES, not structure, so the pinned compiled rules stay
/// valid for the fixtures-modified cube. `fed_set` is left at the trait default
/// (`None`) so evaluation always takes the dense path — the sparse license was
/// proved against the ORIGINAL stored data, which the fixtures may have changed.
struct FixtureRegistry<'a> {
    base: &'a PinnedRegistry,
    target: u32,
    fixed_cube: Cube,
}

impl EvalRegistry for FixtureRegistry<'_> {
    fn cube(&self, ordinal: u32) -> Option<&Cube> {
        if ordinal == self.target {
            Some(&self.fixed_cube)
        } else {
            self.base.cube(ordinal)
        }
    }
    fn compiled(&self, ordinal: u32) -> Option<&CompiledModel> {
        self.base.compiled(ordinal)
    }
    fn ordinal(&self, name: &str) -> Option<u32> {
        self.base.ordinal(name)
    }
    fn compile_error(&self, ordinal: u32) -> Option<&str> {
        // Preserve fail-loud: a cube whose stored rules no longer compile must error
        // the test, exactly as calc's own runner would.
        self.base.compile_error(ordinal)
    }
    // `fed_set` intentionally uses the default `None` (dense path).
}

/// Resolve a `{dimension: member}` test coordinate to element indices in dimension
/// order against `cube`, mapping a missing/unknown member to a [`TestRunError`]
/// (matching calc's own `resolve_coord`).
fn resolve_test_coord(
    cube: &Cube,
    coord: &BTreeMap<String, String>,
) -> Result<Vec<u32>, TestRunError> {
    let mut out = Vec::with_capacity(cube.rank());
    for d in 0..cube.rank() {
        let dim = cube.dimension(d);
        let member = coord
            .get(dim.name())
            .ok_or_else(|| TestRunError::BadCoord(format!("missing dimension '{}'", dim.name())))?;
        let idx = dim.resolve(member).ok_or_else(|| {
            TestRunError::BadCoord(format!("unknown member '{member}' in '{}'", dim.name()))
        })?;
        out.push(idx);
    }
    Ok(out)
}

/// Run the target cube's rule tests against the pinned MULTI-cube registry (A6/CA4),
/// so a cross-cube rule test evaluates on real values instead of erroring with the
/// single-cube limitation calc's `run_rule_tests` reports.
///
/// `registry` is the production [`PinnedRegistry`] (every cube's pinned snapshot +
/// compiled rules); `target` is the tested cube's ordinal; `model` supplies its
/// tests (and the base cube to clone). For each test — in name order, deterministic
/// — the target cube is cloned, the fixtures applied as stored leaves, and every
/// assertion evaluated through a [`CalcEngine`] over a [`FixtureRegistry`] (so rule
/// references into other cubes resolve against their pinned snapshots). A rule that
/// fails to compile still surfaces (via `compile_error`) as a loud error, and a
/// malformed coordinate/fixture is a [`TestRunError`], mirroring calc's contract.
pub(crate) fn run_cross_cube_rule_tests(
    registry: &PinnedRegistry,
    target: u32,
    model: &Model,
) -> Result<Vec<TestOutcome>, TestRunError> {
    // A compile failure on the target's own rules is a loud error, not a silently
    // rule-less run (calc's runner fails the whole run on a bad compile too). The
    // registry renders the error to a string, so surface it as `RulesFailed` — the
    // same fail-loud signal `CalcEngine::value` would raise mid-evaluation — rather
    // than fabricating a structured `CompileError`.
    if let Some(err) = registry.compile_error(target) {
        let cube = registry
            .cube(target)
            .map(|c| c.name().to_string())
            .unwrap_or_default();
        return Err(TestRunError::Calc(CalcError::RulesFailed {
            cube,
            error: err.to_string(),
        }));
    }
    let mut outcomes = Vec::with_capacity(model.tests.len());
    // `model.tests` is a BTreeMap, so iteration is name-ordered and deterministic.
    for test in model.tests.values() {
        outcomes.push(run_one_cross_cube(registry, target, &model.cube, test)?);
    }
    Ok(outcomes)
}

/// Run one rule test against the multi-cube registry: clone the target cube, apply
/// the fixtures, then check each assertion through the rule-aware engine.
fn run_one_cross_cube(
    registry: &PinnedRegistry,
    target: u32,
    base_cube: &Cube,
    test: &RuleTest,
) -> Result<TestOutcome, TestRunError> {
    // A fresh clone isolates this test's fixtures from the live cube.
    let mut fixed_cube = base_cube.clone();
    for fixture in &test.fixtures {
        let coord = resolve_test_coord(&fixed_cube, &fixture.coord)?;
        let value = Fixed::from_str(&fixture.value).map_err(TestRunError::Model)?;
        fixed_cube
            .set_leaf(&coord, value)
            .map_err(TestRunError::Model)?;
    }

    let fixture_registry = FixtureRegistry {
        base: registry,
        target,
        fixed_cube,
    };
    let engine = CalcEngine::new(&fixture_registry);

    let mut failures = Vec::new();
    for assertion in &test.assertions {
        let coord = resolve_test_coord(&fixture_registry.fixed_cube, &assertion.coord)?;
        let actual = engine.value(target, &coord).map_err(TestRunError::Calc)?;
        let expected = Fixed::from_str(&assertion.value).map_err(TestRunError::Model)?;
        if actual != expected {
            failures.push(AssertionFailure {
                coord: assertion.coord.clone(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
    }
    Ok(TestOutcome {
        name: test.name.clone(),
        passed: failures.is_empty(),
        failures,
    })
}

/// Rule coverage for one cube (ADR-0040): whether a leaf coordinate is *computed*
/// by a calculation rule, so a stored write to it would be silently shadowed on
/// read. This is the single authority behind the write-path rejection, the spread
/// exclusion, and the `editable` DTO flag, so display and enforcement cannot
/// diverge — all three ask `covers`.
///
/// It compiles only the target snapshot's own rules (cross-cube references
/// resolved against the engine's other cubes, which coverage does not otherwise
/// depend on — a rule *area* is defined over the target cube). A cube that
/// declares no rules — the common case — skips the compile entirely and covers
/// nothing, so the probe is a cheap `false`; a cube whose rules fail to compile
/// (already fail-loud on the read path) likewise covers nothing here.
pub(crate) struct RuleCoverage {
    model: Option<CompiledModel>,
}

impl RuleCoverage {
    /// Build coverage for `snapshot`'s cube against the engine's cube set. Cheap
    /// no-op (no compile, no sibling snapshots) when the cube declares no rules.
    pub(crate) fn build(engine: &Engine, snapshot: &ReadSnapshot) -> Self {
        let model = if snapshot.rules().source.trim().is_empty() {
            None
        } else {
            let names = engine.cube_names();
            let snaps: Vec<ReadSnapshot> =
                names.iter().filter_map(|n| engine.snapshot(n)).collect();
            let cr = SnapCubes {
                snaps: &snaps,
                names: &names,
            };
            // A parse/compile failure => `None` (uncovered): the read path already
            // fails loud on broken rules, so we do not also block writes on it.
            rules::parse(&snapshot.rules().source)
                .ok()
                .and_then(|doc| compile(snapshot.cube(), &cr, &doc, snapshot.version()).ok())
        };
        Self { model }
    }

    /// A coverage that covers nothing — no compiled model. For unit tests that
    /// build a `Cellset` DTO without a live engine.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self { model: None }
    }

    /// Whether the cube has any compiled rules. Callers use this to skip the
    /// per-cell `covers` probe entirely on the hot cellset path.
    pub(crate) fn has_rules(&self) -> bool {
        self.model.as_ref().is_some_and(|m| !m.rules.is_empty())
    }

    /// Whether a rule computes the leaf at `coord` (index-addressed, in `cube`'s
    /// dimension order). `cube` must be the same cube the coverage was built for.
    pub(crate) fn covers(&self, cube: &Cube, coord: &[u32]) -> bool {
        self.model
            .as_ref()
            .is_some_and(|m| m.matching_rule(cube, coord).is_some())
    }
}

/// A what-if overlay for one target cube (ADR-0014): the sandbox's numeric leaf
/// overrides, consulted beneath the rules. Owned by the resolver (and by the
/// explain handler) so it lives as long as each per-read [`CalcEngine`] borrows
/// it.
pub(crate) struct OwnedOverlay {
    target: u32,
    cells: BTreeMap<Vec<u32>, Fixed>,
    scope: u64,
}

impl OwnedOverlay {
    /// Build an overlay of `sandbox`'s numeric leaves for cube ordinal `target`.
    /// The scope id (the sandbox's injected created id, forced non-zero) keeps
    /// the memo from aliasing a base value.
    pub(crate) fn new(target: u32, sandbox: &Sandbox) -> Self {
        Self {
            target,
            cells: sandbox.cells.clone(),
            scope: sandbox.created.max(1),
        }
    }
}

impl SandboxOverlay for OwnedOverlay {
    fn leaf(&self, ordinal: u32, coord: &[u32]) -> Option<Fixed> {
        if ordinal == self.target {
            self.cells.get(coord).copied()
        } else {
            None
        }
    }

    fn scope_id(&self) -> u64 {
        self.scope
    }
}

/// A [`CellResolver`] that overlays rules for one target cube, backed by a pinned
/// multi-cube registry, optionally overlaying a sandbox's what-if leaves.
///
/// One resolver serves every cell of a cellset execution, so the ADR-0007 memo is
/// reused ACROSS cells rather than rebuilt per cell (a cellset reads a consolidated
/// input's leaves many times). The resolver cannot hold a single long-lived
/// `CalcEngine`, because the engine borrows the registry the resolver owns (a
/// self-referential borrow); instead each `value` call builds a cheap engine over
/// the owned registry and is SEEDED with, then reclaims, a [`CalcMemo`].
///
/// Because `execute_view` may fill the grid from several threads (ADR-0028 Stage
/// B) through one shared `&self`, the memo is partitioned per thread: each thread
/// gets its own [`CalcMemo`], reused across the contiguous band of cells it
/// computes (ADR-0028 decision 9's per-shard memo, realized at this seam). The
/// `Mutex` is held only for the O(1) take/return of a thread's memo, never across
/// a compute, so worker threads never contend on the hot path; a serial read (the
/// common single-cell / small-cellset case) sees one uncontended entry that
/// amortizes across all its cells. Determinism holds regardless of memo state: a
/// memo only caches computed `(scope, ordinal, coord)` results, so a hit returns
/// exactly what a fresh compute would.
struct CalcCellResolver {
    registry: PinnedRegistry,
    target: u32,
    overlay: Option<OwnedOverlay>,
    /// The caller's element deny mask for the target cube (ADR-0015), or `None`
    /// when no element ACLs apply.
    mask: Option<ElementMask>,
    /// Per-thread memo, reused across the cells one thread computes. Guarded only
    /// for the brief take/return; the compute runs with the memo owned locally.
    memos: Mutex<HashMap<ThreadId, CalcMemo>>,
}

impl CalcCellResolver {
    /// Run `f` with this thread's memo, taking it out under the lock, running the
    /// (unlocked) compute, then returning the grown memo for the next cell. The
    /// memo is a pure cache, so taking a fresh one on the very first call per
    /// thread yields identical results.
    fn with_thread_memo<T>(&self, f: impl FnOnce(CalcMemo) -> (CalcMemo, T)) -> T {
        let tid = std::thread::current().id();
        let memo = self
            .memos
            .lock()
            .expect("calc memo mutex")
            .remove(&tid)
            .unwrap_or_default();
        let (memo, out) = f(memo);
        self.memos
            .lock()
            .expect("calc memo mutex")
            .insert(tid, memo);
        out
    }
}

impl CalcCellResolver {
    /// Build a per-call [`CalcEngine`] over the owned registry, seeded with `memo`,
    /// carrying the sandbox overlay (if any) and the element deny mask for the
    /// target cube. One place builds the engine so the numeric and string reads
    /// enforce the *same* mask through the *same* calc path.
    fn engine_with<'a>(&'a self, memo: CalcMemo) -> CalcEngine<'a> {
        match &self.overlay {
            Some(overlay) => CalcEngine::with_overlay_memo(&self.registry, overlay, memo),
            None => CalcEngine::with_memo(&self.registry, memo),
        }
        .with_mask(self.mask.as_ref(), self.target)
    }
}

impl CellResolver for CalcCellResolver {
    fn value(&self, coord: &[u32]) -> Result<Fixed, QueryError> {
        let result = self.with_thread_memo(|memo| {
            let engine = self.engine_with(memo);
            let result = engine.value(self.target, coord);
            (engine.into_memo(), result)
        });
        Ok(result?)
    }

    fn string_value(&self, coord: &[u32]) -> Result<Option<String>, QueryError> {
        // Delegate to calc's masked `CalcView::string_value` (A3): it enforces the
        // engine's element deny mask at the cell terminal exactly as the numeric
        // path does, so element-security masking of a string cell lives in ONE
        // implementation (calc's), not a duplicate `mask.denies` check here. A
        // denied string cell therefore still returns `AccessDenied` (mapped to 403).
        // String cells carry no rules, so the memo is irrelevant; use a fresh one.
        self.with_thread_memo(|memo| {
            let engine = self.engine_with(memo);
            let result = engine.view(self.target).string_value(coord);
            (engine.into_memo(), result)
        })
    }
}

/// The rule-aware resolver factory injected by the server.
#[derive(Debug)]
pub struct CalcFactory {
    engine: Engine,
}

impl CalcFactory {
    /// Build a factory over the engine's cubes.
    pub fn new(engine: Engine) -> Self {
        Self { engine }
    }
}

impl CellResolverFactory for CalcFactory {
    fn resolver(&self, snapshot: &ReadSnapshot) -> Box<dyn CellResolver + Sync> {
        self.resolver_with(snapshot, None, None)
    }

    fn resolver_with(
        &self,
        snapshot: &ReadSnapshot,
        sandbox: Option<&Sandbox>,
        mask: Option<&ElementMask>,
    ) -> Box<dyn CellResolver + Sync> {
        // The target cube's values MUST come from the caller's pinned snapshot, so
        // a cellset's axes, its reported version, and its cell values are all one
        // MVCC version (ADR-0001); a commit racing this construction cannot poison
        // the version-keyed view cache with values from a newer version.
        let registry = PinnedRegistry::build_pinned(&self.engine, snapshot);
        // `build_pinned` always gives the snapshot's cube a slot, so this resolves
        // (no silent `unwrap_or(0)` reading a different cube's values).
        let target = registry
            .ordinal_of(snapshot.cube().name())
            .expect("build_pinned always includes the target cube");
        // The overlay covers the target cube's leaves only (ADR-0014).
        let overlay = sandbox.map(|sb| OwnedOverlay::new(target, sb));
        Box::new(CalcCellResolver {
            registry,
            target,
            overlay,
            mask: mask.cloned(),
            memos: Mutex::new(HashMap::new()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use epiphany_core::Dimension;
    use epiphany_determinism::IdGen;
    use epiphany_engine::{CellWrite, DimensionEdit};
    use epiphany_persist::Store;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "epiphany-calcfactory-{}-{name}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    /// Region(North,South,Total) x Measure(Sales,Cost,Margin), Sales/Cost populated.
    fn sales_cube() -> Cube {
        let mut region = Dimension::new("Region");
        let n = region.add_leaf("North");
        let s = region.add_leaf("South");
        let t = region.add_consolidated("Total");
        region.add_child(t, n, 1).unwrap();
        region.add_child(t, s, 1).unwrap();
        let mut measure = Dimension::new("Measure");
        measure.add_leaf("Sales");
        measure.add_leaf("Cost");
        measure.add_leaf("Margin");
        let mut cube = Cube::new("Sales", vec![region, measure]).unwrap();
        cube.set_leaf(&[n, 0], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, 1], Fixed::from(60)).unwrap();
        cube.set_leaf(&[s, 0], Fixed::from(200)).unwrap();
        cube.set_leaf(&[s, 1], Fixed::from(150)).unwrap();
        cube
    }

    fn engine_with_sales(dir: &std::path::Path) -> Engine {
        let store = Store::create(dir.join("cubes").join("Sales"), sales_cube()).unwrap();
        let mut stores = BTreeMap::new();
        stores.insert("Sales".to_string(), store);
        Engine::from_stores(stores, Arc::new(IdGen::default()))
    }

    fn coord(engine: &Engine, region: &str, measure: &str) -> Vec<u32> {
        let snap = engine.snapshot("Sales").unwrap();
        vec![
            snap.cube().dimension(0).resolve(region).unwrap(),
            snap.cube().dimension(1).resolve(measure).unwrap(),
        ]
    }

    /// MVCC isolation: a resolver built over a PINNED snapshot must read that
    /// snapshot's values, even if the cube is written after the snapshot is taken.
    /// Pre-fix, `PinnedRegistry::build` re-snapshotted fresh, so the resolver saw
    /// the post-write value while the cellset reported the pinned version -- an
    /// isolation break that poisoned the version-keyed view cache.
    #[test]
    fn target_values_come_from_the_pinned_snapshot_not_a_later_write() {
        let dir = scratch("mvcc");
        let engine = engine_with_sales(&dir);
        let factory = CalcFactory::new(engine.clone());

        // Pin a snapshot at version N.
        let pinned = engine.snapshot("Sales").unwrap();
        let north_sales = coord(&engine, "North", "Sales");

        // Commit a write AFTER pinning: North/Sales 100 -> 999 (a new version N+1).
        engine
            .apply_batch(
                "Sales",
                None,
                &[CellWrite::Leaf {
                    coord: north_sales.clone(),
                    value: Fixed::from(999),
                }],
            )
            .unwrap();

        // A resolver over the PINNED snapshot must still read the pinned 100.
        let resolver = factory.resolver(&pinned);
        assert_eq!(
            resolver.value(&north_sales).unwrap(),
            Fixed::from(100),
            "the pinned snapshot's value, not the later write"
        );
        // The consolidation over the pinned snapshot is likewise pre-write.
        let total_sales = coord(&engine, "Total", "Sales");
        assert_eq!(
            resolver.value(&total_sales).unwrap(),
            Fixed::from(300),
            "Total Sales from the pinned snapshot (100 + 200)"
        );

        // A resolver over a FRESH snapshot sees the new value (sanity: the write
        // did land, so the isolation above is real, not a no-op).
        let fresh = engine.snapshot("Sales").unwrap();
        let fresh_resolver = factory.resolver(&fresh);
        assert_eq!(
            fresh_resolver.value(&north_sales).unwrap(),
            Fixed::from(999)
        );
    }

    /// Fail-loud on a cube whose stored rules stopped compiling after a structural
    /// edit: a read must ERROR, never silently revert every rule-derived cell to
    /// stored/aggregated values. Pre-fix, `PinnedRegistry::build` swallowed the
    /// compile error into an empty model, so the read returned wrong numbers.
    #[test]
    fn a_read_of_a_cube_whose_rules_stopped_compiling_errors() {
        let dir = scratch("ruledrop");
        let engine = engine_with_sales(&dir);
        let factory = CalcFactory::new(engine.clone());

        // A valid rule: Margin = Sales - Cost. Stored verbatim (define does not
        // re-validate, mirroring the engine's define_rules contract).
        engine
            .define_rules(
                "Sales",
                None,
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];"
                    .to_string(),
            )
            .unwrap();

        // Sanity: the rule reads back correctly before the edit (North Margin = 40).
        let margin = coord(&engine, "North", "Margin");
        {
            let snap = engine.snapshot("Sales").unwrap();
            let resolver = factory.resolver(&snap);
            assert_eq!(resolver.value(&margin).unwrap(), Fixed::from(40));
        }

        // Now delete an element the rule references (Cost), with NO rule
        // revalidation -- exactly the hazard: the rule source is retained but no
        // longer compiles (unknown element 'Cost').
        engine
            .edit_dimension(
                "Sales",
                "Measure",
                &DimensionEdit::Delete {
                    element: "Cost".to_string(),
                },
            )
            .unwrap();

        // A read of the (now broken) cube must fail loud, not return a stored/
        // aggregated value. `Margin` coord shifted after the delete, so re-resolve.
        let snap = engine.snapshot("Sales").unwrap();
        let resolver = factory.resolver(&snap);
        let margin = vec![
            snap.cube().dimension(0).resolve("North").unwrap(),
            snap.cube().dimension(1).resolve("Margin").unwrap(),
        ];
        match resolver.value(&margin) {
            Err(QueryError::Calc { message }) => {
                assert!(
                    message.contains("rules fail to compile"),
                    "loud compile-failure error, got: {message}"
                );
                assert!(
                    message.contains("Cost"),
                    "names the broken reference: {message}"
                );
            }
            other => panic!("expected a loud RULE compile error, got {other:?}"),
        }
        // Even a plain stored leaf on the broken cube fails loud (the whole cube's
        // reads are gated, not just the rule-target cell), so no path silently
        // serves rule-less numbers.
        let sales = vec![
            snap.cube().dimension(0).resolve("North").unwrap(),
            snap.cube().dimension(1).resolve("Sales").unwrap(),
        ];
        assert!(matches!(
            resolver.value(&sales),
            Err(QueryError::Calc { .. })
        ));
    }

    /// The resolver reuses ONE memo across the cells of a read (ADR-0007), rather
    /// than building a fresh `CalcEngine` per cell. Verified behaviorally: reading
    /// many cells through one resolver leaves this thread's memo populated (a
    /// fresh-per-cell engine would always leave it empty).
    #[test]
    fn one_resolver_reuses_the_memo_across_cells() {
        let dir = scratch("memo");
        let engine = engine_with_sales(&dir);
        engine
            .define_rules(
                "Sales",
                None,
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];"
                    .to_string(),
            )
            .unwrap();

        // Build the concrete resolver directly (same-crate) so its memo is
        // inspectable; this is exactly what `resolver_with(None, None)` builds.
        let snap = engine.snapshot("Sales").unwrap();
        let registry = PinnedRegistry::build_pinned(&engine, &snap);
        let target = registry.ordinal_of("Sales").unwrap();
        let resolver = CalcCellResolver {
            registry,
            target,
            overlay: None,
            mask: None,
            memos: Mutex::new(HashMap::new()),
        };

        // Read every Margin cell (North, South, Total) through the one resolver.
        for region in ["North", "South", "Total"] {
            let c = coord(&engine, region, "Margin");
            resolver.value(&c).unwrap();
        }
        // One thread did all reads, so its memo retains the computed cells.
        let tid = std::thread::current().id();
        let memos = resolver.memos.lock().unwrap();
        let memo = memos.get(&tid).expect("this thread's memo was retained");
        assert!(
            memo.len() >= 3,
            "the shared memo accumulated computed cells across the read, got {}",
            memo.len()
        );
    }

    /// The completeness gate is wired into `PinnedRegistry`: a cube with a fully
    /// analyzable, complete-feeder rule (Margin = Sales - Cost) is licensed for the
    /// sparse path (`fed_set` is `Some`), and the consolidated read of the
    /// rule-derived rollup returns the correct value — the same value the dense
    /// path yields (40 + 50 = 90). This exercises the real production registry
    /// end to end, not a test stand-in.
    #[test]
    fn a_fully_fed_cube_is_licensed_for_sparse_and_reads_correctly() {
        let dir = scratch("fedwire");
        let engine = engine_with_sales(&dir);
        engine
            .define_rules(
                "Sales",
                None,
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];"
                    .to_string(),
            )
            .unwrap();
        let snap = engine.snapshot("Sales").unwrap();
        let registry = PinnedRegistry::build_pinned(&engine, &snap);
        let target = registry.ordinal_of("Sales").unwrap();
        // The gate licensed the sparse path and inferred both Margin feeders.
        let fed = registry
            .fed_set(target)
            .expect("a complete-feeder cube is licensed for sparse");
        assert_eq!(fed.len(), 2, "North/Margin and South/Margin are fed");

        // The rule-derived rollup reads correctly through the resolver (which now
        // takes the sparse path for this cube on a base read).
        let factory = CalcFactory::new(engine.clone());
        let resolver = factory.resolver(&snap);
        let total_margin = coord(&engine, "Total", "Margin");
        assert_eq!(resolver.value(&total_margin).unwrap(), Fixed::from(90));
        // The leaf margins are correct too.
        assert_eq!(
            resolver.value(&coord(&engine, "North", "Margin")).unwrap(),
            Fixed::from(40)
        );
    }

    /// A cube with NO rules is trivially licensed for sparse (`fed_set` is
    /// `Some(empty)`): sparse degenerates to summing stored cells, which equals the
    /// dense rollup. Total Sales = 100 + 200 = 300 either way.
    #[test]
    fn a_no_rules_cube_is_licensed_for_sparse_with_an_empty_fed_set() {
        let dir = scratch("norules");
        let engine = engine_with_sales(&dir);
        let snap = engine.snapshot("Sales").unwrap();
        let registry = PinnedRegistry::build_pinned(&engine, &snap);
        let target = registry.ordinal_of("Sales").unwrap();
        let fed = registry
            .fed_set(target)
            .expect("a no-rules cube is trivially safe for sparse");
        assert!(fed.is_empty(), "no rules -> empty fed set");

        let factory = CalcFactory::new(engine.clone());
        let resolver = factory.resolver(&snap);
        assert_eq!(
            resolver.value(&coord(&engine, "Total", "Sales")).unwrap(),
            Fixed::from(300)
        );
    }
}
