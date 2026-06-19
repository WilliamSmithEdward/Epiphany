//! The query model seam: subsets and the set-evaluator trait.
//!
//! This module defines named selections ([`Subset`]) over a dimension and the
//! [`SetEvaluator`] trait by which dynamic (MDX) subsets are resolved. The trait
//! is *owned by core but implemented elsewhere* (epiphany-mdx), so core carries
//! no MDX dependency: a static subset resolves with no evaluator at all, and a
//! dynamic one delegates through the injected trait object. This mirrors how the
//! determinism `Clock`/`IdGen` are core-defined and injected at the edges.
//!
//! Views and cellsets build on this in Phase 3D.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use crate::{Cube, Dimension, ElementKind, Fixed, ModelError, Position};

/// Whether a saved object is shared with everyone or kept to its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// Visible to all users.
    Public,
    /// Visible only to its owner (and administrators).
    Private,
}

impl Visibility {
    /// `true` for [`Visibility::Public`].
    pub fn is_public(self) -> bool {
        matches!(self, Visibility::Public)
    }
}

/// How a subset's members are determined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubsetKind {
    /// An explicit, ordered list of member names (resolved at execution).
    Static {
        /// Member names in author order.
        members: Vec<String>,
    },
    /// An MDX set expression, evaluated at execution against the live dimension.
    Dynamic {
        /// The MDX set expression source.
        mdx: String,
    },
}

/// A named, ordered selection of elements from a single dimension.
///
/// Members are stored by name (not index) so a subset survives structural
/// change and round-trips through canonical text. Resolution to indices happens
/// at execution via [`resolve_subset`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subset {
    /// The subset name (unique within its dimension scope).
    pub name: String,
    /// The dimension this subset selects from.
    pub dimension: String,
    /// The owning user, if any (for visibility enforcement at the API layer).
    pub owner: Option<String>,
    /// Whether the subset is shared or private.
    pub visibility: Visibility,
    /// Static member list or dynamic MDX.
    pub kind: SubsetKind,
}

/// The seam by which dynamic (MDX) subsets are resolved.
///
/// Implemented in epiphany-mdx (`MdxEvaluator`) and injected at the composition
/// root, so epiphany-core never depends on the MDX crate. Evaluation reads the
/// cube/dimension immutably and resolves against the live (snapshot) dimension.
pub trait SetEvaluator {
    /// Evaluate an MDX set expression over `dim` (within `cube`) to an ordered,
    /// de-duplicated list of element indices.
    fn eval_set(&self, cube: &Cube, dim: &Dimension, mdx: &str) -> Result<Vec<u32>, QueryError>;
}

/// An evaluator that rejects dynamic subsets, letting the core query model and
/// its tests build and run with zero MDX dependency.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSetEvaluator;

impl SetEvaluator for NoSetEvaluator {
    fn eval_set(&self, _cube: &Cube, _dim: &Dimension, _mdx: &str) -> Result<Vec<u32>, QueryError> {
        Err(QueryError::DynamicUnsupported)
    }
}

/// The value seam: cell reads route through this so the same view-execution and
/// cell-read code serves plain stored consolidation ([`StoredCells`]) and, in
/// Phase 4, a rule-aware overlay implemented in `epiphany-calc` and injected at
/// the composition root. This mirrors the [`SetEvaluator`] injection pattern.
pub trait CellResolver {
    /// The numeric (consolidation-aware) value at a coordinate.
    fn value(&self, coord: &[u32]) -> Result<Fixed, QueryError>;
    /// The string value at a coordinate, if any.
    fn string_value(&self, coord: &[u32]) -> Result<Option<String>, QueryError>;
}

/// The default resolver: reads stored cells and consolidation directly, so its
/// output is byte-identical to [`Cube::get`] / [`Cube::get_string`] (no rules).
#[derive(Clone, Copy, Debug)]
pub struct StoredCells<'a>(pub &'a Cube);

impl CellResolver for StoredCells<'_> {
    fn value(&self, coord: &[u32]) -> Result<Fixed, QueryError> {
        Ok(self.0.get(coord)?)
    }

    fn string_value(&self, coord: &[u32]) -> Result<Option<String>, QueryError> {
        Ok(self.0.get_string(coord)?.map(str::to_string))
    }
}

/// Errors from resolving subsets and executing views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// A view referenced a subset that does not exist.
    UnknownSubset {
        /// The missing subset name.
        name: String,
    },
    /// A subset or axis named a dimension the cube does not have.
    UnknownDimension {
        /// The cube being queried.
        cube: String,
        /// The missing dimension name.
        dimension: String,
    },
    /// A static subset listed a member that does not resolve in its dimension.
    UnknownMember {
        /// The dimension being resolved.
        dimension: String,
        /// The unresolved member name.
        member: String,
    },
    /// A view's axes did not cover the cube's dimensions exactly once each.
    DimensionCoverage {
        /// A human-readable explanation.
        detail: String,
    },
    /// An axis placed a subset whose dimension differs from the axis dimension.
    SubsetDimensionMismatch {
        /// The referenced subset.
        subset: String,
        /// The dimension the axis declared.
        axis_dimension: String,
        /// The dimension the subset actually selects from.
        subset_dimension: String,
    },
    /// A dynamic subset was used with an evaluator that does not support MDX.
    DynamicUnsupported,
    /// A dynamic subset failed to parse or evaluate (message from the MDX layer).
    DynamicEval {
        /// The MDX parse/eval failure rendered as text.
        message: String,
    },
    /// A rule failed to evaluate (message from the calc layer: parse, cycle,
    /// division by zero, overflow, etc.). Carried as text so core stays calc-free.
    Calc {
        /// The calc failure rendered as text.
        message: String,
    },
    /// The caller is not permitted to read this coordinate: it directly names, or
    /// rolls up, an element denied to them (ADR-0015 element security). Carries no
    /// member identity (RG-13).
    AccessDenied,
    /// An underlying core model error.
    Model(ModelError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::UnknownSubset { name } => write!(f, "subset '{name}' not found"),
            QueryError::UnknownDimension { cube, dimension } => {
                write!(f, "dimension '{dimension}' not found in cube '{cube}'")
            }
            QueryError::UnknownMember { dimension, member } => {
                write!(f, "member '{member}' not found in dimension '{dimension}'")
            }
            QueryError::DimensionCoverage { detail } => write!(f, "{detail}"),
            QueryError::SubsetDimensionMismatch {
                subset,
                axis_dimension,
                subset_dimension,
            } => write!(
                f,
                "subset '{subset}' selects from '{subset_dimension}' but the axis is on '{axis_dimension}'"
            ),
            QueryError::DynamicUnsupported => {
                write!(
                    f,
                    "dynamic (MDX) subsets are not supported by this evaluator"
                )
            }
            QueryError::DynamicEval { message } => write!(f, "{message}"),
            QueryError::Calc { message } => write!(f, "{message}"),
            QueryError::AccessDenied => write!(f, "access denied"),
            QueryError::Model(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<ModelError> for QueryError {
    fn from(e: ModelError) -> Self {
        QueryError::Model(e)
    }
}

/// Find a dimension by name within a cube.
pub(crate) fn dimension_by_name<'a>(
    cube: &'a Cube,
    name: &str,
) -> Result<&'a Dimension, QueryError> {
    cube.dimensions()
        .iter()
        .find(|d| d.name() == name)
        .ok_or_else(|| QueryError::UnknownDimension {
            cube: cube.name().to_string(),
            dimension: name.to_string(),
        })
}

/// Resolve member names to an ordered, de-duplicated list of element indices.
///
/// Names resolve in order (first occurrence wins); an unresolved name yields
/// [`QueryError::UnknownMember`].
fn resolve_members(dim: &Dimension, names: &[String]) -> Result<Vec<u32>, QueryError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        let idx = dim.resolve(name).ok_or_else(|| QueryError::UnknownMember {
            dimension: dim.name().to_string(),
            member: name.clone(),
        })?;
        if seen.insert(idx) {
            out.push(idx);
        }
    }
    Ok(out)
}

/// Resolve a subset to an ordered, de-duplicated list of element indices.
///
/// Static subsets resolve member names in author order (first occurrence wins);
/// dynamic subsets delegate to `eval`. The result is deterministic.
pub fn resolve_subset(
    cube: &Cube,
    subset: &Subset,
    eval: &dyn SetEvaluator,
) -> Result<Vec<u32>, QueryError> {
    let dim = dimension_by_name(cube, &subset.dimension)?;
    match &subset.kind {
        SubsetKind::Static { members } => resolve_members(dim, members),
        SubsetKind::Dynamic { mdx } => eval.eval_set(cube, dim, mdx),
    }
}

/// Structurally validate a subset definition without resolving it.
///
/// Checks that the dimension exists and (for a static subset) that every listed
/// member resolves. A dynamic subset's MDX is opaque here (it is validated by
/// the evaluator at the API boundary), so only its dimension is checked.
pub fn validate_subset(cube: &Cube, subset: &Subset) -> Result<(), QueryError> {
    let dim = dimension_by_name(cube, &subset.dimension)?;
    if let SubsetKind::Static { members } = &subset.kind {
        for member in members {
            dim.resolve(member)
                .ok_or_else(|| QueryError::UnknownMember {
                    dimension: dim.name().to_string(),
                    member: member.clone(),
                })?;
        }
    }
    Ok(())
}

/// A single placement on an axis: a saved subset, or an inline member list.
///
/// Each spec selects members from exactly one dimension. An [`Axis`] is an
/// ordered list of specs whose member lists are crossjoined to form tuples, with
/// the first spec varying slowest (outermost nesting).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AxisSpec {
    /// Reference a saved subset by dimension and name.
    Subset {
        /// The dimension the subset selects from.
        dimension: String,
        /// The subset name.
        subset: String,
    },
    /// An ad-hoc, ordered list of member names in a dimension.
    Members {
        /// The dimension the members belong to.
        dimension: String,
        /// Member names in author order.
        members: Vec<String>,
    },
}

impl AxisSpec {
    /// The dimension this spec selects from.
    pub fn dimension(&self) -> &str {
        match self {
            AxisSpec::Subset { dimension, .. } | AxisSpec::Members { dimension, .. } => dimension,
        }
    }
}

/// An ordered list of axis specs; their member lists crossjoin into tuples.
pub type Axis = Vec<AxisSpec>;

/// How deep a provenance trace should recurse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainDepth {
    /// The cell and one level of inputs (their inputs are omitted).
    Immediate,
    /// Recurse fully (bounded by a safety cap and the per-query cycle guard).
    Full,
    /// Recurse a fixed number of levels.
    Levels(u32),
}

/// What produced a cell's value, in a [`CellTrace`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceKind {
    /// A stored leaf value.
    Stored,
    /// A rule-derived value (the firing rule and its source span).
    Rule {
        /// The rule's id (its index in the cube's rule set).
        rule: usize,
        /// The rule statement's source byte span (the API maps it to line/col).
        span: (usize, usize),
    },
    /// A consolidation of contributing leaves.
    Consolidation {
        /// How many contributing inputs were included.
        contributions: usize,
    },
}

/// A provenance ("explain") node: the value at a cell, what produced it, and its
/// inputs (recursively, depth-bounded). Coordinates are member names; the value
/// is exact [`Fixed`] (the API stringifies it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellTrace {
    /// The cube the cell belongs to.
    pub cube: String,
    /// The cell coordinate as member names (dimension order).
    pub coord: Vec<String>,
    /// The cell's value.
    pub value: Fixed,
    /// What produced the value.
    pub kind: TraceKind,
    /// The input cells consulted (empty at the depth limit or for a stored leaf).
    pub inputs: Vec<CellTrace>,
}

/// A saved query: rows, columns, a context (slicer), and a zero-suppression
/// flag, over one cube. Every cube dimension must appear on exactly one of
/// rows / columns / context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The view name.
    pub name: String,
    /// The cube this view queries.
    pub cube: String,
    /// The owning user, if any.
    pub owner: Option<String>,
    /// Whether the view is shared or private.
    pub visibility: Visibility,
    /// The row axis.
    pub rows: Axis,
    /// The column axis.
    pub columns: Axis,
    /// The context (slicer): a fixed member per remaining dimension.
    pub context: Vec<(String, String)>,
    /// Drop all-zero row and column tuples when executing.
    pub suppress_zeros: bool,
}

/// The result of executing a view: a dense, row-major value matrix over the
/// surviving row and column tuples, plus what zero-suppression removed.
///
/// Tuples are member *names* (the presentation form); the API layer re-resolves
/// them to indices to derive per-cell kind/editability. Values are exact
/// [`Fixed`]; the API stringifies them (ADR-0008).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cellset {
    /// Row-axis dimension names, outermost first.
    pub row_dimensions: Vec<String>,
    /// Column-axis dimension names, outermost first.
    pub column_dimensions: Vec<String>,
    /// Surviving row tuples (one member name per row dimension).
    pub row_tuples: Vec<Vec<String>>,
    /// Surviving column tuples (one member name per column dimension).
    pub column_tuples: Vec<Vec<String>>,
    /// The echoed context (dimension, member).
    pub context: Vec<(String, String)>,
    /// Cell values, row-major: `cells[r * column_tuples.len() + c]`.
    pub cells: Vec<Fixed>,
    /// Row tuples removed by zero-suppression, in original order.
    pub suppressed_row_tuples: Vec<Vec<String>>,
    /// Column tuples removed by zero-suppression, in original order.
    pub suppressed_column_tuples: Vec<Vec<String>>,
}

/// How the cellset value grid is computed: serial, or in parallel across
/// independent output cells (ADR-0028 Stage B).
///
/// Parallelism is over whole output cells, never within a cell's reduction, so
/// the result is bit-identical regardless of worker count or scheduling: cell
/// `(r, c)` always writes only its own slot and the within-cell reduction order
/// is unchanged. Small reads stay serial (below `threshold`), so the common case
/// is untouched.
#[derive(Clone, Copy, Debug)]
pub struct Parallelism {
    max_workers: usize,
    threshold: usize,
}

impl Parallelism {
    /// Parallelize large reads across the available cores (capped), leaving small
    /// reads serial. The default used by [`execute_view`].
    pub fn auto() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self {
            max_workers: cores.min(8),
            threshold: 1024,
        }
    }

    /// Always serial (one worker). Used by callers that must not spawn threads
    /// and as the determinism-test baseline.
    pub fn serial() -> Self {
        Self {
            max_workers: 1,
            threshold: usize::MAX,
        }
    }

    /// Force `workers` workers regardless of size (testing/benchmarks). One worker
    /// is serial.
    pub fn forced(workers: usize) -> Self {
        Self {
            max_workers: workers.max(1),
            threshold: 0,
        }
    }

    /// The worker count to use for a grid of `total` cells (1 = serial).
    fn workers_for(&self, total: usize) -> usize {
        if self.max_workers <= 1 || total == 0 || total < self.threshold {
            1
        } else {
            self.max_workers.min(total)
        }
    }
}

/// Execute a view over a cube into a [`Cellset`], filling the value grid with the
/// default [`Parallelism::auto`] policy. See [`execute_view_with`].
pub fn execute_view<'a>(
    cube: &Cube,
    view: &View,
    cells: &(dyn CellResolver + Sync),
    subset_lookup: &dyn Fn(&str, &str) -> Option<&'a Subset>,
    eval: &dyn SetEvaluator,
    mask: Option<&crate::ElementMask>,
) -> Result<Cellset, QueryError> {
    execute_view_with(
        cube,
        view,
        cells,
        subset_lookup,
        eval,
        mask,
        Parallelism::auto(),
    )
}

/// Execute a view over a cube into a [`Cellset`] with an explicit parallelism
/// policy.
///
/// `cells` is the value seam: cell values are read through it, so a
/// [`StoredCells`] reads exactly today's stored consolidation while a rule-aware
/// resolver overlays rule-derived values (Phase 4). `subset_lookup(dimension,
/// name)` resolves a saved subset referenced by an axis; `eval` resolves dynamic
/// (MDX) subsets. The function validates exact one-axis-per-dimension coverage,
/// resolves each axis to crossjoined member tuples, reads consolidation-aware
/// values via `cells`, then applies zero-suppression (rows first, then columns)
/// preserving order. An axis or suppression that yields zero tuples is a valid
/// empty result, not an error. `cells` is `Sync` because the value grid may be
/// filled from several threads (`par`); the resolver is only ever read, never
/// mutated, across them.
pub fn execute_view_with<'a>(
    cube: &Cube,
    view: &View,
    cells: &(dyn CellResolver + Sync),
    subset_lookup: &dyn Fn(&str, &str) -> Option<&'a Subset>,
    eval: &dyn SetEvaluator,
    mask: Option<&crate::ElementMask>,
    par: Parallelism,
) -> Result<Cellset, QueryError> {
    validate_coverage(cube, view)?;

    let (row_dimensions, mut row_tuples_idx) = resolve_axis(cube, &view.rows, subset_lookup, eval)?;
    let (column_dimensions, mut column_tuples_idx) =
        resolve_axis(cube, &view.columns, subset_lookup, eval)?;

    // Map every dimension name to its position in the cube coordinate.
    let dim_index: std::collections::HashMap<&str, usize> = cube
        .dimensions()
        .iter()
        .enumerate()
        .map(|(i, d)| (d.name(), i))
        .collect();

    // The base coordinate fixes the context dimensions; axes overlay the rest.
    let mut base_coord = vec![0u32; cube.rank()];
    for (dim_name, member) in &view.context {
        let ci = dim_index[dim_name.as_str()];
        let idx = cube
            .dimension(ci)
            .resolve(member)
            .ok_or_else(|| QueryError::UnknownMember {
                dimension: dim_name.clone(),
                member: member.clone(),
            })?;
        // A pinned context member the caller may not see denies the whole result:
        // every cell is fixed at that member (ADR-0015 element security).
        if let Some(mask) = mask {
            if mask.denies_member(cube, ci, idx) {
                return Err(QueryError::AccessDenied);
            }
        }
        base_coord[ci] = idx;
    }
    let row_ci: Vec<usize> = row_dimensions
        .iter()
        .map(|d| dim_index[d.as_str()])
        .collect();
    let col_ci: Vec<usize> = column_dimensions
        .iter()
        .map(|d| dim_index[d.as_str()])
        .collect();

    // Suppress denied members from each axis (ADR-0015): a member the caller may
    // not see, or that rolls up a denied leaf, is omitted like zero-suppression.
    // It is dropped silently (never reported in the suppressed lists) so the
    // member's existence does not leak.
    if let Some(mask) = mask {
        row_tuples_idx.retain(|t| {
            !t.iter()
                .enumerate()
                .any(|(k, &m)| mask.denies_member(cube, row_ci[k], m))
        });
        column_tuples_idx.retain(|t| {
            !t.iter()
                .enumerate()
                .any(|(k, &m)| mask.denies_member(cube, col_ci[k], m))
        });
    }

    // Dense value grid over all (row, column) tuples, row-major. Each output cell
    // is computed independently, so the grid can be filled in parallel across
    // disjoint row bands without changing the result (ADR-0028 Stage B): cell
    // (r, c) only ever writes its own slot `r * ncols + c`, so completion order is
    // irrelevant, and the within-cell reduction order is unchanged.
    let nrows = row_tuples_idx.len();
    let ncols = column_tuples_idx.len();
    let total = nrows * ncols;

    // Compute one cell's full coordinate into `scratch` and read its value. Reads
    // only (no shared mutation), so it is safe to call from several threads.
    let cell_at = |r: usize, c: usize, scratch: &mut Vec<u32>| -> Result<Fixed, QueryError> {
        scratch.clear();
        scratch.extend_from_slice(&base_coord);
        for (k, &idx) in row_tuples_idx[r].iter().enumerate() {
            scratch[row_ci[k]] = idx;
        }
        for (k, &idx) in column_tuples_idx[c].iter().enumerate() {
            scratch[col_ci[k]] = idx;
        }
        cells.value(scratch)
    };

    let grid: Vec<Fixed> = match par.workers_for(total) {
        workers if workers > 1 => fill_grid_parallel(nrows, ncols, workers, &cell_at)?,
        _ => {
            let mut grid = Vec::with_capacity(total);
            let mut scratch = Vec::with_capacity(cube.rank());
            for r in 0..nrows {
                for c in 0..ncols {
                    grid.push(cell_at(r, c, &mut scratch)?);
                }
            }
            grid
        }
    };
    let at = |r: usize, c: usize| grid[r * ncols + c];

    // Zero-suppression: rows first, then columns over the surviving rows. Only
    // meaningful when both axes are non-empty (otherwise there are no cells).
    let suppress = view.suppress_zeros && nrows > 0 && ncols > 0;
    let (keep_rows, supp_rows): (Vec<usize>, Vec<usize>) = if suppress {
        (0..nrows).partition(|&r| (0..ncols).any(|c| !at(r, c).is_zero()))
    } else {
        ((0..nrows).collect(), Vec::new())
    };
    let suppress_cols = suppress && !keep_rows.is_empty();
    let (keep_cols, supp_cols): (Vec<usize>, Vec<usize>) = if suppress_cols {
        (0..ncols).partition(|&c| keep_rows.iter().any(|&r| !at(r, c).is_zero()))
    } else {
        ((0..ncols).collect(), Vec::new())
    };

    let mut cells = Vec::with_capacity(keep_rows.len() * keep_cols.len());
    for &r in &keep_rows {
        for &c in &keep_cols {
            cells.push(at(r, c));
        }
    }

    let to_names = |tuples: &[Vec<u32>], ci: &[usize], picks: &[usize]| -> Vec<Vec<String>> {
        picks
            .iter()
            .map(|&i| {
                tuples[i]
                    .iter()
                    .enumerate()
                    .map(|(k, &idx)| cube.dimension(ci[k]).element(idx).unwrap().name.clone())
                    .collect()
            })
            .collect()
    };

    Ok(Cellset {
        row_tuples: to_names(&row_tuples_idx, &row_ci, &keep_rows),
        column_tuples: to_names(&column_tuples_idx, &col_ci, &keep_cols),
        suppressed_row_tuples: to_names(&row_tuples_idx, &row_ci, &supp_rows),
        suppressed_column_tuples: to_names(&column_tuples_idx, &col_ci, &supp_cols),
        row_dimensions,
        column_dimensions,
        context: view.context.clone(),
        cells,
    })
}

/// Fill the dense `nrows x ncols` value grid (row-major) across `workers` scoped
/// threads, each owning a disjoint contiguous band of rows (ADR-0028 Stage B).
///
/// Determinism: thread `k` writes only the slots in its band, so the assembled
/// grid is bit-identical to the serial fill regardless of worker count or
/// scheduling. There is no shared mutable state and no randomness in the parallel
/// region; `cell_at` only reads. An error is reported deterministically as the
/// failing cell with the lowest row-major ordinal, matching the serial path's
/// first-error semantics.
fn fill_grid_parallel<F>(
    nrows: usize,
    ncols: usize,
    workers: usize,
    cell_at: &F,
) -> Result<Vec<Fixed>, QueryError>
where
    F: Fn(usize, usize, &mut Vec<u32>) -> Result<Fixed, QueryError> + Sync,
{
    let total = nrows * ncols;
    let mut grid = vec![Fixed::ZERO; total];
    let rows_per = nrows.div_ceil(workers);
    let band_cells = (rows_per * ncols).max(1);

    let band_errors: Vec<Option<(usize, QueryError)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = grid
            .chunks_mut(band_cells)
            .enumerate()
            .map(|(band, chunk)| {
                let r0 = band * rows_per;
                scope.spawn(move || {
                    let mut scratch: Vec<u32> = Vec::new();
                    let mut first: Option<(usize, QueryError)> = None;
                    let band_rows = chunk.len() / ncols;
                    for rr in 0..band_rows {
                        let r = r0 + rr;
                        for c in 0..ncols {
                            match cell_at(r, c, &mut scratch) {
                                Ok(v) => chunk[rr * ncols + c] = v,
                                Err(e) => {
                                    let ord = r * ncols + c;
                                    if first.as_ref().is_none_or(|(o, _)| ord < *o) {
                                        first = Some((ord, e));
                                    }
                                }
                            }
                        }
                    }
                    first
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("view aggregation worker panicked"))
            .collect()
    });

    // The deterministic error is the failing cell with the lowest ordinal.
    let mut lowest: Option<(usize, QueryError)> = None;
    for err in band_errors.into_iter().flatten() {
        if lowest.as_ref().is_none_or(|(o, _)| err.0 < *o) {
            lowest = Some(err);
        }
    }
    if let Some((_, e)) = lowest {
        return Err(e);
    }
    Ok(grid)
}

/// Structurally validate a view against a model without executing it.
///
/// Checks exact one-axis-per-dimension coverage, that every referenced subset
/// exists with a matching dimension, and that inline-member and context names
/// resolve. Does not resolve dynamic subsets (no evaluator needed), so it is the
/// validation used when persisting a definition.
pub fn validate_view(model: &Model, view: &View) -> Result<(), QueryError> {
    let cube = &model.cube;
    validate_coverage(cube, view)?;
    for spec in view.rows.iter().chain(view.columns.iter()) {
        match spec {
            AxisSpec::Subset { dimension, subset } => {
                let s =
                    model
                        .subset(dimension, subset)
                        .ok_or_else(|| QueryError::UnknownSubset {
                            name: subset.clone(),
                        })?;
                if &s.dimension != dimension {
                    return Err(QueryError::SubsetDimensionMismatch {
                        subset: subset.clone(),
                        axis_dimension: dimension.clone(),
                        subset_dimension: s.dimension.clone(),
                    });
                }
            }
            AxisSpec::Members { dimension, members } => {
                let dim = dimension_by_name(cube, dimension)?;
                for member in members {
                    dim.resolve(member)
                        .ok_or_else(|| QueryError::UnknownMember {
                            dimension: dim.name().to_string(),
                            member: member.clone(),
                        })?;
                }
            }
        }
    }
    for (dimension, member) in &view.context {
        let dim = dimension_by_name(cube, dimension)?;
        dim.resolve(member)
            .ok_or_else(|| QueryError::UnknownMember {
                dimension: dimension.clone(),
                member: member.clone(),
            })?;
    }
    Ok(())
}

/// Validate that every cube dimension is placed on exactly one axis or context,
/// and that no axis/context names a dimension the cube lacks.
fn validate_coverage(cube: &Cube, view: &View) -> Result<(), QueryError> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for spec in view.rows.iter().chain(view.columns.iter()) {
        *counts.entry(spec.dimension().to_string()).or_default() += 1;
    }
    for (dim, _) in &view.context {
        *counts.entry(dim.clone()).or_default() += 1;
    }
    for name in counts.keys() {
        if !cube.dimensions().iter().any(|d| d.name() == name) {
            return Err(QueryError::UnknownDimension {
                cube: cube.name().to_string(),
                dimension: name.clone(),
            });
        }
    }
    for (name, &n) in &counts {
        if n > 1 {
            return Err(QueryError::DimensionCoverage {
                detail: format!("dimension '{name}' is placed on more than one axis"),
            });
        }
    }
    for d in cube.dimensions() {
        if !counts.contains_key(d.name()) {
            return Err(QueryError::DimensionCoverage {
                detail: format!(
                    "dimension '{}' is not placed on any axis or context",
                    d.name()
                ),
            });
        }
    }
    Ok(())
}

/// Resolve an axis to its dimension names and the crossjoined member-index
/// tuples (first spec slowest).
fn resolve_axis<'a>(
    cube: &Cube,
    axis: &Axis,
    subset_lookup: &dyn Fn(&str, &str) -> Option<&'a Subset>,
    eval: &dyn SetEvaluator,
) -> Result<(Vec<String>, Vec<Vec<u32>>), QueryError> {
    let mut dimensions = Vec::with_capacity(axis.len());
    let mut per_spec: Vec<Vec<u32>> = Vec::with_capacity(axis.len());
    for spec in axis {
        let indices = match spec {
            AxisSpec::Subset { dimension, subset } => {
                let s =
                    subset_lookup(dimension, subset).ok_or_else(|| QueryError::UnknownSubset {
                        name: subset.clone(),
                    })?;
                if &s.dimension != dimension {
                    return Err(QueryError::SubsetDimensionMismatch {
                        subset: subset.clone(),
                        axis_dimension: dimension.clone(),
                        subset_dimension: s.dimension.clone(),
                    });
                }
                resolve_subset(cube, s, eval)?
            }
            AxisSpec::Members { dimension, members } => {
                let dim = dimension_by_name(cube, dimension)?;
                resolve_members(dim, members)?
            }
        };
        dimensions.push(spec.dimension().to_string());
        per_spec.push(indices);
    }
    Ok((dimensions, crossjoin(&per_spec)))
}

/// A cube's calculation rules, stored as model-as-code source text. Core keeps it
/// opaque (the way a dynamic subset carries opaque MDX); `epiphany-calc` parses
/// and compiles it. An empty source means the cube has no rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleSet {
    /// The rules-language source text.
    pub source: String,
}

impl RuleSet {
    /// Whether there are no rules.
    pub fn is_empty(&self) -> bool {
        self.source.trim().is_empty()
    }
}

/// One cell of a rule unit test: a coordinate (dimension -> member) and a value
/// (a decimal string to set, for a fixture, or the expected value, for an
/// assertion).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestCell {
    /// The coordinate as dimension -> member names.
    pub coord: BTreeMap<String, String>,
    /// The decimal-string value (fixture input or expected output).
    pub value: String,
}

/// A rule unit test: set the `fixtures`, then assert the derived `assertions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleTest {
    /// The test name (unique within the cube).
    pub name: String,
    /// Leaf cells to set before evaluating.
    pub fixtures: Vec<TestCell>,
    /// Cells whose derived value is asserted.
    pub assertions: Vec<TestCell>,
}

/// How a flow's named data input is bound to a source (ADR-0035). Inputs are
/// configured on the flow in the UI; outputs (cubes, dimensions) are named in
/// code. A global input references a server-global connection by name; a
/// flow-scoped input carries its own connection definition, private to the flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowInputBinding {
    /// References a server-global [`Connection`] by name. The flow input's `name`
    /// equals that connection's name (the UI locks it), and code reads it by the
    /// bare name: `ctx.input('sales_db')`.
    Global,
    /// A flow-scoped connection defined inline on the flow. Code reads it by a
    /// `local.` prefix: `ctx.input('local.daily_csv')`. It obeys the same
    /// connector controls (build feature, enable flag, host allowlist, secrets by
    /// name) as a global connection.
    Local(ConnectionSpec),
}

/// A named data input on a flow (ADR-0035). A flow may declare zero or more; at
/// run time the API resolves each to rows and passes the name->rows map to the
/// runner, so one flow can join several sources (e.g. a CSV and a SQL query).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlowInput {
    /// The in-flow name. For a [`Global`](FlowInputBinding::Global) binding this
    /// equals the referenced connection's name; for a
    /// [`Local`](FlowInputBinding::Local) binding the author chooses it (unique
    /// within the flow). The code address is the bare name for a global source
    /// and `local.<name>` for a flow-scoped source, so the two namespaces never
    /// collide.
    pub name: String,
    /// How the input is bound to a source.
    pub binding: FlowInputBinding,
}

impl FlowInput {
    /// The address the flow body reads this source by (`ctx.input(address)`): the
    /// bare name for a global input, `local.<name>` for a flow-scoped one, so the
    /// two namespaces never collide.
    pub fn address(&self) -> String {
        match self.binding {
            FlowInputBinding::Global => self.name.clone(),
            FlowInputBinding::Local(_) => format!("local.{}", self.name),
        }
    }
}

/// A flow definition: a TypeScript ETL/automation script, stored as model-as-code
/// source text. Core keeps it opaque (the way a cube's rules carry opaque rule
/// source); `epiphany-flow` strips its types, runs it on the embedded engine, and
/// turns its staged outputs into element and cell changes. A flow is a
/// server-global object (ADR-0035), owned by no cube; its body names the cubes
/// and dimensions it acts on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flow {
    /// The flow name (unique across the server).
    pub name: String,
    /// The TypeScript source text.
    pub source: String,
    /// The principal a scheduled run executes as (ADR-0035): a flow run is gated
    /// by this owner's object and element security, so an unattended run is bound
    /// to a real principal's rights rather than running privileged. `None` for a
    /// flow that has never recorded an owner (migration stamps one).
    pub owner: Option<String>,
    /// An optional back-compatibility target cube (ADR-0035): when set, the legacy
    /// cube-less `ctx.writeCells`/`ctx.ensureElements`/... calls target it. A
    /// migration shim for lifted per-cube flows, not an authoring picker; `None`
    /// means a cube-less call errors and the body must name a cube.
    pub default_cube: Option<String>,
    /// The flow's data inputs (ADR-0035), configured in the UI. Resolved to rows
    /// at run time and read in code via `ctx.input(name)`.
    pub inputs: Vec<FlowInput>,
}

/// A flow unit test: run the flow over a fixed `input` and `params`, then assert
/// the resulting cell values. The input is the data-source content (for the CSV
/// source, the inline CSV text); empty for a source-less flow. Reproducible
/// because the input and parameters are pinned and the runtime is deterministic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlowTest {
    /// The test name (unique across the server).
    pub name: String,
    /// The name of the flow this test runs.
    pub flow: String,
    /// The sole-source content the flow reads (e.g. inline CSV text), for a
    /// single-source flow. Back-compatible with the pre-ADR-0035 single input.
    pub input: String,
    /// Named-source contents for a multi-source flow (ADR-0035): the source
    /// address (bare name for a global input, `local.<name>` for a flow-scoped
    /// one) to the inline content the test pins for it. Empty falls back to
    /// `input` for the sole source.
    pub inputs: BTreeMap<String, String>,
    /// The target cube whose staged cells the assertions check (ADR-0035). `None`
    /// uses the flow's `default_cube`; a multi-cube flow names the cube here.
    pub cube: Option<String>,
    /// Flow parameters (name -> value), available to the flow as `ctx.param(...)`.
    pub params: BTreeMap<String, String>,
    /// Cells whose value is asserted after the flow runs.
    pub assertions: Vec<TestCell>,
}

/// A schedule trigger (ADR-0013). `Interval` is the Phase 8 cut; a calendar/cron
/// trigger is a deferred follow-on (its own increment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Fire every `every_millis`, measured from the last fire. Pure millis
    /// arithmetic (DST-immune). A never-fired job is due immediately.
    Interval {
        /// The interval between fires, in milliseconds.
        every_millis: u64,
    },
}

impl Trigger {
    /// The instant at which a job with this trigger is next due, given when it
    /// last fired (`None` = never). Pure: it never reads a clock. A firing is due
    /// when `next_due(last_fired) <= now`. A never-fired interval job returns `0`,
    /// so it fires on the first reconcile tick after it is enabled.
    pub fn next_due(&self, last_fired: Option<u64>) -> u64 {
        match self {
            Trigger::Interval { every_millis } => {
                last_fired.map_or(0, |t| t.saturating_add(*every_millis))
            }
        }
    }
}

/// A scheduled job (ADR-0013, made global by ADR-0035): an ordered sequence of
/// global flows, run on a trigger. This is *desired state*, persisted as
/// model-as-code like a flow; run history lives in the separate durable run
/// ledger, never here. A job is owned by no cube; the cubes it writes are
/// whatever its flows' bodies address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// The job name (unique across the server).
    pub name: String,
    /// The flow names to run in order (fail-fast), each a server-global flow.
    pub steps: Vec<String>,
    /// When the job fires.
    pub trigger: Trigger,
    /// Whether the scheduler considers this job; a disabled job never fires.
    pub enabled: bool,
}

/// How a connector's output (or a flow's input) is parsed into rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourceFormat {
    /// Comma-separated values, a header row naming the columns.
    #[default]
    Csv,
    /// A JSON array of objects (or an array reached by [`CommandSpec::json_path`]).
    Json,
}

/// A command connection's configuration: run `program` with fixed `args` and
/// read its stdout as `format`. The program and args are set by an admin at
/// definition time and are never supplied by a flow, and the program is spawned
/// directly (no shell), so there is no command-injection surface (ADR-0012).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandSpec {
    /// The executable to run (an absolute path or a program on the host PATH).
    pub program: String,
    /// Fixed arguments, passed as an argv array (not a shell string).
    pub args: Vec<String>,
    /// How to parse the program's stdout into rows.
    pub format: SourceFormat,
    /// For JSON output, a dotted path to the array of record objects; `None`
    /// means stdout is itself the array.
    pub json_path: Option<String>,
    /// Kill the process if it runs longer than this many milliseconds. A value
    /// of 0 means no timeout (the REST layer coerces an unset value to a safe
    /// default, so 0 only arises from a hand-edited model).
    pub timeout_ms: u64,
    /// Working directory the program runs in (ADR-0012 addendum). `None` inherits
    /// the server's directory (unspecified, often a filesystem root); set it to
    /// make the program's relative paths predictable. The REST definition boundary
    /// validates it is an absolute path with no `..` traversal; a value loaded
    /// from an on-disk model is trusted (the model file is a full-trust boundary,
    /// ADR-0012 decision 6), so a non-absolute or traversal value only arises from
    /// a hand-edited model.
    pub working_dir: Option<String>,
}

/// The HTTP authentication scheme for an [`HttpAuth`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpAuthKind {
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// `Authorization: Basic base64(user:password)`, the secret holding
    /// `user:password`.
    Basic,
}

/// How an HTTP connection authenticates (ADR-0030). `secret` names an entry in
/// the operator's secret store; the credential value never lives in the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpAuth {
    /// The authentication scheme.
    pub kind: HttpAuthKind,
    /// The name of the secret holding the credential (a bearer token, or
    /// `user:password` for basic). A name, never the value.
    pub secret: String,
}

/// An HTTP(S) connection's configuration (ADR-0030): GET `url` with optional
/// static `headers` and a referenced credential, reading the response as
/// `format`. The capability is off by default and constrained by an operator
/// host allowlist; credentials are referenced by name, never stored here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpSpec {
    /// The absolute URL to fetch (http or https).
    pub url: String,
    /// Static, non-secret request headers.
    pub headers: Vec<(String, String)>,
    /// Optional credential, referenced by secret name.
    pub auth: Option<HttpAuth>,
    /// How to parse the response body into rows.
    pub format: SourceFormat,
    /// For JSON, a dotted path to the array of record objects; `None` means the
    /// body is itself the array.
    pub json_path: Option<String>,
    /// Connect/read timeout in milliseconds (0 means the REST layer's safe
    /// default; 0 only arises from a hand-edited model).
    pub timeout_ms: u64,
}

/// The database engine of a [`SqlSpec`] (ADR-0034). SQL Server is intentionally
/// absent: its only pure-Rust driver pulls a TLS library with active
/// certificate-verification advisories (see ADR-0034), so it is deferred and
/// reached via a `command` connection meanwhile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SqlEngine {
    /// PostgreSQL (and Postgres-wire-compatible databases).
    #[default]
    Postgres,
    /// MySQL / MariaDB.
    MySql,
}

/// How a SQL connection negotiates TLS (ADR-0034). The secure mode is the
/// default; an operator with a self-signed internal database opts down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SqlSslMode {
    /// rustls with the bundled public roots and full certificate verification.
    #[default]
    VerifyFull,
    /// Encrypt over rustls but do NOT verify the server certificate (the libpq
    /// `sslmode=require` behavior; for self-signed internal-database certs).
    Require,
    /// No TLS.
    Disable,
}

/// A SQL connection's configuration (ADR-0034): connect to a database and run a
/// fixed, admin-defined `query`, mapping each result row to the same rows the
/// other connectors produce. The capability is off by default and constrained by
/// an operator host allowlist; the password is referenced by secret name, never
/// stored here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqlSpec {
    /// The database engine (only Postgres is implemented).
    pub engine: SqlEngine,
    /// The database host (the API gates it against an operator allowlist).
    pub host: String,
    /// The database port.
    pub port: u16,
    /// The database (catalog) name.
    pub database: String,
    /// The connecting user (not secret).
    pub user: String,
    /// The name of the secret holding the password; `None` for a passwordless
    /// connection. A name, never the value.
    pub password_secret: Option<String>,
    /// The fixed SQL query to run. Admin-defined at definition time and never
    /// assembled from flow input, so a flow presents no injection surface.
    pub query: String,
    /// TLS negotiation mode.
    pub ssl_mode: SqlSslMode,
    /// Connect/query timeout in milliseconds (0 means the REST layer's safe
    /// default; 0 only arises from a hand-edited model).
    pub timeout_ms: u64,
}

/// What a connection does: run a command (ADR-0012), fetch an HTTP(S) URL
/// (ADR-0030), or query a database (ADR-0034). A native SQL connection uses a
/// pure-Rust driver over rustls, so the server stays a single binary; a database
/// without a built-in driver is still reachable via a command connection running
/// the user's own client script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionSpec {
    /// Run an external program and read its stdout (ADR-0012 decision 6).
    Command(CommandSpec),
    /// Fetch an HTTP(S) URL and read the response body (ADR-0030).
    Http(HttpSpec),
    /// Query a database and read its result rows (ADR-0034).
    Sql(SqlSpec),
}

/// A named, admin-defined data-source connection. A server-global connection
/// (ADR-0035) is referenced by global flows by name to ingest external data; the
/// connection itself (and the capability it grants) is an operator artifact,
/// never authored by a flow. The same shape is reused for a flow-scoped
/// connection embedded on a flow ([`FlowInputBinding::Local`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// The connection name (unique across the server for a global connection;
    /// unique within the flow for a flow-scoped one).
    pub name: String,
    /// What the connection does.
    pub spec: ConnectionSpec,
}

/// A named, per-user what-if overlay over one cube (ADR-0014): a sparse set of
/// stored-leaf value overrides (numeric and string). Rules and consolidations
/// recompute over the overrides without touching base data. Coordinates are
/// stored as element indices, like the cube's own cells, and serialize as
/// element names so they survive structural change and round-trip canonically.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sandbox {
    /// The sandbox name (unique within the cube).
    pub name: String,
    /// The owning user. A sandbox is private to its owner (admins may use any).
    pub owner: String,
    /// An injected id stamped when the sandbox was created (never a wall clock,
    /// per ADR-0009, so creation order is reproducible in tests).
    pub created: u64,
    /// An injected id stamped on the most recent change to the sandbox.
    pub updated: u64,
    /// Numeric leaf overrides, keyed by coordinate (element indices).
    pub cells: BTreeMap<Vec<u32>, Fixed>,
    /// String leaf overrides, keyed by coordinate (element indices).
    pub string_cells: BTreeMap<Vec<u32>, String>,
}

impl Sandbox {
    /// A new, empty sandbox owned by `owner`, stamped with creation id `created`.
    pub fn new(name: impl Into<String>, owner: impl Into<String>, created: u64) -> Self {
        Self {
            name: name.into(),
            owner: owner.into(),
            created,
            updated: created,
            cells: BTreeMap::new(),
            string_cells: BTreeMap::new(),
        }
    }

    /// The numeric override at a coordinate, if any.
    pub fn cell(&self, coord: &[u32]) -> Option<Fixed> {
        self.cells.get(coord).copied()
    }

    /// The string override at a coordinate, if any.
    pub fn string_cell(&self, coord: &[u32]) -> Option<&str> {
        self.string_cells.get(coord).map(String::as_str)
    }

    /// The number of overridden cells (numeric plus string).
    pub fn len(&self) -> usize {
        self.cells.len() + self.string_cells.len()
    }

    /// Whether the sandbox has no overrides.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty() && self.string_cells.is_empty()
    }
}

/// Remap a coordinate-keyed override map for a structural edit to dimension `d`:
/// each key's `d`-th component is moved via `to_new` (an old-index to new-index
/// table), and an entry whose component maps to `u32::MAX` (a removed member) is
/// dropped. Keeps per-user sandbox overrides aligned after a reorder, insert, or
/// delete, since their coordinates are element indices like the cube's own cells.
fn remap_coord_map<V>(
    old: BTreeMap<Vec<u32>, V>,
    d: usize,
    to_new: &[u32],
) -> BTreeMap<Vec<u32>, V> {
    let mut out = BTreeMap::new();
    for (mut coord, value) in old {
        let mapped = to_new.get(coord[d] as usize).copied().unwrap_or(u32::MAX);
        if mapped == u32::MAX {
            continue;
        }
        coord[d] = mapped;
        out.insert(coord, value);
    }
    out
}

/// A complete durable model: a cube plus its named subsets, views, rules, rule
/// tests, and per-user sandboxes.
///
/// Subsets are keyed by `(dimension, name)` (a subset name is unique within its
/// dimension); views and tests are keyed by name (unique within the cube). This
/// is the unit the store owns and persists and the snapshot serializes. Flows,
/// flow tests, connections, and jobs are no longer per-cube (ADR-0035): they live
/// in the server-global [`Automation`] model.
#[derive(Clone, Debug)]
pub struct Model {
    /// The cube and its dimensions.
    pub cube: Cube,
    /// Named subsets, keyed by `(dimension, name)`.
    pub subsets: BTreeMap<(String, String), Subset>,
    /// Named views, keyed by name.
    pub views: BTreeMap<String, View>,
    /// The cube's calculation rules (opaque source text).
    pub rules: RuleSet,
    /// Rule unit tests, keyed by name.
    pub tests: BTreeMap<String, RuleTest>,
    /// Per-user what-if sandboxes, keyed by name (ADR-0014).
    pub sandboxes: BTreeMap<String, Sandbox>,
}

impl Model {
    /// A model wrapping `cube` with no subsets, views, rules, tests, or sandboxes.
    pub fn new(cube: Cube) -> Self {
        Self {
            cube,
            subsets: BTreeMap::new(),
            views: BTreeMap::new(),
            rules: RuleSet::default(),
            tests: BTreeMap::new(),
            sandboxes: BTreeMap::new(),
        }
    }

    /// Look up a sandbox by name.
    pub fn sandbox(&self, name: &str) -> Option<&Sandbox> {
        self.sandboxes.get(name)
    }

    // ---- structural dimension edits that also keep sandboxes aligned ----
    //
    // Sandbox overrides are keyed by element index (like the cube's own cells), so
    // a reorder/insert/delete that shifts indices must remap them too. The cube
    // op only remaps the cube's cells; these wrappers run the same op on the cube
    // and then remap every sandbox by reconstructing the index permutation from
    // member names (members are uniquely named, and the edits preserve name order
    // apart from the change). Index-stable edits (reparent/add-child/set-kind) need
    // no remap, so they stay on `cube` directly.

    /// Reorder a dimension's members (see [`Cube::reorder_elements`]) and remap
    /// every sandbox override so each what-if value follows its member.
    pub fn reorder_elements(
        &mut self,
        dimension: &str,
        new_order: &[String],
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension);
        let old_names = d.map(|d| self.member_names(d));
        self.cube.reorder_elements(dimension, new_order)?;
        if let (Some(d), Some(old_names)) = (d, old_names) {
            let to_new = self.permutation_by_name(d, &old_names);
            self.remap_sandboxes_for_dimension(d, &to_new);
        }
        Ok(())
    }

    /// Delete a member (see [`Cube::delete_element`]) and drop or remap sandbox
    /// overrides: overrides on the deleted member are discarded, the rest follow
    /// their member to its new index.
    pub fn delete_element(&mut self, dimension: &str, element: &str) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension);
        let old_names = d.map(|d| self.member_names(d));
        self.cube.delete_element(dimension, element)?;
        if let (Some(d), Some(old_names)) = (d, old_names) {
            let to_new = self.permutation_by_name(d, &old_names);
            self.remap_sandboxes_for_dimension(d, &to_new);
        }
        Ok(())
    }

    /// Insert a member at a position (see [`Cube::insert_element_at`]) and remap
    /// sandbox overrides so existing members' what-if values follow their shifted
    /// indices.
    pub fn insert_element_at(
        &mut self,
        dimension: &str,
        name: &str,
        kind: ElementKind,
        position: Position,
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension);
        let old_names = d.map(|d| self.member_names(d));
        self.cube
            .insert_element_at(dimension, name, kind, position)?;
        if let (Some(d), Some(old_names)) = (d, old_names) {
            let to_new = self.permutation_by_name(d, &old_names);
            self.remap_sandboxes_for_dimension(d, &to_new);
        }
        Ok(())
    }

    /// The index of a dimension by name, or `None` if there is no such dimension.
    fn dimension_index(&self, dimension: &str) -> Option<usize> {
        self.cube
            .dimensions()
            .iter()
            .position(|dim| dim.name() == dimension)
    }

    /// The member names of dimension `d` in index order, used to reconstruct the
    /// index permutation an edit applied.
    fn member_names(&self, d: usize) -> Vec<String> {
        let dim = self.cube.dimension(d);
        (0..dim.len())
            .map(|i| {
                dim.element(i)
                    .expect("index below len resolves")
                    .name
                    .clone()
            })
            .collect()
    }

    /// Map each old element index of dimension `d` to its new index after an edit
    /// (matching members by name), or `u32::MAX` for a member the edit removed.
    fn permutation_by_name(&self, d: usize, old_names: &[String]) -> Vec<u32> {
        let dim = self.cube.dimension(d);
        old_names
            .iter()
            .map(|name| dim.index_of(name).unwrap_or(u32::MAX))
            .collect()
    }

    /// Remap every sandbox's overrides for a structural edit to dimension `d`.
    fn remap_sandboxes_for_dimension(&mut self, d: usize, to_new: &[u32]) {
        for sandbox in self.sandboxes.values_mut() {
            sandbox.cells = remap_coord_map(std::mem::take(&mut sandbox.cells), d, to_new);
            sandbox.string_cells =
                remap_coord_map(std::mem::take(&mut sandbox.string_cells), d, to_new);
        }
    }

    /// Look up a subset by dimension and name.
    pub fn subset(&self, dimension: &str, name: &str) -> Option<&Subset> {
        self.subsets.get(&(dimension.to_string(), name.to_string()))
    }

    /// Look up a view by name.
    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.get(name)
    }

    /// Execute one of this model's views. Cell values are read through `cells`
    /// (a [`StoredCells`] for plain reads, a rule-aware resolver for calc);
    /// `eval` resolves dynamic (MDX) subsets.
    pub fn execute(
        &self,
        view: &View,
        cells: &(dyn CellResolver + Sync),
        eval: &dyn SetEvaluator,
        mask: Option<&crate::ElementMask>,
    ) -> Result<Cellset, QueryError> {
        execute_view(
            &self.cube,
            view,
            cells,
            &|d, n| self.subset(d, n),
            eval,
            mask,
        )
    }
}

/// The server-global automation model (ADR-0035): flows, flow tests, jobs
/// (schedules), and connections, owned by no cube. Persisted as its own
/// model-as-code file (`{data_dir}/automation/automation.model`) and loaded at
/// boot, separate from the cube engine. A flow's body names the cubes and
/// dimensions it acts on; a connection is referenced by flows by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Automation {
    /// Named flows (TypeScript ETL/automation), keyed by name.
    pub flows: BTreeMap<String, Flow>,
    /// Flow unit tests, keyed by name.
    pub flow_tests: BTreeMap<String, FlowTest>,
    /// Named global data-source connections (admin-defined), keyed by name.
    pub connections: BTreeMap<String, Connection>,
    /// Scheduled jobs (ADR-0013), keyed by name.
    pub jobs: BTreeMap<String, Job>,
}

impl Automation {
    /// An empty automation model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a flow by name.
    pub fn flow(&self, name: &str) -> Option<&Flow> {
        self.flows.get(name)
    }

    /// Look up a flow test by name.
    pub fn flow_test(&self, name: &str) -> Option<&FlowTest> {
        self.flow_tests.get(name)
    }

    /// Look up a job by name.
    pub fn job(&self, name: &str) -> Option<&Job> {
        self.jobs.get(name)
    }

    /// Look up a connection by name.
    pub fn connection(&self, name: &str) -> Option<&Connection> {
        self.connections.get(name)
    }
}

/// Crossjoin per-spec member lists into tuples, first list varying slowest. An
/// empty list of lists yields one empty tuple; any empty member list yields none.
fn crossjoin(lists: &[Vec<u32>]) -> Vec<Vec<u32>> {
    let mut acc: Vec<Vec<u32>> = vec![Vec::new()];
    for list in lists {
        let mut next = Vec::with_capacity(acc.len() * list.len());
        for prefix in &acc {
            for &value in list {
                let mut tuple = prefix.clone();
                tuple.push(value);
                next.push(tuple);
            }
        }
        acc = next;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dimension;

    fn region_cube() -> Cube {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        let total = region.add_consolidated("Total");
        region.add_child(total, north, 1).unwrap();
        region.add_child(total, south, 1).unwrap();
        Cube::new("Sales", vec![region]).unwrap()
    }

    fn static_subset(members: &[&str]) -> Subset {
        Subset {
            name: "S".into(),
            dimension: "Region".into(),
            owner: None,
            visibility: Visibility::Public,
            kind: SubsetKind::Static {
                members: members.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn static_subset_preserves_author_order_and_dedups() {
        let cube = region_cube();
        let subset = static_subset(&["South", "North", "South", "Total"]);
        let resolved = resolve_subset(&cube, &subset, &NoSetEvaluator).unwrap();
        // Author order, first occurrence wins (South=1, North=0, Total=2).
        assert_eq!(resolved, vec![1, 0, 2]);
    }

    #[test]
    fn dynamic_subset_is_rejected_without_an_evaluator() {
        let cube = region_cube();
        let subset = Subset {
            name: "D".into(),
            dimension: "Region".into(),
            owner: None,
            visibility: Visibility::Public,
            kind: SubsetKind::Dynamic {
                mdx: "[Region].Members".into(),
            },
        };
        assert_eq!(
            resolve_subset(&cube, &subset, &NoSetEvaluator),
            Err(QueryError::DynamicUnsupported)
        );
    }

    #[test]
    fn unknown_dimension_is_reported() {
        let cube = region_cube();
        let subset = Subset {
            name: "S".into(),
            dimension: "Nope".into(),
            owner: None,
            visibility: Visibility::Public,
            kind: SubsetKind::Static {
                members: vec!["North".into()],
            },
        };
        assert!(matches!(
            resolve_subset(&cube, &subset, &NoSetEvaluator),
            Err(QueryError::UnknownDimension { .. })
        ));
    }

    #[test]
    fn unknown_member_is_reported() {
        let cube = region_cube();
        let subset = static_subset(&["North", "Atlantis"]);
        assert_eq!(
            resolve_subset(&cube, &subset, &NoSetEvaluator),
            Err(QueryError::UnknownMember {
                dimension: "Region".into(),
                member: "Atlantis".into()
            })
        );
    }

    #[test]
    fn resolution_is_deterministic() {
        let cube = region_cube();
        let subset = static_subset(&["Total", "North", "South"]);
        let a = resolve_subset(&cube, &subset, &NoSetEvaluator).unwrap();
        let b = resolve_subset(&cube, &subset, &NoSetEvaluator).unwrap();
        assert_eq!(a, b);
    }

    // --- 3D: views and cellsets ---

    /// Region(North,South,Total=N+S) x Product(Widget,Gadget,All=W+G) x
    /// Measure(Sales,Cost,Margin=Sales-Cost), with leaf values set so one
    /// row is all-zero and another is partially zero.
    fn sales_cube() -> Cube {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        let r_total = region.add_consolidated("Total");
        region.add_child(r_total, north, 1).unwrap();
        region.add_child(r_total, south, 1).unwrap();

        let mut product = Dimension::new("Product");
        let widget = product.add_leaf("Widget");
        let gadget = product.add_leaf("Gadget");
        let p_all = product.add_consolidated("All");
        product.add_child(p_all, widget, 1).unwrap();
        product.add_child(p_all, gadget, 1).unwrap();

        let mut measure = Dimension::new("Measure");
        let sales = measure.add_leaf("Sales");
        let cost = measure.add_leaf("Cost");
        let margin = measure.add_consolidated("Margin");
        measure.add_child(margin, sales, 1).unwrap();
        measure.add_child(margin, cost, -1).unwrap();

        let mut cube = Cube::new("Sales", vec![region, product, measure]).unwrap();
        // coord order: [Region, Product, Measure]; leaves only.
        let set = |c: &mut Cube, r, p, m, v: i32| {
            c.set_leaf(&[r, p, m], Fixed::from(v)).unwrap();
        };
        // North/Widget: Sales 100, Cost 60  (-> Margin 40)
        set(&mut cube, north, widget, sales, 100);
        set(&mut cube, north, widget, cost, 60);
        // North/Gadget: all zero (left unset)
        // South/Widget: Sales 200, Cost 150 (-> Margin 50)
        set(&mut cube, south, widget, sales, 200);
        set(&mut cube, south, widget, cost, 150);
        // South/Gadget: Sales 50, Cost 50  (-> Margin 0): partially zero
        set(&mut cube, south, gadget, sales, 50);
        set(&mut cube, south, gadget, cost, 50);
        cube
    }

    fn members(dimension: &str, names: &[&str]) -> AxisSpec {
        AxisSpec::Members {
            dimension: dimension.into(),
            members: names.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn view(rows: Axis, columns: Axis, context: &[(&str, &str)], suppress: bool) -> View {
        View {
            name: "V".into(),
            cube: "Sales".into(),
            owner: None,
            visibility: Visibility::Public,
            rows,
            columns,
            context: context
                .iter()
                .map(|(d, m)| (d.to_string(), m.to_string()))
                .collect(),
            suppress_zeros: suppress,
        }
    }

    fn no_subsets(_: &str, _: &str) -> Option<&'static Subset> {
        None
    }

    fn fixed(values: &[i32]) -> Vec<Fixed> {
        values.iter().map(|&v| Fixed::from(v)).collect()
    }

    #[test]
    fn flat_view_values_and_order() {
        let cube = sales_cube();
        let v = view(
            vec![members("Region", &["North", "South", "Total"])],
            vec![members("Measure", &["Sales", "Cost", "Margin"])],
            &[("Product", "Widget")],
            false,
        );
        let cs = execute_view(
            &cube,
            &v,
            &StoredCells(&cube),
            &no_subsets,
            &NoSetEvaluator,
            None,
        )
        .unwrap();
        assert_eq!(
            cs.row_tuples,
            vec![vec!["North"], vec!["South"], vec!["Total"]]
        );
        assert_eq!(
            cs.column_tuples,
            vec![vec!["Sales"], vec!["Cost"], vec!["Margin"]]
        );
        // North/Widget 100,60,40 ; South/Widget 200,150,50 ; Total/Widget 300,210,90
        assert_eq!(cs.cells, fixed(&[100, 60, 40, 200, 150, 50, 300, 210, 90]));
        // Determinism.
        assert_eq!(
            execute_view(
                &cube,
                &v,
                &StoredCells(&cube),
                &no_subsets,
                &NoSetEvaluator,
                None
            )
            .unwrap(),
            cs
        );
    }

    #[test]
    fn parallel_aggregation_matches_serial_bit_for_bit() {
        // A grid with consolidations on both axes (rollups, so the parallel path
        // does real aggregation work), tested with and without zero-suppression.
        let cube = sales_cube();
        for suppress in [false, true] {
            let v = view(
                vec![
                    members("Region", &["North", "South", "Total"]),
                    members("Product", &["Widget", "Gadget", "All"]),
                ],
                vec![members("Measure", &["Sales", "Cost", "Margin"])],
                &[],
                suppress,
            );
            let serial = execute_view_with(
                &cube,
                &v,
                &StoredCells(&cube),
                &no_subsets,
                &NoSetEvaluator,
                None,
                Parallelism::serial(),
            )
            .unwrap();
            // Non-power-of-two worker counts expose row-band boundary bugs.
            for workers in [2usize, 3, 5, 7] {
                let par = execute_view_with(
                    &cube,
                    &v,
                    &StoredCells(&cube),
                    &no_subsets,
                    &NoSetEvaluator,
                    None,
                    Parallelism::forced(workers),
                )
                .unwrap();
                assert_eq!(
                    par, serial,
                    "parallel ({workers} workers, suppress={suppress}) must equal serial"
                );
            }
            // Repeat-run stability: many runs at a fixed worker count are identical
            // (guards against a latent shared-state race a single compare may miss).
            for _ in 0..25 {
                let again = execute_view_with(
                    &cube,
                    &v,
                    &StoredCells(&cube),
                    &no_subsets,
                    &NoSetEvaluator,
                    None,
                    Parallelism::forced(4),
                )
                .unwrap();
                assert_eq!(again, serial, "repeat parallel run drifted");
            }
        }
    }

    #[test]
    fn nested_rows_are_outer_major() {
        let cube = sales_cube();
        let v = view(
            vec![
                members("Region", &["North", "South"]),
                members("Product", &["Widget", "Gadget"]),
            ],
            vec![members("Measure", &["Sales"])],
            &[],
            false,
        );
        let cs = execute_view(
            &cube,
            &v,
            &StoredCells(&cube),
            &no_subsets,
            &NoSetEvaluator,
            None,
        )
        .unwrap();
        assert_eq!(cs.row_dimensions, vec!["Region", "Product"]);
        assert_eq!(
            cs.row_tuples,
            vec![
                vec!["North", "Widget"],
                vec!["North", "Gadget"],
                vec!["South", "Widget"],
                vec!["South", "Gadget"],
            ]
        );
        // Sales for each row tuple.
        assert_eq!(cs.cells, fixed(&[100, 0, 200, 50]));
    }

    #[test]
    fn zero_suppression_drops_all_zero_rows_keeps_partial() {
        let cube = sales_cube();
        let v = view(
            vec![
                members("Region", &["North", "South"]),
                members("Product", &["Widget", "Gadget"]),
            ],
            vec![members("Measure", &["Sales", "Cost", "Margin"])],
            &[],
            true,
        );
        let cs = execute_view(
            &cube,
            &v,
            &StoredCells(&cube),
            &no_subsets,
            &NoSetEvaluator,
            None,
        )
        .unwrap();
        // North/Gadget is all zero -> dropped and reported; South/Gadget is
        // partially zero (50,50,0) -> kept.
        assert_eq!(
            cs.row_tuples,
            vec![
                vec!["North", "Widget"],
                vec!["South", "Widget"],
                vec!["South", "Gadget"],
            ]
        );
        assert_eq!(cs.suppressed_row_tuples, vec![vec!["North", "Gadget"]]);
        assert_eq!(cs.cells, fixed(&[100, 60, 40, 200, 150, 50, 50, 50, 0]));
    }

    #[test]
    fn zero_suppression_drops_all_zero_columns() {
        let cube = sales_cube();
        // Row North/Sales: Widget=100, Gadget=0 -> the Gadget column is all-zero.
        let v = view(
            vec![members("Region", &["North"])],
            vec![members("Product", &["Widget", "Gadget"])],
            &[("Measure", "Sales")],
            true,
        );
        let cs = execute_view(
            &cube,
            &v,
            &StoredCells(&cube),
            &no_subsets,
            &NoSetEvaluator,
            None,
        )
        .unwrap();
        assert_eq!(cs.column_tuples, vec![vec!["Widget"]]);
        assert_eq!(cs.suppressed_column_tuples, vec![vec!["Gadget"]]);
        assert_eq!(cs.cells, fixed(&[100]));
    }

    #[test]
    fn cells_match_cube_get_for_every_tuple() {
        let cube = sales_cube();
        let v = view(
            vec![
                members("Region", &["North", "South", "Total"]),
                members("Product", &["Widget", "Gadget", "All"]),
            ],
            vec![members("Measure", &["Sales", "Cost", "Margin"])],
            &[],
            false,
        );
        let cs = execute_view(
            &cube,
            &v,
            &StoredCells(&cube),
            &no_subsets,
            &NoSetEvaluator,
            None,
        )
        .unwrap();
        let ri = |n: &str| cube.dimension(0).resolve(n).unwrap();
        let pi = |n: &str| cube.dimension(1).resolve(n).unwrap();
        let mi = |n: &str| cube.dimension(2).resolve(n).unwrap();
        let ncols = cs.column_tuples.len();
        for (r, row) in cs.row_tuples.iter().enumerate() {
            for (c, col) in cs.column_tuples.iter().enumerate() {
                let coord = [ri(&row[0]), pi(&row[1]), mi(&col[0])];
                let expected = cube.get(&coord).unwrap();
                assert_eq!(
                    cs.cells[r * ncols + c],
                    expected,
                    "mismatch at {row:?},{col:?}"
                );
            }
        }
    }

    #[test]
    fn dimension_on_two_axes_is_rejected() {
        let cube = sales_cube();
        let v = view(
            vec![members("Region", &["North"])],
            vec![members("Region", &["South"])],
            &[("Product", "Widget"), ("Measure", "Sales")],
            false,
        );
        assert!(matches!(
            execute_view(
                &cube,
                &v,
                &StoredCells(&cube),
                &no_subsets,
                &NoSetEvaluator,
                None
            ),
            Err(QueryError::DimensionCoverage { .. })
        ));
    }

    #[test]
    fn uncovered_dimension_is_rejected() {
        let cube = sales_cube();
        // Measure is left off entirely.
        let v = view(
            vec![members("Region", &["North"])],
            vec![members("Product", &["Widget"])],
            &[],
            false,
        );
        assert!(matches!(
            execute_view(
                &cube,
                &v,
                &StoredCells(&cube),
                &no_subsets,
                &NoSetEvaluator,
                None
            ),
            Err(QueryError::DimensionCoverage { .. })
        ));
    }

    #[test]
    fn unknown_subset_and_dimension_mismatch_are_reported() {
        let cube = sales_cube();
        // Reference a subset that the lookup does not know.
        let v = view(
            vec![AxisSpec::Subset {
                dimension: "Region".into(),
                subset: "Missing".into(),
            }],
            vec![members("Measure", &["Sales"])],
            &[("Product", "Widget")],
            false,
        );
        assert!(matches!(
            execute_view(
                &cube,
                &v,
                &StoredCells(&cube),
                &no_subsets,
                &NoSetEvaluator,
                None
            ),
            Err(QueryError::UnknownSubset { .. })
        ));

        // A subset whose dimension differs from the axis dimension.
        let wrong = Subset {
            name: "Wrong".into(),
            dimension: "Product".into(),
            owner: None,
            visibility: Visibility::Public,
            kind: SubsetKind::Static {
                members: vec!["Widget".into()],
            },
        };
        let lookup = |_dim: &str, _name: &str| Some(&wrong);
        let v2 = view(
            vec![AxisSpec::Subset {
                dimension: "Region".into(),
                subset: "Wrong".into(),
            }],
            vec![members("Measure", &["Sales"])],
            &[("Product", "Widget")],
            false,
        );
        assert!(matches!(
            execute_view(
                &cube,
                &v2,
                &StoredCells(&cube),
                &lookup,
                &NoSetEvaluator,
                None
            ),
            Err(QueryError::SubsetDimensionMismatch { .. })
        ));
    }

    #[test]
    fn empty_member_axis_is_a_valid_empty_cellset() {
        let cube = sales_cube();
        // An empty member list on rows -> zero row tuples, not an error.
        let v = view(
            vec![members("Region", &[])],
            vec![members("Measure", &["Sales"])],
            &[("Product", "Widget")],
            false,
        );
        let cs = execute_view(
            &cube,
            &v,
            &StoredCells(&cube),
            &no_subsets,
            &NoSetEvaluator,
            None,
        )
        .unwrap();
        assert!(cs.row_tuples.is_empty());
        assert!(cs.cells.is_empty());
        assert_eq!(cs.column_tuples, vec![vec!["Sales"]]);
    }
}
