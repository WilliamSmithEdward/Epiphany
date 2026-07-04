//! Sparse feeds and automatic feeder inference (ADR-0005).
//!
//! A feeder marks a rule-derived leaf coordinate as potentially populated, so a
//! consolidation can include it via the sparse union scan
//! ([`epiphany_core::Cube::consolidate_fed`]) instead of enumerating the dense
//! leaf space. [`infer_feeders`] derives feeders for the statically analyzable
//! rule shape (a leaf rule whose value comes from same-cube inputs): feed a
//! target leaf wherever an input leaf it reads is populated. This is a sound
//! over-approximation (it never under-feeds an analyzable rule); rules it cannot
//! analyze are reported so they can be manually fed or diagnosed (Phase 4F).
//! Determinism: the index is a sorted `BTreeSet`.

use std::collections::{BTreeMap, BTreeSet};

use epiphany_core::{Cube, Fixed};

use crate::compiled::{AddrSlot, CCell, CExpr, CompiledArea, CompiledModel, DimPredicate, RuleId};
use crate::eval::{CalcEngine, CalcError, EvalRegistry};
use crate::rules::ArithOp;

/// Cap on the cartesian expansion of one consolidated input. Beyond this the
/// input is conservatively assumed potent (feed the target) rather than fully
/// enumerated, bounding the cost of `input_potent` on a pathological hierarchy.
/// Over-feeding is a warning, never an under-feed, so the cap stays sound.
const INPUT_EXPANSION_CAP: usize = 4096;

/// Approximate bytes a fed cell costs (index slot plus the rule evaluation it
/// enables), used to estimate the waste of over-feeding (ROADMAP section 8).
const FED_CELL_BYTES: usize = 20;

/// A sparse set of fed (rule-derived) leaf coordinates, sorted for determinism.
#[derive(Debug, Clone, Default)]
pub struct FeederIndex {
    coords: BTreeSet<Box<[u32]>>,
}

impl FeederIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a coordinate fed.
    pub fn insert(&mut self, coord: &[u32]) {
        self.coords.insert(coord.to_vec().into_boxed_slice());
    }

    /// Whether a coordinate is fed.
    pub fn contains(&self, coord: &[u32]) -> bool {
        self.coords.contains(coord)
    }

    /// The fed coordinates, sorted, as a slice-friendly vector for
    /// [`epiphany_core::Cube::consolidate_fed`].
    pub fn coords(&self) -> Vec<Box<[u32]>> {
        self.coords.iter().cloned().collect()
    }

    /// The number of fed coordinates.
    pub fn len(&self) -> usize {
        self.coords.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.coords.is_empty()
    }
}

/// A rule whose feeders could not be auto-inferred (it needs manual feeders or
/// only diagnostics), with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueRule {
    /// The rule id.
    pub rule: RuleId,
    /// Why inference could not analyze it.
    pub reason: String,
}

/// The result of feeder inference: the fed set plus the rules it could not
/// analyze.
#[derive(Debug, Clone, Default)]
pub struct FeederInference {
    /// The inferred fed coordinates.
    pub index: FeederIndex,
    /// Rules inference could not analyze (manual feeders / diagnostics needed).
    pub opaque: Vec<OpaqueRule>,
}

/// One analyzable rule: the per-dimension leaf members of its target area (the
/// target leaves are their cartesian product, walked streamingly -- see
/// [`CoordWalk`]), the same-cube input cells whose population drives the feed,
/// and whether the rule has a base contribution (a term that can be non-zero
/// even when every same-cube input is zero, so the whole target area must be
/// fed).
struct Analyzable<'a> {
    target_dims: Vec<Vec<u32>>,
    inputs: Vec<&'a CCell>,
    base_potent: bool,
}

/// Whether `expr` has a *localizable* base contribution: a term that can be
/// non-zero when every *same-cube* cell it reads is zero, so the rule is non-zero
/// across its whole target area regardless of which same-cube inputs are populated
/// (a non-zero constant, an attribute, a cross-cube cell, or such a term inside a
/// conditional branch). Such a rule feeds its entire area. A conservative-but-sound
/// static analysis: it may answer `true` when the real value happens to be zero (a
/// harmless over-feed), but never `false` when a base term can be non-zero.
///
/// A *same-cube* cell is deliberately not a base contribution: it is zeroed by
/// definition where its own leaves are empty, so the fixpoint localizes the feed
/// via [`input_potent`]. A *cross-cube* cell, however, cannot be localized from
/// this cube's data and can be non-zero when every same-cube input is zero, so an
/// additive/branch cross-cube term IS a base contribution (feed the whole area, a
/// sound over-feed). Without this, a mixed rule like
/// `Rev = value['Units'] + 'FX'!['EUR']` would be silently under-fed where Units
/// is empty but the FX scalar is not (ADR-0005 soundness). A *cross-cube-only*
/// rule is still classified opaque by [`infer_feeders`], not fed.
fn base_potent(expr: &CExpr, target_ordinal: u32) -> bool {
    match expr {
        CExpr::Num(n) => *n != Fixed::ZERO,
        // An attribute value can be non-zero on its own.
        CExpr::AttrNum { .. } => true,
        // A cross-cube cell is a non-localizable base term (it may be non-zero
        // regardless of same-cube population); a same-cube cell is not.
        CExpr::Cell(c) => c.cube != target_ordinal,
        CExpr::Undef => false,
        CExpr::Neg(e) => base_potent(e, target_ordinal),
        CExpr::Bin { op, left, right } => match op {
            // A sum/difference is non-zero if either side can be.
            ArithOp::Add | ArithOp::Sub => {
                base_potent(left, target_ordinal) || base_potent(right, target_ordinal)
            }
            // A product is non-zero only if both sides can be.
            ArithOp::Mul => base_potent(left, target_ordinal) && base_potent(right, target_ordinal),
            // A quotient is non-zero only if the numerator can be.
            ArithOp::Div => base_potent(left, target_ordinal),
        },
        // The value is one of the branches; the condition does not contribute a
        // magnitude. A missing else branch is zero.
        CExpr::If {
            cond: _,
            then,
            otherwise,
        } => {
            base_potent(then, target_ordinal)
                || otherwise
                    .as_ref()
                    .is_some_and(|o| base_potent(o, target_ordinal))
        }
    }
}

/// Infer feeders for a cube's compiled rules (ADR-0005).
///
/// For each rule that targets leaves, every target leaf is fed when at least one
/// of the rule's same-cube inputs is *potentially non-zero* at that target: a
/// stored leaf, a leaf fed by another rule, or a consolidated input that rolls up
/// any such leaf. This is computed to a **fixpoint** seeded by the stored leaves,
/// so a rule reading another rule's derived output is fed too (chained rules).
/// It is a sound over-approximation: it never under-feeds an analyzable rule, and
/// at worst over-feeds a target whose inputs turn out to be zero (a warning).
///
/// A rule with a base contribution (a non-zero constant or attribute term, see
/// [`base_potent`]) feeds its whole target area, since it is non-zero everywhere
/// regardless of input population. `target_ordinal` is the cube's own ordinal;
/// cross-cube inputs cannot be localized from this cube's data, so a rule whose
/// only inputs are cross-cube is reported opaque rather than guessed at. A rule
/// whose area selects no leaf coordinates is a pure consolidation override and
/// needs no leaf feeder.
pub fn infer_feeders(cube: &Cube, model: &CompiledModel, target_ordinal: u32) -> FeederInference {
    let mut result = FeederInference::default();

    // Classify rules: skip pure overrides (no leaf targets), report opaque rules
    // (no same-cube input to localize), and keep the rest as analyzable.
    let mut analyzable: Vec<Analyzable> = Vec::new();
    for (i, rule) in model.rules.iter().enumerate() {
        let target_dims = area_leaf_dims(cube, &rule.area);
        if target_dims.iter().any(|leaves| leaves.is_empty()) {
            continue; // pure consolidation override: value computed at the coord
        }
        let mut cells = Vec::new();
        collect_cells(&rule.expr, &mut cells);
        let inputs: Vec<&CCell> = cells
            .iter()
            .copied()
            .filter(|c| c.cube == target_ordinal)
            .collect();
        let bp = base_potent(&rule.expr, target_ordinal);
        if inputs.is_empty() {
            // No same-cube input to localize the feed.
            let has_cross_cube = cells.iter().any(|c| c.cube != target_ordinal);
            if has_cross_cube {
                // A cross-cube-only rule cannot be localized from this cube's data.
                // Report it rather than guess (ADR-0005 decision 3), even if a
                // constant/attribute term also makes it base-potent -- the operator
                // must decide the feed for the cross-cube part.
                result.opaque.push(OpaqueRule {
                    rule: RuleId(i),
                    reason: "only cross-cube inputs; feeders cannot be localized".to_string(),
                });
            } else if bp {
                // A pure constant/attribute base term (no cells): non-zero across
                // the whole area, so feed all of it.
                analyzable.push(Analyzable {
                    target_dims,
                    inputs,
                    base_potent: true,
                });
            }
            // Otherwise no cells and no base term: identically zero, nothing to do.
        } else {
            // Analyzable via same-cube inputs. `base_potent` is true when a base
            // term (a constant/attribute) OR an additive/branch cross-cube term can
            // be non-zero where every same-cube input is zero, in which case the
            // whole area is fed (a sound over-feed); otherwise the fixpoint
            // localizes the feed to targets whose same-cube input is potent.
            analyzable.push(Analyzable {
                target_dims,
                inputs,
                base_potent: bp,
            });
        }
    }

    // An opaque rule feeds nothing itself (its feed must be decided manually),
    // but its targets can be non-zero -- that is exactly why it is reported. A
    // downstream rule reading an opaque-covered coordinate must therefore still
    // be fed (ADR-0005 soundness: never under-feed an analyzable rule), so the
    // fixpoint treats coordinates any opaque rule targets as potent. The refs
    // point into `model`, so they never alias the growing `result`.
    let opaque_areas: Vec<&CompiledArea> = result
        .opaque
        .iter()
        .map(|o| &model.rules[o.rule.0].area)
        .collect();

    // Fixpoint: a leaf is "potent" (potentially non-zero) if it is stored, has
    // been fed, or is targeted by an opaque rule. Feed a target whose input is
    // potent; the newly fed target is then itself potent, so a later iteration
    // can feed a rule that reads it. Each round only adds feeders, and there are
    // finitely many target leaves, so this terminates. Targets are walked
    // streamingly (never materialized), in a fixed order (determinism).
    let mut potent: BTreeSet<Vec<u32>> = cube.cell_entries().map(|(coord, _)| coord).collect();
    let mut target = Vec::new();
    loop {
        let mut changed = false;
        for a in &analyzable {
            let mut walk = CoordWalk::new(&a.target_dims);
            while walk.next_coord(&mut target) {
                if result.index.contains(&target) {
                    continue;
                }
                // A base-potent rule is non-zero across its whole area, so feed
                // every target; otherwise feed where a same-cube input is potent.
                let feed = a.base_potent
                    || a.inputs.iter().any(|cell| {
                        input_potent(cube, model, &opaque_areas, cell, &target, &potent)
                    });
                if feed {
                    result.index.insert(&target);
                    potent.insert(target.clone());
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    result
}

/// Feeder validation diagnostics (Phase 4F).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeederDiagnostics {
    /// Rule-target leaves with a non-zero rule value that are NOT fed: a silent
    /// wrong-zero in rollups. This is the hard error condition.
    pub under_fed: Vec<Vec<u32>>,
    /// Fed coordinates whose rule value is zero: wasted scan/RAM (a warning).
    pub over_fed: Vec<Vec<u32>>,
    /// Coordinates whose rule value could not be evaluated (e.g. `DivByZero`,
    /// `Cycle`), with the error message. Their feed status is indeterminate, so
    /// they are reported here rather than aborting the whole validation: a single
    /// erroring rule no longer withholds the under/over-feed report for every
    /// healthy rule (sorted by coordinate for determinism).
    pub erroring: Vec<(Vec<u32>, String)>,
    /// The number of fed coordinates.
    pub fed_cell_count: usize,
    /// An estimate of the RAM/scan cost of the over-fed cells.
    pub estimated_over_fed_bytes: usize,
}

impl FeederDiagnostics {
    /// Whether the model is correctly fed (no under-feed). Over-feed is a warning,
    /// not a correctness failure; an evaluation error is a separate concern
    /// reported in [`erroring`](Self::erroring), not an under-feed.
    pub fn is_clean(&self) -> bool {
        self.under_fed.is_empty()
    }
}

/// Validate a feeder index against the true (densely-evaluated) rule values for a
/// cube, reporting under-feed (an error) and over-feed (a warning). This is an
/// explicit on-demand operation, never on the read path.
///
/// Determinism: candidate target leaves and fed coordinates are checked in sorted
/// order, so the lists are byte-identical run to run.
pub fn validate_feeders(
    registry: &dyn EvalRegistry,
    ordinal: u32,
    index: &FeederIndex,
) -> Result<FeederDiagnostics, CalcError> {
    let cube = registry
        .cube(ordinal)
        .ok_or(CalcError::UnknownCube(ordinal))?;
    let model = match registry.compiled(ordinal) {
        Some(m) => m,
        None => return Ok(FeederDiagnostics::default()),
    };
    let engine = CalcEngine::new(registry);

    // A target that fails to evaluate (DivByZero, Cycle, ...) has an indeterminate
    // feed status: record it and keep going, so one erroring rule does not withhold
    // the whole under/over-feed report. Keyed by coordinate (deterministic order).
    let mut erroring: BTreeMap<Vec<u32>, String> = BTreeMap::new();

    // Under-feed: every leaf a rule targets with a non-zero value must be fed.
    // `area_leaf_dims` admits only leaf targets, so a pure consolidation
    // override (no leaf targets) contributes none and a mixed-target rule still
    // has its leaf targets checked (no override skip can hide an under-feed).
    // The target area is walked streamingly ([`CoordWalk`]), never materialized:
    // a broad area over large dimensions selects astronomically more coordinates
    // than fit in memory, and this loop needs only one at a time.
    let mut under = BTreeSet::new();
    let mut target = Vec::new();
    for rule in &model.rules {
        let target_dims = area_leaf_dims(cube, &rule.area);
        let mut walk = CoordWalk::new(&target_dims);
        while walk.next_coord(&mut target) {
            match engine.value(ordinal, &target) {
                Ok(v) => {
                    if v != Fixed::ZERO && !index.contains(&target) {
                        under.insert(target.clone());
                    }
                }
                Err(e) => {
                    if !erroring.contains_key(&target) {
                        erroring.insert(target.clone(), e.to_string());
                    }
                }
            }
        }
    }

    // Over-feed: a fed coordinate whose rule value is zero is wasted.
    let mut over = BTreeSet::new();
    for fed in index.coords() {
        match engine.value(ordinal, &fed) {
            Ok(v) => {
                if v == Fixed::ZERO {
                    over.insert(fed.to_vec());
                }
            }
            Err(e) => {
                erroring
                    .entry(fed.to_vec())
                    .or_insert_with(|| e.to_string());
            }
        }
    }

    Ok(FeederDiagnostics {
        under_fed: under.into_iter().collect(),
        estimated_over_fed_bytes: over.len() * FED_CELL_BYTES,
        over_fed: over.into_iter().collect(),
        erroring: erroring.into_iter().collect(),
        fed_cell_count: index.len(),
    })
}

/// The result of the sparse-consolidation completeness gate for one cube: either
/// the proven-complete fed leaf set (safe to route consolidated reads through the
/// sparse [`epiphany_core::Cube::consolidate_fed`]) or the reason the cube must
/// stay on the dense [`epiphany_core::Cube::consolidate_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FedGate {
    /// Sparse consolidation is provably byte-identical to dense for this cube. The
    /// fed coordinates are sorted (deterministic) and complete: every non-stored
    /// leaf whose rule value is non-zero (or that errors) is present.
    Safe(Vec<Box<[u32]>>),
    /// The cube must use the dense path. Carries a short machine-oriented reason
    /// (for diagnostics/logging), one of: `"no-model"`, `"opaque-rules"`,
    /// `"under-fed"`, `"erroring"`.
    Dense(&'static str),
}

/// Decide whether a cube's consolidated reads may use the sparse union scan
/// ([`epiphany_core::Cube::consolidate_fed`]) instead of the dense cartesian
/// enumeration ([`epiphany_core::Cube::consolidate_with`]), and if so return the
/// complete fed leaf set to pass to it (ADR-0005).
///
/// This is the completeness GATE. Sparse is byte-identical to dense for a base
/// read (no what-if overlay, no element mask — the caller enforces those) exactly
/// when the fed set covers every leaf that could contribute a non-zero (or
/// erroring) value yet is not in the stored cell store. The gate proves that by
/// combining [`infer_feeders`] with [`validate_feeders`] and admits the sparse
/// path ONLY when ALL of the following hold; any doubt yields [`FedGate::Dense`]:
///
/// * **No opaque rule.** Feeder inference could localize every rule in scope. An
///   opaque rule (e.g. one driven only by a cross-cube scalar) can be non-zero at
///   leaves inference cannot enumerate, so its targets might be unfed — unsafe.
/// * **No under-fed leaf.** `validate_feeders` (dense-truth comparison) finds no
///   rule-target leaf with a non-zero value that is missing from the fed set. This
///   is the direct statement of completeness: a non-stored leaf can be non-zero
///   only if a rule targets it, and every such rule's leaf targets are validated.
/// * **No erroring leaf.** No rule-target leaf evaluates to an error (DivByZero,
///   Cycle, Overflow). An erroring contributing leaf makes the dense rollup error;
///   if it were unfed the sparse scan would skip it and NOT error, a divergence in
///   the `Ok`/`Err` outcome. Requiring zero erroring leaves keeps the two paths'
///   error behavior identical too. (Over-feeding is fine: an over-fed leaf whose
///   value is zero contributes zero to both sums.)
///
/// A cube with **no compiled rules** returns `Safe(empty)`: the fed set is empty,
/// so sparse degenerates to "sum the stored cells", which is exactly what dense
/// does — trivially identical, and the common case. A cube with **no compiled
/// model at all** (its rules failed to compile) returns `Dense("no-model")`; such
/// a cube already fails loud on read, and the dense path preserves that.
///
/// Runs once per registry build (not per cell): it evaluates the dense truth for
/// every rule-target leaf, which is the same work `validate_feeders` does. The
/// returned fed set is sorted, so the sparse scan and thus the result are
/// deterministic.
pub fn safe_fed_set(registry: &dyn EvalRegistry, ordinal: u32) -> Result<FedGate, CalcError> {
    let cube = registry
        .cube(ordinal)
        .ok_or(CalcError::UnknownCube(ordinal))?;
    // A cube whose rules failed to compile has no model here; it fails loud on
    // read, so keep it on the dense path (which preserves that behavior).
    let model = match registry.compiled(ordinal) {
        Some(m) => m,
        None => return Ok(FedGate::Dense("no-model")),
    };
    // A cube with no rules: the fed set is empty and sparse == "sum stored cells"
    // == dense. Skip inference/validation entirely (the hot common case).
    if model.rules.is_empty() {
        return Ok(FedGate::Safe(Vec::new()));
    }

    let inference = infer_feeders(cube, model, ordinal);
    // An opaque rule cannot be localized, so its targets may be unfed: unsafe.
    if !inference.opaque.is_empty() {
        return Ok(FedGate::Dense("opaque-rules"));
    }

    // Dense-truth validation: the fed set must cover every non-zero rule-target
    // leaf (no under-feed) and no rule-target leaf may error (else the dense
    // rollup would error where the sparse scan would not).
    let diag = validate_feeders(registry, ordinal, &inference.index)?;
    if !diag.under_fed.is_empty() {
        return Ok(FedGate::Dense("under-fed"));
    }
    if !diag.erroring.is_empty() {
        return Ok(FedGate::Dense("erroring"));
    }

    // Proven complete: hand back the sorted fed coordinates.
    Ok(FedGate::Safe(inference.index.coords()))
}

/// The per-dimension LEAF members a rule's area admits, each list in ascending
/// index order. The area's leaf coordinates are the cartesian product of these
/// lists, walked streamingly by [`CoordWalk`]; an empty list in any dimension
/// means the area selects no leaf coordinate (a pure consolidation override).
fn area_leaf_dims(cube: &Cube, area: &CompiledArea) -> Vec<Vec<u32>> {
    let mut per_dim: Vec<Vec<u32>> = Vec::with_capacity(cube.rank());
    for d in 0..cube.rank() {
        let is_leaf = |i: u32| {
            cube.dimension(d)
                .element(i)
                .map(|e| e.kind.is_leaf())
                .unwrap_or(false)
        };
        let leaves: Vec<u32> = match &area.per_dim[d] {
            DimPredicate::Any => (0..cube.dimension(d).len())
                .filter(|&i| is_leaf(i))
                .collect(),
            DimPredicate::OneOf(set) => set.iter().copied().filter(|&i| is_leaf(i)).collect(),
        };
        per_dim.push(leaves);
    }
    per_dim
}

/// A streaming walk of the cartesian product of per-dimension element lists, in
/// lexicographic coordinate order (given ascending lists). It holds only the
/// per-dimension positions -- never the materialized product, which for a broad
/// rule area or a top-level consolidation is astronomically larger than the sum
/// of the lists (the dense-expansion blow-up ADR-0005 exists to avoid), and it
/// never computes the product size, so no `usize` product can overflow.
/// `next_coord` writes into a caller-owned buffer: a full walk allocates nothing
/// per step.
pub(crate) struct CoordWalk<'a> {
    per_dim: &'a [Vec<u32>],
    /// Per-dimension positions of the next coordinate; `None` once exhausted
    /// (immediately so when some dimension has no elements: an empty product).
    pos: Option<Vec<usize>>,
}

impl<'a> CoordWalk<'a> {
    pub(crate) fn new(per_dim: &'a [Vec<u32>]) -> Self {
        let pos = if per_dim.iter().any(|v| v.is_empty()) {
            None
        } else {
            Some(vec![0usize; per_dim.len()])
        };
        Self { per_dim, pos }
    }

    /// Write the next coordinate into `out` and return `true`, or return `false`
    /// when the walk is exhausted. Deterministic: coordinates come out in a
    /// fixed order (lexicographic for ascending per-dimension lists).
    pub(crate) fn next_coord(&mut self, out: &mut Vec<u32>) -> bool {
        let pos = match self.pos.as_mut() {
            Some(p) => p,
            None => return false,
        };
        out.clear();
        out.extend(pos.iter().zip(self.per_dim).map(|(&p, dim)| dim[p]));
        // Advance like an odometer, last dimension fastest.
        let mut d = self.per_dim.len();
        loop {
            if d == 0 {
                self.pos = None; // every dimension rolled over: done
                break;
            }
            d -= 1;
            pos[d] += 1;
            if pos[d] < self.per_dim[d].len() {
                break;
            }
            pos[d] = 0;
        }
        true
    }
}

fn collect_cells<'a>(expr: &'a CExpr, out: &mut Vec<&'a CCell>) {
    match expr {
        CExpr::Cell(c) => out.push(c),
        CExpr::Neg(e) => collect_cells(e, out),
        CExpr::Bin { left, right, .. } => {
            collect_cells(left, out);
            collect_cells(right, out);
        }
        CExpr::If {
            cond: _,
            then,
            otherwise,
        } => {
            collect_cells(then, out);
            if let Some(o) = otherwise {
                collect_cells(o, out);
            }
        }
        CExpr::Num(_) | CExpr::AttrNum { .. } | CExpr::Undef => {}
    }
}

/// Whether the input addressed by `cell` for target leaf `target` is potentially
/// non-zero: at least one leaf it resolves to is in `potent`. Each dimension
/// resolves to a leaf set: a copied (`FromTarget`) dim to the target's leaf, a
/// pinned leaf to itself, and a pinned consolidated element to the leaves it rolls
/// up (so a rule reading a consolidated input is fed when any contributing leaf
/// is potent). An input that resolves to no leaves (an empty consolidation) is
/// not potent, which is correct: its value is zero.
///
/// A consolidation-override rule (e.g. `['Region':'Total','Measure':'Sales'] =
/// 1000`) populates a consolidated cell *directly*, not via any leaf, so its value
/// is invisible to the leaf-expansion above: a downstream rule reading that
/// overridden consolidated cell would be judged not potent and silently under-fed
/// (ADR-0005 soundness). To close that, the input's own resolved element coordinate
/// is checked against `model`: if a rule targets it (an override, or any rule that
/// could be non-zero there), the input is potent. Over-feeding when that rule turns
/// out zero is a sound warning, never an under-feed.
///
/// `opaque_areas` are the target areas of the rules inference reported opaque
/// (e.g. cross-cube-only). Such a rule feeds nothing into `potent` itself, yet
/// its targets can be non-zero -- that is exactly why it is reported -- so a
/// leaf covered by an opaque rule counts as potent here. Without this, a rule
/// downstream of an opaque rule (`Net = Margin` where Margin is cross-cube
/// driven) would be silently under-fed AND not reported: the chained
/// manifestation of the cross-cube gap.
fn input_potent(
    cube: &Cube,
    model: &CompiledModel,
    opaque_areas: &[&CompiledArea],
    cell: &CCell,
    target: &[u32],
    potent: &BTreeSet<Vec<u32>>,
) -> bool {
    if cell.addr.len() != target.len() {
        return false;
    }
    let mut element_coord: Vec<u32> = Vec::with_capacity(cell.addr.len());
    let mut per_dim: Vec<Vec<u32>> = Vec::with_capacity(cell.addr.len());
    for (d, slot) in cell.addr.iter().enumerate() {
        let element = match slot {
            AddrSlot::Pinned(pin) => *pin,
            // `cell.addr` is in the referenced cube's dimension order; for a
            // same-cube reference (the only kind that uses `FromTarget`) that
            // matches the target's order, so the copied member is `target[d]`.
            AddrSlot::FromTarget(_) => target[d],
        };
        element_coord.push(element);
        // Expand to the contributing leaves under `element` (a leaf yields itself;
        // a consolidation yields its non-zero-weight leaves, deterministically).
        let leaves: Vec<u32> = match cube.dimension(d).leaf_weights(element) {
            Ok(lw) => lw.into_iter().map(|(leaf, _)| leaf).collect(),
            Err(_) => return false,
        };
        if leaves.is_empty() {
            return false;
        }
        per_dim.push(leaves);
    }
    // A *consolidation-override* rule targeting the input's own (consolidated)
    // coordinate populates it directly, so its value is invisible to the leaf
    // expansion below. Detect that: if the input coordinate is consolidated (not
    // all-leaf) and a rule targets it, the input can be non-zero regardless of leaf
    // population -- treat it as potent (a sound over-feed). A *leaf* input needs no
    // such check: its own rule feeds it into `potent` via the fixpoint, so gating
    // on "consolidated" keeps leaf-driven feeds tight.
    let input_is_consolidated = element_coord.iter().enumerate().any(|(d, &idx)| {
        cube.dimension(d)
            .element(idx)
            .map(|e| !e.kind.is_leaf())
            .unwrap_or(false)
    });
    if input_is_consolidated && model.matching_rule(cube, &element_coord).is_some() {
        return true;
    }
    // Bound the work on a pathological hierarchy: a huge consolidated input is
    // conservatively treated as potent (a sound over-feed) rather than enumerated.
    // A *checked* product matters here: the release profile has no overflow-checks,
    // so an unchecked `usize` product over many consolidated dimensions can wrap
    // (e.g. 256^8 == 2^64 wraps to 0), which would slip past the cap and make
    // `cartesian_any(total = 0)` report the input NOT potent -- a silent under-feed,
    // the exact inversion of the cap's sound over-feed direction. On overflow we
    // treat the input as exceeding the cap (potent).
    match checked_product(&per_dim) {
        Some(total) if total <= INPUT_EXPANSION_CAP => cartesian_any(&per_dim, |coord| {
            potent.contains(coord) || opaque_areas.iter().any(|a| a.matches(cube, coord))
        }),
        // Over the cap or the product overflowed `usize`: conservatively potent.
        _ => true,
    }
}

/// The product of the per-dimension lengths, or `None` if it overflows `usize`.
/// Short-circuits on the first zero (an empty dimension yields an empty product).
fn checked_product(per_dim: &[Vec<u32>]) -> Option<usize> {
    let mut total: usize = 1;
    for dim in per_dim {
        total = total.checked_mul(dim.len())?;
    }
    Some(total)
}

/// Whether any coordinate in the cartesian product of `per_dim` satisfies `pred`.
/// Deterministic (it walks the product in index order) and bounded by the product
/// size, which is small in practice (most dimensions resolve to a single leaf).
fn cartesian_any(per_dim: &[Vec<u32>], mut pred: impl FnMut(&[u32]) -> bool) -> bool {
    let total: usize = per_dim.iter().map(|v| v.len()).product();
    if total == 0 {
        return false;
    }
    let mut coord = vec![0u32; per_dim.len()];
    for n in 0..total {
        let mut rem = n;
        for (d, dim) in per_dim.iter().enumerate() {
            coord[d] = dim[rem % dim.len()];
            rem /= dim.len();
        }
        if pred(&coord) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::eval::{CalcEngine, EvalRegistry};
    use crate::registry::SingleCube;
    use crate::rules::parse;
    use epiphany_core::{Cube, Dimension, Fixed};

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
        Cube::new("Sales", vec![region, measure]).unwrap()
    }

    struct OneCube {
        cube: Cube,
        model: CompiledModel,
    }
    impl EvalRegistry for OneCube {
        fn cube(&self, o: u32) -> Option<&Cube> {
            (o == 0).then_some(&self.cube)
        }
        fn compiled(&self, o: u32) -> Option<&CompiledModel> {
            (o == 0).then_some(&self.model)
        }
        fn ordinal(&self, name: &str) -> Option<u32> {
            (name == self.cube.name()).then_some(0)
        }
    }

    #[test]
    fn infers_feeders_for_a_leaf_rule() {
        let mut cube = sales_cube();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];")
                .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        assert!(inf.opaque.is_empty(), "the rule is analyzable");
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        // North and South have a populated input -> both Margin leaves fed.
        assert!(inf.index.contains(&[n, margin]));
        assert!(inf.index.contains(&[s, margin]));
        assert_eq!(inf.index.len(), 2);
    }

    #[test]
    fn sparse_fed_consolidation_equals_dense() {
        // With complete inferred feeders, the sparse union scan equals the dense
        // consolidate_with for the rule-derived rollup (no under-feed).
        let mut cube = sales_cube();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
        cube.set_leaf(&[s, cost], Fixed::from(150)).unwrap();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];")
                .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let reg = OneCube { cube, model };
        let engine = CalcEngine::new(&reg);
        let total = reg.cube.dimension(0).resolve("Total").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let coord = [total, margin];
        // Dense (the always-correct path the evaluator uses).
        let dense = engine.value(0, &coord).unwrap();
        // Sparse union scan over the inferred feeders.
        let fed = inf.index.coords();
        let sparse = reg
            .cube
            .consolidate_fed::<epiphany_core::QueryError, _>(&coord, &fed, |lc| {
                Ok(engine.value(0, lc)?)
            })
            .unwrap();
        assert_eq!(sparse, dense);
        assert_eq!(dense, Fixed::from(90));
    }

    #[test]
    fn constant_rule_is_base_potent_and_feeds_its_whole_area() {
        let cube = sales_cube();
        // A non-zero constant is non-zero across the whole target area, so every
        // target leaf is fed (and the rule is not opaque: it is fully analyzed).
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = 5;").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        assert!(inf.opaque.is_empty());
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        assert!(inf.index.contains(&[n, margin]));
        assert!(inf.index.contains(&[s, margin]));
    }

    #[test]
    fn additive_constant_feeds_targets_with_empty_inputs() {
        // Margin = Sales + 5 is non-zero even where Sales is empty (it is 5), so
        // both regions must be fed -- the base-potent case the per-input fixpoint
        // alone would under-feed.
        let mut cube = sales_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let sales = cube.dimension(1).resolve("Sales").unwrap();
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = value['Measure':'Sales'] + 5;").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        let s = cube.dimension(0).resolve("South").unwrap();
        assert!(inf.index.contains(&[n, margin]));
        assert!(
            inf.index.contains(&[s, margin]),
            "South is fed even though its Sales input is empty (Margin = 5 there)"
        );
    }

    #[test]
    fn conditional_with_constant_branch_is_base_potent() {
        // The else branch is a non-zero constant, so the rule can be non-zero
        // anywhere: every target is fed regardless of the input population.
        let mut cube = sales_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let sales = cube.dimension(1).resolve("Sales").unwrap();
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Measure':'Margin'] = if value['Measure':'Sales'] > 100 \
                 then value['Measure':'Sales'] else 50;",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let margin = cube.dimension(1).resolve("Margin").unwrap();
        let s = cube.dimension(0).resolve("South").unwrap();
        assert!(inf.index.contains(&[n, margin]));
        assert!(inf.index.contains(&[s, margin]));
    }

    /// Build the Margin model populated for the given regions, returning the
    /// registry and inferred feeders.
    fn margin_model(populate_south: bool) -> (OneCube, FeederInference) {
        let mut cube = sales_cube();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        if populate_south {
            cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
            cube.set_leaf(&[s, cost], Fixed::from(150)).unwrap();
        }
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];")
                .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        (OneCube { cube, model }, inf)
    }

    #[test]
    fn validate_clean_model_has_no_under_or_over_feed() {
        let (reg, inf) = margin_model(true);
        let diag = validate_feeders(&reg, 0, &inf.index).unwrap();
        assert!(diag.is_clean(), "no under-feed");
        assert!(diag.over_fed.is_empty(), "no over-feed");
        assert_eq!(diag.fed_cell_count, 2);
    }

    #[test]
    fn missing_feeders_are_reported_under_fed() {
        let (reg, _inf) = margin_model(true);
        // An empty index under-feeds both non-zero Margin leaves.
        let diag = validate_feeders(&reg, 0, &FeederIndex::new()).unwrap();
        assert!(!diag.is_clean());
        assert_eq!(diag.under_fed.len(), 2);
    }

    #[test]
    fn fed_but_zero_is_reported_over_fed() {
        // South unpopulated: its Margin is zero, so feeding it is over-feed.
        let (reg, inf) = margin_model(false);
        let s = reg.cube.dimension(0).resolve("South").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let mut idx = inf.index.clone();
        idx.insert(&[s, margin]);
        let diag = validate_feeders(&reg, 0, &idx).unwrap();
        assert_eq!(diag.over_fed, vec![vec![s, margin]]);
        assert!(diag.estimated_over_fed_bytes > 0);
        assert!(diag.is_clean(), "over-feed is not an under-feed");
    }

    #[test]
    fn consolidation_override_needs_no_feeder() {
        let cube = sales_cube();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Region':'Total', 'Measure':'Sales'] = 1000;").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        assert!(inf.index.is_empty());
        assert!(
            inf.opaque.is_empty(),
            "an override is not opaque, just feeder-less"
        );
    }

    #[test]
    fn checked_product_reports_overflow_instead_of_wrapping() {
        // A modest product is exact.
        assert_eq!(checked_product(&[vec![0; 4], vec![0; 8]]), Some(32));
        // An empty dimension yields a zero-size product (short-circuits).
        assert_eq!(
            checked_product(&[vec![0; 4], Vec::new(), vec![0; 8]]),
            Some(0)
        );
        // A product that would exceed `usize` reports `None` rather than wrapping
        // (e.g. to 0), which is what defeats the INPUT_EXPANSION_CAP soundness
        // guard in the release profile (no overflow-checks).
        let huge = vec![0u32; 1 << 21]; // 2^21 per dim
                                        // (2^21)^4 == 2^84 > usize::MAX on 64-bit: must be None, never 0.
        assert_eq!(
            checked_product(&[huge.clone(), huge.clone(), huge.clone(), huge]),
            None
        );
    }

    #[test]
    fn coord_walk_streams_the_full_product_in_lexicographic_order() {
        let dims = vec![vec![1u32, 3], vec![10u32, 20, 30]];
        let mut walk = CoordWalk::new(&dims);
        let mut c = Vec::new();
        let mut seen = Vec::new();
        while walk.next_coord(&mut c) {
            seen.push(c.clone());
        }
        assert_eq!(
            seen,
            vec![
                vec![1, 10],
                vec![1, 20],
                vec![1, 30],
                vec![3, 10],
                vec![3, 20],
                vec![3, 30],
            ]
        );
        // An empty dimension empties the whole product.
        let empty_dims = [vec![1u32], Vec::new()];
        let mut empty_walk = CoordWalk::new(&empty_dims);
        assert!(!empty_walk.next_coord(&mut c));
        // A zero-dimension product has exactly one (empty) coordinate.
        let mut unit_walk = CoordWalk::new(&[]);
        assert!(unit_walk.next_coord(&mut c));
        assert!(c.is_empty());
        assert!(!unit_walk.next_coord(&mut c));
    }

    #[test]
    fn mixed_same_and_cross_cube_additive_rule_feeds_whole_area() {
        // Rev = Units + FX!EUR mixes a same-cube input (Units) with an additive
        // cross-cube scalar. Where Units is empty the value is still the (non-zero)
        // FX scalar, so every target must be fed -- the additive cross-cube term is
        // a base contribution. Feeding the whole area is sound; under-feeding it
        // (the pre-fix behavior) is a silent wrong-zero (ADR-0005 soundness).
        let mut sales = {
            let mut region = Dimension::new("Region");
            let _n = region.add_leaf("North");
            let _s = region.add_leaf("South");
            let mut measure = Dimension::new("Measure");
            measure.add_leaf("Units");
            measure.add_leaf("Rev");
            Cube::new("Sales", vec![region, measure]).unwrap()
        };
        let north = sales.dimension(0).resolve("North").unwrap();
        let units = sales.dimension(1).resolve("Units").unwrap();
        // Only North/Units is populated; South has no same-cube input.
        sales.set_leaf(&[north, units], Fixed::from(10)).unwrap();

        let mut pair = Dimension::new("Pair");
        pair.add_leaf("EUR");
        let mut fx = Cube::new("FX", vec![pair]).unwrap();
        let eur = fx.dimension(0).resolve("EUR").unwrap();
        fx.set_leaf(&[eur], Fixed::from(5)).unwrap();

        let reg_for_compile = crate::registry::VecRegistry::new(vec![sales.clone(), fx.clone()]);
        let model = compile(
            &sales,
            &reg_for_compile,
            &parse("['Measure':'Rev'] = value['Measure':'Units'] + 'FX'!['Pair':'EUR'];").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&sales, &model, 0);
        // A mixed rule is analyzable (it has a same-cube input), not opaque.
        assert!(
            inf.opaque.is_empty(),
            "a mixed rule is analyzable, not opaque"
        );
        let rev = sales.dimension(1).resolve("Rev").unwrap();
        let s = sales.dimension(0).resolve("South").unwrap();
        assert!(inf.index.contains(&[north, rev]), "North fed");
        assert!(
            inf.index.contains(&[s, rev]),
            "South is fed too: Rev = 0 + FX(5) != 0 even with no Units there"
        );
    }

    #[test]
    fn cross_cube_only_rule_is_reported_opaque() {
        // A rule driven only by a cross-cube scalar has no same-cube input to
        // localize, so it is reported opaque, not guessed (ADR-0005 decision 3) --
        // even though the additive/constant analysis would call it base-potent.
        let sales = {
            let region = Dimension::new("Region");
            let mut measure = Dimension::new("Measure");
            measure.add_leaf("Rev");
            let mut r = region;
            r.add_leaf("North");
            Cube::new("Sales", vec![r, measure]).unwrap()
        };
        let fx = Cube::new(
            "FX",
            vec![{
                let mut pair = Dimension::new("Pair");
                pair.add_leaf("EUR");
                pair
            }],
        )
        .unwrap();
        let reg_for_compile = crate::registry::VecRegistry::new(vec![sales.clone(), fx.clone()]);
        let model = compile(
            &sales,
            &reg_for_compile,
            &parse("['Measure':'Rev'] = 'FX'!['Pair':'EUR'];").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&sales, &model, 0);
        assert_eq!(inf.opaque.len(), 1, "cross-cube-only rule is opaque");
        assert!(inf.index.is_empty(), "and nothing is guessed as fed");
    }

    #[test]
    fn rule_downstream_of_an_opaque_rule_is_fed() {
        // Margin is driven only by a cross-cube scalar (opaque: its own feed
        // cannot be localized and must be decided manually), and Net reads
        // Margin. Margin's true value is the non-zero FX scalar everywhere, so
        // every Net leaf must be fed even though the opaque rule feeds nothing
        // into `potent`. Pre-fix, Net was silently under-fed AND not reported
        // opaque -- the chained manifestation of the cross-cube gap.
        let sales = {
            let mut region = Dimension::new("Region");
            region.add_leaf("North");
            region.add_leaf("South");
            let mut measure = Dimension::new("Measure");
            measure.add_leaf("Margin");
            measure.add_leaf("Net");
            Cube::new("Sales", vec![region, measure]).unwrap()
        };
        let mut fx = Cube::new(
            "FX",
            vec![{
                let mut pair = Dimension::new("Pair");
                pair.add_leaf("EUR");
                pair
            }],
        )
        .unwrap();
        let eur = fx.dimension(0).resolve("EUR").unwrap();
        fx.set_leaf(&[eur], Fixed::from(5)).unwrap();

        let reg_for_compile = crate::registry::VecRegistry::new(vec![sales.clone(), fx.clone()]);
        let model = compile(
            &sales,
            &reg_for_compile,
            &parse(
                "['Measure':'Margin'] = 'FX'!['Pair':'EUR'];\n\
                 ['Measure':'Net'] = value['Measure':'Margin'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&sales, &model, 0);
        assert_eq!(inf.opaque.len(), 1, "the Margin rule is still reported");
        let (n, s) = (
            sales.dimension(0).resolve("North").unwrap(),
            sales.dimension(0).resolve("South").unwrap(),
        );
        let (margin, net) = (
            sales.dimension(1).resolve("Margin").unwrap(),
            sales.dimension(1).resolve("Net").unwrap(),
        );
        assert!(
            inf.index.contains(&[n, net]) && inf.index.contains(&[s, net]),
            "Net is fed everywhere its opaque-covered input can be non-zero"
        );
        assert!(
            !inf.index.contains(&[n, margin]) && !inf.index.contains(&[s, margin]),
            "the opaque rule's own targets stay unfed (manual feed, as reported)"
        );
    }

    /// Like `sales_cube` but with an extra leaf measure `Net`, for chained rules.
    fn chain_cube() -> Cube {
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
        measure.add_leaf("Net");
        Cube::new("Sales", vec![region, measure]).unwrap()
    }

    #[test]
    fn chained_rules_feed_the_downstream_target() {
        let mut cube = chain_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        // Rule A derives Margin; Rule B reads the derived Margin to derive Net.
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];\n\
                 ['Measure':'Net'] = value['Measure':'Margin'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let (margin, net) = (
            cube.dimension(1).resolve("Margin").unwrap(),
            cube.dimension(1).resolve("Net").unwrap(),
        );
        let s = cube.dimension(0).resolve("South").unwrap();
        // The fixpoint feeds Net[North], which reads the rule-derived Margin[North]
        // -- the chained dependency the stored-leaf-only inference missed.
        assert!(inf.index.contains(&[n, margin]));
        assert!(inf.index.contains(&[n, net]));
        // South has no stored inputs, so nothing is fed there (still tight).
        assert!(!inf.index.contains(&[s, margin]));
        assert!(!inf.index.contains(&[s, net]));
    }

    #[test]
    fn consolidated_input_feeds_via_its_leaves() {
        let mut cube = chain_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        // Net reads the Region:Total rollup of the rule-derived Margin: a
        // consolidated input. The total is non-zero (Margin[North] != 0), so both
        // Net leaves are fed -- the consolidated-input case the old inference missed.
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];\n\
                 ['Measure':'Net'] = value['Region':'Total', 'Measure':'Margin'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let net = cube.dimension(1).resolve("Net").unwrap();
        let s = cube.dimension(0).resolve("South").unwrap();
        assert!(inf.index.contains(&[n, net]));
        assert!(inf.index.contains(&[s, net]));
    }

    #[test]
    fn rule_reading_an_overridden_consolidated_cell_is_fed() {
        // A consolidation-override sets Total/Sales = 1000 while the North/South
        // Sales leaves are empty. A downstream rule reads that overridden cell, so
        // its value is 1000 (non-zero) everywhere and both Net leaves must be fed.
        // Leaf expansion alone misses this (no populated leaf under Total/Sales) --
        // the override coordinate itself must count as potent (ADR-0005 soundness).
        let cube = chain_cube();
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Region':'Total', 'Measure':'Sales'] = 1000;\n\
                 ['Measure':'Net'] = value['Region':'Total', 'Measure':'Sales'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        let net = cube.dimension(1).resolve("Net").unwrap();
        let (n, s) = (
            cube.dimension(0).resolve("North").unwrap(),
            cube.dimension(0).resolve("South").unwrap(),
        );
        assert!(
            inf.index.contains(&[n, net]) && inf.index.contains(&[s, net]),
            "both Net leaves are fed via the overridden consolidated input"
        );
        // And validation confirms no under-feed against the dense truth.
        let reg = OneCube { cube, model };
        let diag = validate_feeders(&reg, 0, &inf.index).unwrap();
        assert!(diag.is_clean(), "no under-feed: {:?}", diag.under_fed);
    }

    #[test]
    fn multi_element_target_with_pinned_input_feeds_all_targets() {
        let mut cube = chain_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let cost = cube.dimension(1).resolve("Cost").unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        // Target is every leaf measure; the input pins one measure. The old
        // inference rejected a pinned input on a multi-member target dim as opaque;
        // now every target leaf whose pinned input is populated is fed.
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse("['Measure':{leaves}] = value['Measure':'Cost'];").unwrap(),
            1,
        )
        .unwrap();
        let inf = infer_feeders(&cube, &model, 0);
        assert!(
            inf.opaque.is_empty(),
            "a pinned input on a multi-leaf target is now analyzable"
        );
        let s = cube.dimension(0).resolve("South").unwrap();
        let leaves =
            ["Sales", "Cost", "Margin", "Net"].map(|m| cube.dimension(1).resolve(m).unwrap());
        for m in leaves {
            assert!(
                inf.index.contains(&[n, m]),
                "North feeds every measure leaf"
            );
            assert!(!inf.index.contains(&[s, m]), "South has no populated input");
        }
    }

    #[test]
    fn validate_records_erroring_targets_instead_of_aborting() {
        // Two rules: a healthy Margin and a Ratio that divides by an empty Cost
        // (DivByZero). Validation must still report Margin's under-feed AND list
        // Ratio's target in `erroring`, rather than aborting the whole report.
        let mut region = Dimension::new("Region");
        let n = region.add_leaf("North");
        let mut measure = Dimension::new("Measure");
        measure.add_leaf("Sales");
        measure.add_leaf("Cost");
        measure.add_leaf("Margin");
        measure.add_leaf("Ratio");
        let mut cube = Cube::new("Sales", vec![region, measure]).unwrap();
        let sales = cube.dimension(1).resolve("Sales").unwrap();
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        // Cost is left empty, so Ratio = Sales / Cost -> DivByZero.
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];\n\
                 ['Measure':'Ratio'] = value['Measure':'Sales'] / value['Measure':'Cost'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let reg = OneCube { cube, model };
        // Empty index: Margin[North] (value 100) under-fed; Ratio[North] errors.
        let diag = validate_feeders(&reg, 0, &FeederIndex::new()).unwrap();
        let n = reg.cube.dimension(0).resolve("North").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        let ratio = reg.cube.dimension(1).resolve("Ratio").unwrap();
        assert!(
            diag.under_fed.contains(&vec![n, margin]),
            "the healthy rule's under-feed is still reported"
        );
        assert_eq!(diag.erroring.len(), 1, "the erroring target is recorded");
        assert_eq!(diag.erroring[0].0, vec![n, ratio]);
        assert!(diag.erroring[0].1.contains("division by zero"));
    }

    #[test]
    fn mixed_target_override_rule_under_feed_is_detected() {
        let mut cube = sales_cube();
        let n = cube.dimension(0).resolve("North").unwrap();
        let (sales, cost) = (
            cube.dimension(1).resolve("Sales").unwrap(),
            cube.dimension(1).resolve("Cost").unwrap(),
        );
        cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
        cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
        // The Region target spans both leaves and the Total consolidation. The old
        // classifier treated this as a pure override and skipped validation, hiding
        // an under-feed of the leaf target [North, Margin] (value 40).
        let model = compile(
            &cube,
            &SingleCube::new(&cube),
            &parse(
                "['Region':{descendants of 'Total'}, 'Measure':'Margin'] = \
                 value['Measure':'Sales'] - value['Measure':'Cost'];",
            )
            .unwrap(),
            1,
        )
        .unwrap();
        let reg = OneCube { cube, model };
        let diag = validate_feeders(&reg, 0, &FeederIndex::new()).unwrap();
        let n = reg.cube.dimension(0).resolve("North").unwrap();
        let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
        assert!(
            diag.under_fed.contains(&vec![n, margin]),
            "the mixed-target rule's leaf under-feed is no longer silently skipped"
        );
    }
}
