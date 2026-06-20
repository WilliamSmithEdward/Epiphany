//! Cube-detail and cell endpoints (name-addressed, clean JSON). Writes funnel
//! through the engine's atomic batch commit; reads are consolidation-aware on a
//! lock-free snapshot.

use std::str::FromStr;

use axum::extract::{Path, State};
use axum::Json;

use epiphany_core::{
    spread_leaves, AttributeKind, AttributeValue, CellResolver, Cube, ElementMask, Fixed,
    SpreadError, SpreadMethod,
};
use epiphany_engine::{BatchError, CellWrite, ReadSnapshot};
use epiphany_security::AccessLevel;

use crate::auth::AuthPrincipal;
use crate::authz::{
    element_mask, require_cube_access, require_element_write, require_element_write_indices,
};
use crate::dto::{
    AttributeDto, AttributeValueDto, BatchWriteRequest, BatchWriteResponse, CellDto, CoordMap,
    CubeDetailDto, DimensionDto, EdgeDto, ElementDto, ReadCellsRequest, ReadCellsResponse,
    SpreadRequest, WriteCellRequest,
};
use crate::resolve::{kind_str, resolve, Resolved};
use crate::sandbox_routes::{resolve_sandbox, SandboxSelector};
use crate::ws::ChangeEvent;
use crate::{ApiError, AppState};

// ---- shared route helpers (used across the route modules) ----

/// Pin a lock-free read snapshot of a cube, or 404 if it does not exist
/// (ADR-0001). The single definition shared by every route module.
pub(crate) fn snapshot(state: &AppState, cube: &str) -> Result<ReadSnapshot, ApiError> {
    state
        .engine
        .snapshot(cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))
}

/// Broadcast that a cube's objects changed, at its current version (a no-op if
/// the cube has no version yet). Used after object edits where the caller does
/// not already hold the committed version.
pub(crate) fn broadcast(state: &AppState, cube: &str) {
    if let Some(version) = state.engine.version(cube) {
        broadcast_with_version(state, cube, version);
    }
}

/// Broadcast an objects-changed event at a specific (just-committed) version.
pub(crate) fn broadcast_with_version(state: &AppState, cube: &str, version: u64) {
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.to_string(),
        version,
    });
}

/// `GET /api/v1/cubes/{cube}` -> the cube with its dimensions and elements.
pub(crate) async fn get_cube(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<CubeDetailDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    // Element security (ADR-0015): a non-admin must not learn the names of
    // elements denied to them, so denied members (and any edge touching them) are
    // suppressed from the cube structure, just as from a member enumeration.
    let mask = element_mask(&state, &auth, &snap);
    // The global dimension id (ADR-0024/0031) when a dimension is registry-backed,
    // so the web can present one global dimension list and route edits correctly.
    // Resolved in one registry pass, then a cheap per-dimension map lookup.
    let backings = state.engine.dimension_backings(&cube);
    let backing = |dim_name: &str| backings.get(dim_name).map(|id| id.0);
    Ok(Json(cube_detail(snap.cube(), mask.as_ref(), backing)))
}

fn cube_detail(
    cube: &Cube,
    mask: Option<&ElementMask>,
    backing: impl Fn(&str) -> Option<u64>,
) -> CubeDetailDto {
    let dimensions = cube
        .dimensions()
        .iter()
        .enumerate()
        .map(|(d, dim)| {
            let denied = |idx: u32| mask.is_some_and(|m| m.denies_member(cube, d, idx));
            DimensionDto {
                name: dim.name().to_string(),
                id: backing(dim.name()),
                elements: dim
                    .iter_elements()
                    .enumerate()
                    .filter(|(i, _)| !denied(*i as u32))
                    .map(|(_, el)| ElementDto {
                        name: el.name.clone(),
                        kind: kind_str(el.kind),
                        pinned_to_top: el.pinned_to_top,
                    })
                    .collect(),
                edges: dim
                    .edges()
                    .into_iter()
                    .filter(|(parent, child, _)| !denied(*parent) && !denied(*child))
                    .map(|(parent, child, weight)| EdgeDto {
                        parent: dim.element(parent).expect("valid index").name.clone(),
                        child: dim.element(child).expect("valid index").name.clone(),
                        weight,
                    })
                    .collect(),
                attributes: dimension_attributes(dim, &denied),
            }
        })
        .collect();
    CubeDetailDto {
        name: cube.name().to_string(),
        dimensions,
    }
}

fn attr_kind_str(kind: AttributeKind) -> &'static str {
    match kind {
        AttributeKind::Text => "text",
        AttributeKind::Numeric => "numeric",
        AttributeKind::Alias => "alias",
    }
}

/// Build the attribute DTOs for one dimension, suppressing values whose element
/// the caller may not see (element security).
fn dimension_attributes(
    dim: &epiphany_core::Dimension,
    denied: &impl Fn(u32) -> bool,
) -> Vec<AttributeDto> {
    let defs = dim.attribute_defs();
    let mut per_attr: Vec<Vec<AttributeValueDto>> = (0..defs.len()).map(|_| Vec::new()).collect();
    for (element, attr_index, value) in dim.attribute_values() {
        if denied(element) {
            continue;
        }
        let element_name = dim.element(element).expect("valid index").name.clone();
        let text = match value {
            AttributeValue::Text(t) => t,
            AttributeValue::Numeric(n) => n.to_string(),
        };
        per_attr[attr_index as usize].push(AttributeValueDto {
            element: element_name,
            value: text,
        });
    }
    defs.iter()
        .zip(per_attr)
        .map(|(def, values)| AttributeDto {
            name: def.name.clone(),
            kind: attr_kind_str(def.kind),
            values,
        })
        .collect()
}

/// `POST /api/v1/cubes/{cube}/cells/read` -> values for a set of coordinates
/// (consolidation-aware).
pub(crate) async fn read_cells(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(req): Json<ReadCellsRequest>,
) -> Result<Json<ReadCellsResponse>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let cube_ref = snap.cube();
    // An active sandbox (X-Epiphany-Sandbox) overlays its what-if leaves beneath
    // the rules, so values recompute over them (ADR-0014); absent it, base.
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    // Values come through the injected resolver (rule-aware in the server,
    // stored-only in no-rules deployments and tests), carrying the caller's
    // element deny mask (ADR-0015): a directly-addressed denied coordinate (or a
    // rollup of a denied leaf) returns 403.
    let mask = element_mask(&state, &auth, &snap);
    let resolver = state.cells.resolver_with(&snap, sandbox, mask.as_ref());
    let mut cells = Vec::with_capacity(req.coords.len());
    for coord in &req.coords {
        let resolved = resolve(cube_ref, coord)?;
        // A cell is "overlaid" only when this exact leaf is a what-if override
        // (a consolidation that merely rolled one up is not flagged). Overrides
        // are numeric this phase (ADR-0014), so only `cells` is consulted.
        let overlaid = sandbox.is_some_and(|sb| sb.cells.contains_key(&resolved.indices));
        cells.push(read_one(&*resolver, coord, &resolved, overlaid)?);
    }
    Ok(Json(ReadCellsResponse { cells }))
}

/// `PUT /api/v1/cubes/{cube}/cell` -> write one leaf cell, return its new value.
pub(crate) async fn write_cell(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(req): Json<WriteCellRequest>,
) -> Result<Json<CellDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    // Element security (ADR-0015): a write to a coordinate the caller may not
    // write is rejected before anything is staged.
    require_element_write(&state, &auth, &cube, &req.coord)?;
    let (write, sandbox_name) = {
        let snap = snapshot(&state, &cube)?;
        let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
        (
            build_write(snap.cube(), &req.coord, &req.value)?,
            sandbox_name,
        )
    };
    // A what-if write stages into the sandbox (base untouched); a base write
    // commits to the cube (ADR-0014).
    let outcome = match &sandbox_name {
        Some(name) => state.engine.sandbox_set_cells(&cube, None, name, &[write]),
        None => state.engine.apply_batch(&cube, None, &[write]),
    }
    .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::CellsChanged {
        cube: cube.clone(),
        version: outcome.version,
        coords: vec![req.coord.clone()],
        sandbox: sandbox_name.clone(),
        owner: sandbox_name
            .as_ref()
            .map(|_| auth.principal.username.clone()),
    });
    // Re-read on a fresh snapshot, overlaying the sandbox so the caller sees the
    // staged what-if value (flagged overlaid).
    let snap = state
        .engine
        .snapshot(&cube)
        .ok_or_else(ApiError::internal)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    let mask = element_mask(&state, &auth, &snap);
    let resolver = state.cells.resolver_with(&snap, sandbox, mask.as_ref());
    let resolved = resolve(snap.cube(), &req.coord)?;
    let overlaid = sandbox.is_some_and(|sb| sb.cells.contains_key(&resolved.indices));
    Ok(Json(read_one(&*resolver, &req.coord, &resolved, overlaid)?))
}

/// `POST /api/v1/cubes/{cube}/cells/batch` -> apply all writes or none.
pub(crate) async fn batch_write(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(req): Json<BatchWriteRequest>,
) -> Result<Json<BatchWriteResponse>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    // Element security (ADR-0015): if any write targets a coordinate the caller
    // may not write, the whole batch is rejected before anything is staged.
    for w in &req.writes {
        require_element_write(&state, &auth, &cube, &w.coord)?;
    }
    let (writes, sandbox_name) = {
        let snap = snapshot(&state, &cube)?;
        let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
        let cube_ref = snap.cube();
        let writes = req
            .writes
            .iter()
            .map(|item| build_write(cube_ref, &item.coord, &item.value))
            .collect::<Result<Vec<_>, _>>()?;
        (writes, sandbox_name)
    };
    // A what-if batch stages into the sandbox (base untouched); the base-version
    // check applies only to base commits.
    let outcome = match &sandbox_name {
        Some(name) => state.engine.sandbox_set_cells(&cube, None, name, &writes),
        None => state.engine.apply_batch(&cube, req.base_version, &writes),
    }
    .map_err(map_batch_error)?;
    let coords = req.writes.iter().map(|w| w.coord.clone()).collect();
    let _ = state.events.send(ChangeEvent::CellsChanged {
        cube: cube.clone(),
        version: outcome.version,
        coords,
        sandbox: sandbox_name.clone(),
        owner: sandbox_name
            .as_ref()
            .map(|_| auth.principal.username.clone()),
    });
    Ok(Json(BatchWriteResponse {
        applied: writes.len(),
        version: outcome.version,
    }))
}

/// `POST /api/v1/cubes/{cube}/cells/spread` -> distribute a value entered at a
/// (possibly consolidated) coordinate across its leaves (ADR-0029).
pub(crate) async fn spread_cells(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(req): Json<SpreadRequest>,
) -> Result<Json<BatchWriteResponse>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    let method = parse_spread_method(&req.method)?;
    let value = Fixed::from_str(&req.value).map_err(|_| {
        ApiError::unprocessable("INVALID_NUMBER", format!("invalid number '{}'", req.value))
    })?;

    let snap = snapshot(&state, &cube)?;
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let resolved = resolve(snap.cube(), &req.target)?;
    if resolved.has_string {
        return Err(ApiError::unprocessable(
            "SPREAD_TO_STRING",
            "cannot spread into a string cell",
        ));
    }

    // Expand the target into leaf writes, reading current values (for the
    // proportional basis) through a resolver that honors the sandbox + mask.
    let writes = {
        let sandbox = sandbox_name
            .as_deref()
            .and_then(|n| snap.model().sandbox(n));
        let mask = element_mask(&state, &auth, &snap);
        let resolver = state.cells.resolver_with(&snap, sandbox, mask.as_ref());
        spread_leaves(snap.cube(), &resolved.indices, value, method, &|c| {
            resolver.value(c)
        })
        .map_err(map_spread_error)?
    };
    if writes.is_empty() {
        return Ok(Json(BatchWriteResponse {
            applied: 0,
            version: snap.version(),
        }));
    }

    // Element security (ADR-0015): fail-closed. If any contributing leaf is not
    // writable, the whole spread is denied before anything is staged.
    let coords: Vec<Vec<u32>> = writes.iter().map(|(c, _)| c.clone()).collect();
    require_element_write_indices(&state, &auth, &cube, &snap, &coords)?;

    let batch: Vec<CellWrite> = writes
        .into_iter()
        .map(|(coord, value)| CellWrite::Leaf { coord, value })
        .collect();
    let outcome = match &sandbox_name {
        Some(name) => state.engine.sandbox_set_cells(&cube, None, name, &batch),
        None => state.engine.apply_batch(&cube, None, &batch),
    }
    .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::CellsChanged {
        cube: cube.clone(),
        version: outcome.version,
        coords: vec![req.target.clone()],
        sandbox: sandbox_name.clone(),
        owner: sandbox_name
            .as_ref()
            .map(|_| auth.principal.username.clone()),
    });
    Ok(Json(BatchWriteResponse {
        applied: batch.len(),
        version: outcome.version,
    }))
}

fn parse_spread_method(token: &str) -> Result<SpreadMethod, ApiError> {
    match token {
        "equal" => Ok(SpreadMethod::Equal),
        "proportional" => Ok(SpreadMethod::Proportional),
        "repeat" => Ok(SpreadMethod::Repeat),
        "clear" => Ok(SpreadMethod::Clear),
        other => Err(ApiError::bad_request(format!(
            "unknown spread method '{other}'"
        ))),
    }
}

fn map_spread_error(err: SpreadError) -> ApiError {
    match err {
        SpreadError::WeightedConsolidation => ApiError::unprocessable(
            "SPREAD_WEIGHTED",
            "cannot spread across a weighted consolidation",
        ),
        SpreadError::TooManyLeaves { count, cap } => ApiError::unprocessable(
            "SPREAD_TOO_LARGE",
            format!("the target expands to {count} cells, over the limit of {cap}"),
        ),
        SpreadError::Read(query_error) => ApiError::from(query_error),
    }
}

fn read_one(
    cells: &dyn CellResolver,
    coord: &CoordMap,
    resolved: &Resolved,
    overlaid: bool,
) -> Result<CellDto, ApiError> {
    if resolved.has_string {
        let value = cells.string_value(&resolved.indices)?;
        Ok(CellDto {
            coord: coord.clone(),
            value,
            kind: "string",
            editable: resolved.all_leaf,
            overlaid,
        })
    } else {
        let value = cells.value(&resolved.indices)?;
        Ok(CellDto {
            coord: coord.clone(),
            value: Some(value.to_string()),
            kind: "numeric",
            editable: resolved.all_leaf,
            overlaid,
        })
    }
}

pub(crate) fn build_write(
    cube: &Cube,
    coord: &CoordMap,
    value: &str,
) -> Result<CellWrite, ApiError> {
    let resolved = resolve(cube, coord)?;
    if !resolved.all_leaf {
        return Err(ApiError::unprocessable(
            "WRITE_TO_NON_LEAF",
            "cannot write to a consolidated coordinate",
        ));
    }
    if resolved.has_string {
        Ok(CellWrite::Str {
            coord: resolved.indices,
            value: value.to_string(),
        })
    } else {
        let number = Fixed::from_str(value).map_err(|_| {
            ApiError::unprocessable("INVALID_NUMBER", format!("invalid number '{value}'"))
        })?;
        Ok(CellWrite::Leaf {
            coord: resolved.indices,
            value: number,
        })
    }
}

/// Map a persistence failure from the global automation store (ADR-0035) into a
/// 500: the cause is never serialized (RG-12), matching the cube write path.
pub(crate) fn map_persist_error(_error: epiphany_persist::PersistError) -> ApiError {
    ApiError::internal()
}

pub(crate) fn map_batch_error(error: BatchError) -> ApiError {
    match error {
        BatchError::UnknownCube(cube) => ApiError::not_found(format!("unknown cube '{cube}'")),
        BatchError::Conflict { expected, actual } => ApiError::conflict(format!(
            "stale base version {expected}; the cube is at {actual}"
        )),
        BatchError::Rejected { index, source } => ApiError::unprocessable(
            "BATCH_REJECTED",
            format!("write {index} rejected: {source}"),
        )
        .with_details(serde_json::json!({ "failed_index": index })),
        // A structurally invalid definition carries a typed QueryError with its
        // own status and code (404 for a missing object, 422 otherwise).
        BatchError::Invalid(e) => ApiError::from(e),
        BatchError::AlreadyExists(name) => {
            ApiError::conflict(format!("cube '{name}' already exists"))
        }
        BatchError::Unsupported(what) => ApiError::unprocessable("UNSUPPORTED", what),
        BatchError::Persist(_) => ApiError::internal(),
    }
}
