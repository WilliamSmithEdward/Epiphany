//! Cube-detail and cell endpoints (name-addressed, clean JSON). Writes funnel
//! through the engine's atomic batch commit; reads are consolidation-aware on a
//! lock-free snapshot.

use std::str::FromStr;

use axum::extract::{Path, State};
use axum::Json;

use epiphany_core::{CellResolver, Cube, Fixed};
use epiphany_engine::{BatchError, CellWrite};

use crate::auth::AuthPrincipal;
use crate::dto::{
    BatchWriteRequest, BatchWriteResponse, CellDto, CoordMap, CubeDetailDto, DimensionDto, EdgeDto,
    ElementDto, ReadCellsRequest, ReadCellsResponse, WriteCellRequest,
};
use crate::resolve::{kind_str, resolve, Resolved};
use crate::sandbox_routes::{resolve_sandbox, SandboxSelector};
use crate::ws::ChangeEvent;
use crate::{ApiError, AppState};

/// `GET /api/v1/cubes/{cube}` -> the cube with its dimensions and elements.
pub(crate) async fn get_cube(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<CubeDetailDto>, ApiError> {
    let snap = state
        .engine
        .snapshot(&cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))?;
    Ok(Json(cube_detail(snap.cube())))
}

fn cube_detail(cube: &Cube) -> CubeDetailDto {
    let dimensions = cube
        .dimensions()
        .iter()
        .map(|dim| DimensionDto {
            name: dim.name().to_string(),
            elements: dim
                .iter_elements()
                .map(|el| ElementDto {
                    name: el.name.clone(),
                    kind: kind_str(el.kind),
                })
                .collect(),
            edges: dim
                .edges()
                .into_iter()
                .map(|(parent, child, weight)| EdgeDto {
                    parent: dim.element(parent).expect("valid index").name.clone(),
                    child: dim.element(child).expect("valid index").name.clone(),
                    weight,
                })
                .collect(),
        })
        .collect();
    CubeDetailDto {
        name: cube.name().to_string(),
        dimensions,
    }
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
    let snap = state
        .engine
        .snapshot(&cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))?;
    let cube_ref = snap.cube();
    // An active sandbox (X-Epiphany-Sandbox) overlays its what-if leaves beneath
    // the rules, so values recompute over them (ADR-0014); absent it, base.
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let sandbox = sandbox_name.as_deref().and_then(|n| snap.model().sandbox(n));
    // Values come through the injected resolver (rule-aware in the server,
    // stored-only in no-rules deployments and tests).
    let resolver = state.cells.resolver_with(&snap, sandbox);
    let mut cells = Vec::with_capacity(req.coords.len());
    for coord in &req.coords {
        let resolved = resolve(cube_ref, coord)?;
        // A cell is "overlaid" only when this exact leaf is a what-if override
        // (a consolidation that merely rolled one up is not flagged).
        let overlaid = sandbox.is_some_and(|sb| {
            sb.cells.contains_key(&resolved.indices)
                || sb.string_cells.contains_key(&resolved.indices)
        });
        cells.push(read_one(&*resolver, coord, &resolved, overlaid)?);
    }
    Ok(Json(ReadCellsResponse { cells }))
}

/// `PUT /api/v1/cubes/{cube}/cell` -> write one leaf cell, return its new value.
pub(crate) async fn write_cell(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(req): Json<WriteCellRequest>,
) -> Result<Json<CellDto>, ApiError> {
    let write = {
        let snap = state
            .engine
            .snapshot(&cube)
            .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))?;
        build_write(snap.cube(), &req.coord, &req.value)?
    };
    let outcome = state
        .engine
        .apply_batch(&cube, None, &[write])
        .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::CellsChanged {
        cube: cube.clone(),
        version: outcome.version,
        coords: vec![req.coord.clone()],
    });
    // Re-read on a fresh snapshot so the caller sees the committed value.
    let snap = state
        .engine
        .snapshot(&cube)
        .ok_or_else(ApiError::internal)?;
    let resolver = state.cells.resolver(&snap);
    let resolved = resolve(snap.cube(), &req.coord)?;
    Ok(Json(read_one(&*resolver, &req.coord, &resolved, false)?))
}

/// `POST /api/v1/cubes/{cube}/cells/batch` -> apply all writes or none.
pub(crate) async fn batch_write(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(req): Json<BatchWriteRequest>,
) -> Result<Json<BatchWriteResponse>, ApiError> {
    let writes = {
        let snap = state
            .engine
            .snapshot(&cube)
            .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))?;
        let cube_ref = snap.cube();
        req.writes
            .iter()
            .map(|item| build_write(cube_ref, &item.coord, &item.value))
            .collect::<Result<Vec<_>, _>>()?
    };
    let outcome = state
        .engine
        .apply_batch(&cube, req.base_version, &writes)
        .map_err(map_batch_error)?;
    let coords = req.writes.iter().map(|w| w.coord.clone()).collect();
    let _ = state.events.send(ChangeEvent::CellsChanged {
        cube: cube.clone(),
        version: outcome.version,
        coords,
    });
    Ok(Json(BatchWriteResponse {
        applied: writes.len(),
        version: outcome.version,
    }))
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
        BatchError::Persist(_) => ApiError::internal(),
    }
}
