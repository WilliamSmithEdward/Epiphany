//! Cube-detail and cell endpoints (name-addressed, clean JSON). Writes funnel
//! through the engine's atomic batch commit; reads are consolidation-aware on a
//! lock-free snapshot.

use std::str::FromStr;

use axum::extract::{Path, State};
use axum::Json;

use epiphany_core::{Cube, Fixed};
use epiphany_engine::{BatchError, CellWrite};

use crate::auth::AuthPrincipal;
use crate::dto::{
    BatchWriteRequest, BatchWriteResponse, CellDto, CoordMap, CubeDetailDto, DimensionDto, EdgeDto,
    ElementDto, ReadCellsRequest, ReadCellsResponse, WriteCellRequest,
};
use crate::resolve::{kind_str, resolve, Resolved};
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
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(req): Json<ReadCellsRequest>,
) -> Result<Json<ReadCellsResponse>, ApiError> {
    let snap = state
        .engine
        .snapshot(&cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))?;
    let cube_ref = snap.cube();
    let mut cells = Vec::with_capacity(req.coords.len());
    for coord in &req.coords {
        let resolved = resolve(cube_ref, coord)?;
        cells.push(read_one(cube_ref, coord, &resolved)?);
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
    state
        .engine
        .apply_batch(&cube, None, &[write])
        .map_err(map_batch_error)?;
    // Re-read on a fresh snapshot so the caller sees the committed value.
    let snap = state
        .engine
        .snapshot(&cube)
        .ok_or_else(ApiError::internal)?;
    let resolved = resolve(snap.cube(), &req.coord)?;
    Ok(Json(read_one(snap.cube(), &req.coord, &resolved)?))
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
    Ok(Json(BatchWriteResponse {
        applied: writes.len(),
        version: outcome.version,
    }))
}

fn read_one(cube: &Cube, coord: &CoordMap, resolved: &Resolved) -> Result<CellDto, ApiError> {
    if resolved.has_string {
        let value = cube
            .get_string(&resolved.indices)
            .map_err(|_| ApiError::internal())?
            .map(str::to_string);
        Ok(CellDto {
            coord: coord.clone(),
            value,
            kind: "string",
            editable: resolved.all_leaf,
        })
    } else {
        let value = cube
            .get(&resolved.indices)
            .map_err(|_| ApiError::internal())?;
        Ok(CellDto {
            coord: coord.clone(),
            value: Some(value.to_string()),
            kind: "numeric",
            editable: resolved.all_leaf,
        })
    }
}

fn build_write(cube: &Cube, coord: &CoordMap, value: &str) -> Result<CellWrite, ApiError> {
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

fn map_batch_error(error: BatchError) -> ApiError {
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
        BatchError::Persist(_) => ApiError::internal(),
    }
}
