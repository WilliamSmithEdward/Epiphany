//! Shared dimension-library endpoints (ADR-0024): register a reusable dimension
//! in the server-level registry, list and inspect the library, append members to
//! a shared dimension (fanning the change out to every cube that references it),
//! and delete an unreferenced dimension. The library is server-global, so these
//! are gated on the global `Dimension` permission (ADR-0023): `Read` to list and
//! inspect, `Write` to register, grow, or delete. All mutations are audited.
//!
//! Creating a cube *by reference* to library dimensions lives in `model_routes`
//! (the create-cube body accepts a `ref`), as does the divergence guard that
//! blocks cube-local edits to a registry-backed dimension.

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_core::{EdgeSpec, ElementSpec};
use epiphany_engine::{DimensionError, DimensionId, PromoteError};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_cube_access, require_kind_access};
use crate::model_routes::{
    build_dimension_def, parse_element_kind, validate_name, ElementMemberDto, LocalEdgeDto,
};
use crate::resolve::kind_str;
use crate::routes::map_batch_error;
use crate::{ApiError, AppState};

// ---- request bodies ----

#[derive(Deserialize)]
pub(crate) struct NewSharedDimensionBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    elements: Vec<ElementMemberDto>,
    #[serde(default)]
    edges: Vec<LocalEdgeDto>,
}

#[derive(Deserialize)]
pub(crate) struct GrowDimensionBody {
    #[serde(default)]
    elements: Vec<GrowElementDto>,
    #[serde(default)]
    edges: Vec<GrowEdgeDto>,
}

/// An element to append to a shared dimension. Unlike the cube-local element DTO
/// there is no `dimension` field: the target is the path id, and the engine
/// stamps the dimension's name when it fans the append out.
#[derive(Deserialize)]
struct GrowElementDto {
    name: String,
    kind: String,
}

#[derive(Deserialize)]
struct GrowEdgeDto {
    parent: String,
    child: String,
    #[serde(default = "one")]
    weight: i64,
}

fn one() -> i64 {
    1
}

// ---- response bodies ----

#[derive(Serialize)]
pub(crate) struct DimensionSummaryDto {
    id: u64,
    name: String,
    generation: u64,
    element_count: u32,
    references: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct DimensionDetailDto {
    id: u64,
    name: String,
    generation: u64,
    references: Vec<String>,
    elements: Vec<ElementDto>,
    edges: Vec<EdgeDto>,
}

#[derive(Serialize)]
struct ElementDto {
    name: String,
    kind: &'static str,
}

#[derive(Serialize)]
struct EdgeDto {
    parent: String,
    child: String,
    weight: i64,
}

#[derive(Serialize)]
pub(crate) struct RegisteredDto {
    id: u64,
    name: String,
    generation: u64,
}

#[derive(Serialize)]
pub(crate) struct GrownDto {
    id: u64,
    generation: u64,
    fanned_out_to: Vec<String>,
}

// ---- helpers ----

fn summary(
    registry: &epiphany_engine::DimensionRegistry,
    shared: &epiphany_engine::SharedDimension,
) -> DimensionSummaryDto {
    DimensionSummaryDto {
        id: shared.id.0,
        name: shared.dimension.name().to_string(),
        generation: shared.generation,
        element_count: shared.dimension.len(),
        references: registry.referencing(shared.id),
    }
}

fn map_dimension_error(e: DimensionError) -> ApiError {
    match e {
        DimensionError::Unknown(id) => {
            ApiError::not_found(format!("unknown shared dimension #{}", id.0))
        }
        DimensionError::Referenced(cubes) => ApiError::conflict(format!(
            "shared dimension is referenced by {} cube(s) and cannot be deleted: {}",
            cubes.len(),
            cubes.join(", ")
        )),
    }
}

fn map_promote_error(e: PromoteError) -> ApiError {
    match e {
        PromoteError::UnknownCube(cube) => ApiError::not_found(format!("unknown cube '{cube}'")),
        PromoteError::UnknownDimension { cube, dimension } => {
            ApiError::not_found(format!("cube '{cube}' has no dimension '{dimension}'"))
        }
        PromoteError::AlreadyGlobal(id) => ApiError::conflict(format!(
            "dimension is already a global dimension (#{})",
            id.0
        )),
    }
}

// ---- handlers ----

/// `GET /api/v1/dimensions` -> list the shared dimension library (id, name,
/// generation, member count, referencing cubes). Requires global `Dimension`
/// `Read`.
pub(crate) async fn list_dimensions(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<Vec<DimensionSummaryDto>>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Dimension,
        None,
        AccessLevel::Read,
    )?;
    let registry = state.engine.dimension_registry();
    let list = registry
        .all()
        .iter()
        .map(|shared| summary(&registry, shared))
        .collect();
    Ok(Json(list))
}

/// `POST /api/v1/dimensions` -> register a new reusable dimension in the library.
/// Requires global `Dimension` `Write`.
pub(crate) async fn register_dimension(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Json(body): Json<NewSharedDimensionBody>,
) -> Result<Json<RegisteredDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Dimension,
        None,
        AccessLevel::Write,
    )?;

    // Reuse the same validation and build path as an inline cube dimension; the
    // engine realizes it into a core `Dimension` through the validated element/
    // edge path and the registry mints its stable id.
    let def = build_dimension_def(&body.name, &body.elements, &body.edges)?;
    let id = state
        .engine
        .register_dimension_def(&def)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectCreate,
        Some(&ObjectRef::global(ObjectKind::Dimension, &body.name)),
        true,
    );
    Ok(Json(RegisteredDto {
        id: id.0,
        name: body.name,
        generation: 0,
    }))
}

/// `GET /api/v1/dimensions/{id}` -> the full definition of one shared dimension.
/// Requires global `Dimension` `Read`.
pub(crate) async fn get_dimension(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Result<Json<DimensionDetailDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Dimension,
        None,
        AccessLevel::Read,
    )?;
    let registry = state.engine.dimension_registry();
    let shared = registry
        .get(DimensionId(id))
        .ok_or_else(|| ApiError::not_found(format!("unknown shared dimension #{id}")))?;
    let def = shared.to_dimension_def();
    Ok(Json(DimensionDetailDto {
        id,
        name: def.name,
        generation: shared.generation,
        references: registry.referencing(DimensionId(id)),
        elements: def
            .elements
            .into_iter()
            .map(|(name, kind)| ElementDto {
                name,
                kind: kind_str(kind),
            })
            .collect(),
        edges: def
            .edges
            .into_iter()
            .map(|(parent, child, weight)| EdgeDto {
                parent,
                child,
                weight,
            })
            .collect(),
    }))
}

/// `POST /api/v1/dimensions/{id}/elements` -> append members and consolidation
/// edges to a shared dimension (append-only, idempotent), fanning the change out
/// to every referencing cube. Requires global `Dimension` `Write`.
pub(crate) async fn grow_dimension(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Json(body): Json<GrowDimensionBody>,
) -> Result<Json<GrownDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Dimension,
        None,
        AccessLevel::Write,
    )?;

    // The element/edge specs carry no dimension name here; the engine stamps the
    // target dimension's name when it grows the registry and fans out, so a
    // placeholder is fine (it is overwritten before any cube write).
    let mut elements = Vec::with_capacity(body.elements.len());
    for e in &body.elements {
        validate_name("element", &e.name)?;
        elements.push(ElementSpec {
            dimension: String::new(),
            name: e.name.clone(),
            kind: parse_element_kind(&e.kind)?,
        });
    }
    let edges: Vec<EdgeSpec> = body
        .edges
        .iter()
        .map(|edge| EdgeSpec {
            dimension: String::new(),
            parent: edge.parent.clone(),
            child: edge.child.clone(),
            weight: edge.weight,
        })
        .collect();

    let generation = state
        .engine
        .grow_dimension(DimensionId(id), &elements, &edges)
        .map_err(map_batch_error)?;

    // Broadcast each fanned-out cube's new version so connected UIs refresh.
    let fanned_out_to = state
        .engine
        .dimension_registry()
        .referencing(DimensionId(id));
    for cube in &fanned_out_to {
        if let Some(version) = state.engine.version(cube) {
            let _ = state.events.send(crate::ws::ChangeEvent::ObjectsChanged {
                cube: cube.clone(),
                version,
            });
        }
    }

    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::global(ObjectKind::Dimension, format!("#{id}"))),
        true,
    );
    Ok(Json(GrownDto {
        id,
        generation,
        fanned_out_to,
    }))
}

/// `DELETE /api/v1/dimensions/{id}` -> remove an unreferenced shared dimension
/// from the library. A dimension still referenced by any cube is a 409 (the
/// cubes keep their materialized copies). Requires global `Dimension` `Write`.
pub(crate) async fn delete_dimension(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Dimension,
        None,
        AccessLevel::Write,
    )?;
    state
        .engine
        .delete_dimension(DimensionId(id))
        .map_err(map_dimension_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::global(ObjectKind::Dimension, format!("#{id}"))),
        true,
    );
    Ok(Json(serde_json::json!({ "deleted": id })))
}

/// `POST /api/v1/cubes/{cube}/dimensions/{dim}/promote` -> promote a cube's
/// embedded dimension into the global registry (ADR-0031 Phase 1), so it can be
/// referenced by other cubes. The cube keeps its own data unchanged; only the
/// dimension's identity becomes global. Requires global `Dimension` `Write` (it
/// creates a global dimension) and `Read` on the cube. A dimension that is
/// already registry-backed for the cube is a 409.
pub(crate) async fn promote_dimension(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim)): Path<(String, String)>,
) -> Result<Json<RegisteredDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Dimension,
        None,
        AccessLevel::Write,
    )?;
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let id = state
        .engine
        .promote_cube_dimension(&cube, &dim)
        .map_err(map_promote_error)?;
    // The cube's data/version is unchanged, but its dimension is now
    // registry-backed; broadcast so connected explorers refresh the dimension
    // list (the dimension moves from "lives in cube" to a global entry).
    if let Some(version) = state.engine.version(&cube) {
        let _ = state.events.send(crate::ws::ChangeEvent::ObjectsChanged {
            cube: cube.clone(),
            version,
        });
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectCreate,
        Some(&ObjectRef::global(ObjectKind::Dimension, &dim)),
        true,
    );
    Ok(Json(RegisteredDto {
        id: id.0,
        name: dim,
        generation: 0,
    }))
}
