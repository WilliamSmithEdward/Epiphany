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

use crate::{Cube, Dimension, Fixed, ModelError};

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
        SubsetKind::Static { members } => {
            let mut out = Vec::new();
            let mut seen = HashSet::new();
            for name in members {
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
        SubsetKind::Dynamic { mdx } => eval.eval_set(cube, dim, mdx),
    }
}

/// A single placement on an axis: a saved subset, or an inline member list.
///
/// Each spec selects members from exactly one dimension. An [`Axis`] is an
/// ordered list of specs whose member lists are crossjoined to form tuples, with
/// the first spec varying slowest (outermost nesting).
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// Execute a view over a cube into a [`Cellset`].
///
/// `subset_lookup(dimension, name)` resolves a saved subset referenced by an
/// axis; `eval` resolves dynamic (MDX) subsets. The function validates exact
/// one-axis-per-dimension coverage, resolves each axis to crossjoined member
/// tuples, reads consolidation-aware values via [`Cube::get`], then applies
/// zero-suppression (rows first, then columns) preserving order. An axis or
/// suppression that yields zero tuples is a valid empty result, not an error.
pub fn execute_view<'a>(
    cube: &Cube,
    view: &View,
    subset_lookup: &dyn Fn(&str, &str) -> Option<&'a Subset>,
    eval: &dyn SetEvaluator,
) -> Result<Cellset, QueryError> {
    validate_coverage(cube, view)?;

    let (row_dimensions, row_tuples_idx) = resolve_axis(cube, &view.rows, subset_lookup, eval)?;
    let (column_dimensions, column_tuples_idx) =
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

    // Dense value matrix over all (row, column) tuples.
    let nrows = row_tuples_idx.len();
    let ncols = column_tuples_idx.len();
    let mut matrix: Vec<Vec<Fixed>> = Vec::with_capacity(nrows);
    for row in &row_tuples_idx {
        let mut line = Vec::with_capacity(ncols);
        for col in &column_tuples_idx {
            let mut coord = base_coord.clone();
            for (k, &idx) in row.iter().enumerate() {
                coord[row_ci[k]] = idx;
            }
            for (k, &idx) in col.iter().enumerate() {
                coord[col_ci[k]] = idx;
            }
            line.push(cube.get(&coord)?);
        }
        matrix.push(line);
    }

    // Zero-suppression: rows first, then columns over the surviving rows. Only
    // meaningful when both axes are non-empty (otherwise there are no cells).
    let suppress = view.suppress_zeros && nrows > 0 && ncols > 0;
    let (keep_rows, supp_rows): (Vec<usize>, Vec<usize>) = if suppress {
        (0..nrows).partition(|&r| (0..ncols).any(|c| !matrix[r][c].is_zero()))
    } else {
        ((0..nrows).collect(), Vec::new())
    };
    let suppress_cols = suppress && !keep_rows.is_empty();
    let (keep_cols, supp_cols): (Vec<usize>, Vec<usize>) = if suppress_cols {
        (0..ncols).partition(|&c| keep_rows.iter().any(|&r| !matrix[r][c].is_zero()))
    } else {
        ((0..ncols).collect(), Vec::new())
    };

    let mut cells = Vec::with_capacity(keep_rows.len() * keep_cols.len());
    for &r in &keep_rows {
        for &c in &keep_cols {
            cells.push(matrix[r][c]);
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
                let mut out = Vec::new();
                let mut seen = HashSet::new();
                for m in members {
                    let idx = dim.resolve(m).ok_or_else(|| QueryError::UnknownMember {
                        dimension: dim.name().to_string(),
                        member: m.clone(),
                    })?;
                    if seen.insert(idx) {
                        out.push(idx);
                    }
                }
                out
            }
        };
        dimensions.push(spec.dimension().to_string());
        per_spec.push(indices);
    }
    Ok((dimensions, crossjoin(&per_spec)))
}

/// A complete durable model: a cube plus its named subsets and views.
///
/// Subsets are keyed by `(dimension, name)` (a subset name is unique within its
/// dimension); views are keyed by name (unique within the cube). This is the
/// unit the store owns and persists (Phase 3F) and the snapshot serializes.
#[derive(Clone, Debug)]
pub struct Model {
    /// The cube and its dimensions.
    pub cube: Cube,
    /// Named subsets, keyed by `(dimension, name)`.
    pub subsets: BTreeMap<(String, String), Subset>,
    /// Named views, keyed by name.
    pub views: BTreeMap<String, View>,
}

impl Model {
    /// A model wrapping `cube` with no subsets or views.
    pub fn new(cube: Cube) -> Self {
        Self {
            cube,
            subsets: BTreeMap::new(),
            views: BTreeMap::new(),
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

    /// Execute one of this model's views, resolving its subsets and (via `eval`)
    /// any dynamic subsets.
    pub fn execute(&self, view: &View, eval: &dyn SetEvaluator) -> Result<Cellset, QueryError> {
        execute_view(&self.cube, view, &|d, n| self.subset(d, n), eval)
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
        let cs = execute_view(&cube, &v, &no_subsets, &NoSetEvaluator).unwrap();
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
            execute_view(&cube, &v, &no_subsets, &NoSetEvaluator).unwrap(),
            cs
        );
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
        let cs = execute_view(&cube, &v, &no_subsets, &NoSetEvaluator).unwrap();
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
        let cs = execute_view(&cube, &v, &no_subsets, &NoSetEvaluator).unwrap();
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
        let cs = execute_view(&cube, &v, &no_subsets, &NoSetEvaluator).unwrap();
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
        let cs = execute_view(&cube, &v, &no_subsets, &NoSetEvaluator).unwrap();
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
            execute_view(&cube, &v, &no_subsets, &NoSetEvaluator),
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
            execute_view(&cube, &v, &no_subsets, &NoSetEvaluator),
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
            execute_view(&cube, &v, &no_subsets, &NoSetEvaluator),
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
            execute_view(&cube, &v2, &lookup, &NoSetEvaluator),
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
        let cs = execute_view(&cube, &v, &no_subsets, &NoSetEvaluator).unwrap();
        assert!(cs.row_tuples.is_empty());
        assert!(cs.cells.is_empty());
        assert_eq!(cs.column_tuples, vec![vec!["Sales"]]);
    }
}
