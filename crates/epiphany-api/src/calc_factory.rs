//! The rule-aware value resolver factory and a pinned multi-cube registry.
//!
//! [`CalcFactory`] implements the engine's `CellResolverFactory` seam by snapshot
//! ting every cube, compiling each cube's rules, and handing back a resolver that
//! overlays rule-derived values (the composition root injects it; tests inject
//! the engine's `StoredCellsFactory` instead). [`PinnedRegistry`] is the
//! eval-time registry the explain and diagnostics endpoints also build.

use std::collections::BTreeMap;

use epiphany_calc::rules::RuleParseError;
use epiphany_calc::{
    compile, rules, CalcEngine, CompileError, CompiledModel, CubeRegistry, EvalRegistry,
    SandboxOverlay,
};
use epiphany_core::{CellResolver, Cube, ElementMask, Fixed, QueryError, Sandbox};
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
pub(crate) struct PinnedRegistry {
    snaps: Vec<ReadSnapshot>,
    models: Vec<CompiledModel>,
    names: Vec<String>,
}

impl PinnedRegistry {
    /// Snapshot every cube and compile its rules once. A cube whose rules fail to
    /// parse/compile (which the API rejects at define time) is treated as
    /// rule-less so reads still work.
    pub(crate) fn build(engine: &Engine) -> Self {
        let names = engine.cube_names();
        let snaps: Vec<ReadSnapshot> = names.iter().filter_map(|n| engine.snapshot(n)).collect();
        let cr = SnapCubes {
            snaps: &snaps,
            names: &names,
        };
        let models = snaps
            .iter()
            .map(|s| {
                rules::parse(&s.rules().source)
                    .ok()
                    .and_then(|doc| compile(s.cube(), &cr, &doc, s.version()).ok())
                    .unwrap_or(CompiledModel {
                        version: s.version(),
                        rules: Vec::new(),
                    })
            })
            .collect();
        Self {
            snaps,
            models,
            names,
        }
    }

    /// The ordinal of a cube by name.
    pub(crate) fn ordinal_of(&self, name: &str) -> Option<u32> {
        self.names.iter().position(|n| n == name).map(|i| i as u32)
    }
}

impl EvalRegistry for PinnedRegistry {
    fn cube(&self, ordinal: u32) -> Option<&Cube> {
        self.snaps.get(ordinal as usize).map(|s| s.cube())
    }
    fn compiled(&self, ordinal: u32) -> Option<&CompiledModel> {
        self.models.get(ordinal as usize)
    }
    fn ordinal(&self, name: &str) -> Option<u32> {
        self.ordinal_of(name)
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
/// multi-cube registry, optionally overlaying a sandbox's what-if leaves. A fresh
/// evaluator (and memo) is used per value read.
struct CalcCellResolver {
    registry: PinnedRegistry,
    target: u32,
    overlay: Option<OwnedOverlay>,
    /// The caller's element deny mask for the target cube (ADR-0015), or `None`
    /// when no element ACLs apply.
    mask: Option<ElementMask>,
}

impl CellResolver for CalcCellResolver {
    fn value(&self, coord: &[u32]) -> Result<Fixed, QueryError> {
        let engine = match &self.overlay {
            Some(overlay) => CalcEngine::with_overlay(&self.registry, overlay),
            None => CalcEngine::new(&self.registry),
        }
        .with_mask(self.mask.as_ref(), self.target);
        Ok(engine.value(self.target, coord)?)
    }

    fn string_value(&self, coord: &[u32]) -> Result<Option<String>, QueryError> {
        let cube = self
            .registry
            .cube(self.target)
            .ok_or_else(|| QueryError::Calc {
                message: "unknown target cube".to_string(),
            })?;
        // String cells carry no rules; enforce the element mask directly.
        if let Some(mask) = &self.mask {
            if mask.denies(cube, coord) {
                return Err(QueryError::AccessDenied);
            }
        }
        Ok(cube.get_string(coord)?.map(str::to_string))
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
    fn resolver(&self, snapshot: &ReadSnapshot) -> Box<dyn CellResolver> {
        self.resolver_with(snapshot, None, None)
    }

    fn resolver_with(
        &self,
        snapshot: &ReadSnapshot,
        sandbox: Option<&Sandbox>,
        mask: Option<&ElementMask>,
    ) -> Box<dyn CellResolver> {
        let registry = PinnedRegistry::build(&self.engine);
        let target = registry.ordinal_of(snapshot.cube().name()).unwrap_or(0);
        // The overlay covers the target cube's leaves only (ADR-0014).
        let overlay = sandbox.map(|sb| OwnedOverlay::new(target, sb));
        Box::new(CalcCellResolver {
            registry,
            target,
            overlay,
            mask: mask.cloned(),
        })
    }
}
