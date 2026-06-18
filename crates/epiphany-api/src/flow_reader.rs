//! The live, principal-masked read view a flow run reads through (ADR-0035).
//!
//! A flow body may read live model state (`ctx.cube(name).readCell(...)`,
//! `.members(...)`, `.property(...)`, `ctx.dimension(name).members()`). The pure
//! runner has no engine access, so the API injects an [`ApiFlowReader`] that
//! resolves those reads against pinned per-cube snapshots, masked by the run
//! principal's element security exactly as a direct read would be. Each cube's
//! snapshot, deny mask, and resolver are captured once on first read and cached
//! for the run, so reads are deterministic within a run and never expose masked
//! cells (fail-closed). The reader owns its `AppState` clone and the run
//! principal's username, so it is `'static`; a run is single-threaded.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;

use epiphany_core::{CellResolver, ElementMask, QueryError};
use epiphany_engine::ReadSnapshot;
use epiphany_flow::{FlowCell, FlowReadError, FlowReader};

use crate::authz::{element_mask_for, is_admin};
use crate::dto::CoordMap;
use crate::resolve::resolve;
use crate::AppState;

/// Per-cube captured read context: the pinned snapshot, the run principal's deny
/// mask over it, and a resolver bound to both.
struct CubeView {
    snapshot: ReadSnapshot,
    mask: Option<ElementMask>,
    resolver: Box<dyn CellResolver + Sync>,
}

/// A [`FlowReader`] over an [`AppState`] for one run principal (by username). All
/// reads are masked for that principal; per-cube views are captured lazily and
/// cached for the run.
pub(crate) struct ApiFlowReader {
    state: AppState,
    /// The run principal's username: a manual run uses the caller, a scheduled run
    /// the flow's owner (ADR-0035).
    username: String,
    /// Lazily-captured per-cube views, keyed by cube name. `RefCell` because the
    /// `FlowReader` trait takes `&self` and a run is single-threaded.
    cache: RefCell<HashMap<String, CubeView>>,
}

impl ApiFlowReader {
    /// A reader that serves reads as `username`.
    pub(crate) fn new(state: AppState, username: impl Into<String>) -> Self {
        Self {
            state,
            username: username.into(),
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Run `f` with the cached view for `cube`, capturing it on first use. Returns
    /// `UnknownCube` if the cube does not exist.
    fn with_view<R>(
        &self,
        cube: &str,
        f: impl FnOnce(&CubeView) -> Result<R, FlowReadError>,
    ) -> Result<R, FlowReadError> {
        if !self.cache.borrow().contains_key(cube) {
            let snapshot = self
                .state
                .engine
                .snapshot(cube)
                .ok_or_else(|| FlowReadError::UnknownCube(cube.to_string()))?;
            let mask = element_mask_for(&self.state, &self.username, &snapshot);
            let resolver = self
                .state
                .cells
                .resolver_with(&snapshot, None, mask.as_ref());
            self.cache.borrow_mut().insert(
                cube.to_string(),
                CubeView {
                    snapshot,
                    mask,
                    resolver,
                },
            );
        }
        let cache = self.cache.borrow();
        let view = cache.get(cube).expect("view just inserted");
        f(view)
    }
}

/// Map a value-resolver error into a flow read error (fail-closed): denied stays
/// denied; anything else is an invalid coordinate.
fn map_query_error(err: QueryError) -> FlowReadError {
    match err {
        QueryError::AccessDenied => FlowReadError::AccessDenied,
        other => FlowReadError::Invalid(other.to_string()),
    }
}

impl FlowReader for ApiFlowReader {
    fn read_cell(
        &self,
        cube: &str,
        coord: &BTreeMap<String, String>,
    ) -> Result<FlowCell, FlowReadError> {
        self.with_view(cube, |view| {
            let coord: &CoordMap = coord;
            let resolved = resolve(view.snapshot.cube(), coord)
                .map_err(|e| FlowReadError::Invalid(e.message().to_string()))?;
            // A string cell carries text; a numeric cell carries an exact decimal.
            if resolved.has_string {
                let text = view
                    .resolver
                    .string_value(&resolved.indices)
                    .map_err(map_query_error)?;
                Ok(FlowCell {
                    numeric: None,
                    text,
                })
            } else {
                let value = view
                    .resolver
                    .value(&resolved.indices)
                    .map_err(map_query_error)?;
                Ok(FlowCell {
                    numeric: Some(value.to_string()),
                    text: None,
                })
            }
        })
    }

    fn cube_members(&self, cube: &str, dimension: &str) -> Result<Vec<String>, FlowReadError> {
        self.with_view(cube, |view| {
            let cube_ref = view.snapshot.cube();
            // Find the dimension by name, scanning the cube's rank.
            let dim_index = (0..cube_ref.rank())
                .find(|&d| cube_ref.dimension(d).name() == dimension)
                .ok_or_else(|| {
                    FlowReadError::Invalid(format!("cube '{cube}' has no dimension '{dimension}'"))
                })?;
            let dim = cube_ref.dimension(dim_index);
            // Suppress any member the run principal may not see (element security).
            let members = dim
                .iter_elements()
                .enumerate()
                .filter(|(idx, _)| {
                    view.mask
                        .as_ref()
                        .map(|m| !m.denies_member(cube_ref, dim_index, *idx as u32))
                        .unwrap_or(true)
                })
                .map(|(_, el)| el.name.clone())
                .collect();
            Ok(members)
        })
    }

    fn dimension_members(&self, dimension: &str) -> Result<Vec<String>, FlowReadError> {
        // Global-dimension element masking is out of scope for the flow reader
        // (ADR-0035): an element-restricted principal could otherwise observe a
        // denied member through a global read, so a non-admin is denied here
        // (fail-closed). An admin sees the full member list.
        if !is_admin(&self.state, &self.username) {
            return Err(FlowReadError::AccessDenied);
        }
        self.state
            .engine
            .registry_dimension_members(dimension)
            .ok_or_else(|| {
                FlowReadError::Invalid(format!("unknown global dimension '{dimension}'"))
            })
    }

    fn cube_property(&self, cube: &str, key: &str) -> Result<Option<String>, FlowReadError> {
        self.with_view(cube, |view| {
            match key {
                // The rules text is the one cube-level property a flow may read for
                // now; an empty rule set reads as absent.
                "rules" => {
                    let rules = view.snapshot.model().rules.source.clone();
                    Ok((!rules.is_empty()).then_some(rules))
                }
                // No other cube property is exposed yet.
                _ => Ok(None),
            }
        })
    }
}
