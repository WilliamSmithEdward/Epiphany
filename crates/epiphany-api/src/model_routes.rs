//! Model-editing endpoints (ADR-0021): create a cube, add elements and
//! consolidation edges to existing dimensions, and define or set attributes.
//! All are AuthPrincipal-gated and audited. Creating a cube is an admin
//! operation; editing an existing cube's structure requires cube `Write`.
//! Destructive and rank-changing edits are out of scope (ADR-0021 non-goals).

use std::str::FromStr;

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_core::{
    AttributeKind, AttributeValue, DimensionDef, EdgeSpec, ElementKind, ElementSpec, Fixed,
};
use epiphany_engine::{CubeDimensionSpec, DimensionId};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_kind_access, require_manage_cubes};
use crate::routes::{broadcast, map_batch_error};
use crate::{ApiError, AppState};

// ---- request bodies ----

#[derive(Deserialize)]
pub(crate) struct NewCubeBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    dimensions: Vec<NewDimensionDto>,
}

#[derive(Deserialize)]
struct NewDimensionDto {
    #[serde(default)]
    name: String,
    #[serde(default)]
    elements: Vec<ElementMemberDto>,
    #[serde(default)]
    edges: Vec<LocalEdgeDto>,
    /// When set, this dimension is a reference to a registered shared dimension
    /// (ADR-0024); the cube materializes a copy of it and `name`/`elements`/`edges`
    /// are ignored.
    #[serde(default, rename = "ref")]
    reference: Option<u64>,
}

#[derive(Deserialize)]
pub(crate) struct ElementMemberDto {
    pub(crate) name: String,
    pub(crate) kind: String,
}

#[derive(Deserialize)]
pub(crate) struct LocalEdgeDto {
    pub(crate) parent: String,
    pub(crate) child: String,
    #[serde(default = "one")]
    pub(crate) weight: i64,
}

#[derive(Deserialize)]
pub(crate) struct AddElementsBody {
    #[serde(default)]
    elements: Vec<ElementSpecDto>,
    #[serde(default)]
    edges: Vec<EdgeSpecDto>,
}

#[derive(Deserialize)]
struct ElementSpecDto {
    dimension: String,
    name: String,
    kind: String,
}

#[derive(Deserialize)]
struct EdgeSpecDto {
    dimension: String,
    parent: String,
    child: String,
    #[serde(default = "one")]
    weight: i64,
}

#[derive(Deserialize)]
pub(crate) struct AttributeBody {
    kind: String,
}

#[derive(Deserialize)]
pub(crate) struct AttributeValuesBody {
    #[serde(default)]
    values: Vec<AttributeValueDto>,
}

#[derive(Deserialize)]
struct AttributeValueDto {
    element: String,
    value: String,
}

fn one() -> i64 {
    1
}

#[derive(Serialize)]
pub(crate) struct CommitDto {
    version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    elements_added: Option<usize>,
}

// ---- helpers ----

pub(crate) fn parse_element_kind(s: &str) -> Result<ElementKind, ApiError> {
    match s {
        "numeric" | "leaf" => Ok(ElementKind::Leaf),
        "string" => Ok(ElementKind::String),
        "consolidated" => Ok(ElementKind::Consolidated),
        other => Err(ApiError::unprocessable(
            "INVALID_ELEMENT_KIND",
            format!("unknown element kind '{other}' (expected numeric, string, or consolidated)"),
        )),
    }
}

fn parse_attribute_kind(s: &str) -> Result<AttributeKind, ApiError> {
    match s {
        "text" => Ok(AttributeKind::Text),
        "numeric" => Ok(AttributeKind::Numeric),
        "alias" => Ok(AttributeKind::Alias),
        other => Err(ApiError::unprocessable(
            "INVALID_ATTRIBUTE_KIND",
            format!("unknown attribute kind '{other}' (expected text, numeric, or alias)"),
        )),
    }
}

/// A cube/dimension/element/attribute name must be a non-empty, trimmed token
/// without the separators the model-as-code format and coordinates rely on.
pub(crate) fn validate_name(kind: &str, name: &str) -> Result<(), ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ApiError::unprocessable(
            "INVALID_NAME",
            format!("{kind} name must not be empty"),
        ));
    }
    if trimmed != name {
        return Err(ApiError::unprocessable(
            "INVALID_NAME",
            format!("{kind} name '{name}' must not have leading or trailing spaces"),
        ));
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(ApiError::unprocessable(
            "INVALID_NAME",
            format!("{kind} name contains a control character"),
        ));
    }
    Ok(())
}

/// Validate and assemble a [`DimensionDef`] from request DTOs (name, members,
/// edges). Shared by inline cube creation and shared-dimension registration so
/// both apply identical name and kind validation.
pub(crate) fn build_dimension_def(
    name: &str,
    elements: &[ElementMemberDto],
    edges: &[LocalEdgeDto],
) -> Result<DimensionDef, ApiError> {
    validate_name("dimension", name)?;
    let mut els = Vec::with_capacity(elements.len());
    for e in elements {
        validate_name("element", &e.name)?;
        els.push((e.name.clone(), parse_element_kind(&e.kind)?));
    }
    let edges = edges
        .iter()
        .map(|edge| (edge.parent.clone(), edge.child.clone(), edge.weight))
        .collect();
    Ok(DimensionDef {
        name: name.to_string(),
        elements: els,
        edges,
    })
}

// ---- handlers ----

/// `POST /api/v1/cubes` -> create a new cube with its dimensions and initial
/// members declared up front (admin only). A dimension cannot be added later, so
/// declare them all here.
pub(crate) async fn create_cube(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Json(body): Json<NewCubeBody>,
) -> Result<Json<CommitDto>, ApiError> {
    require_manage_cubes(&state, &auth)?;
    validate_name("cube", &body.name)?;
    if body.dimensions.is_empty() {
        return Err(ApiError::unprocessable(
            "EMPTY_CUBE",
            "a cube must have at least one dimension",
        ));
    }

    // Each dimension is either a reference to a registered shared dimension
    // (materialized at create time, ADR-0024) or an inline definition.
    let mut dims = Vec::with_capacity(body.dimensions.len());
    for d in &body.dimensions {
        match d.reference {
            Some(id) => dims.push(CubeDimensionSpec::Ref(DimensionId(id))),
            None => dims.push(CubeDimensionSpec::Inline(build_dimension_def(
                &d.name,
                &d.elements,
                &d.edges,
            )?)),
        }
    }

    let outcome = state
        .engine
        .create_cube_with_refs(&body.name, &dims)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectCreate,
        Some(&ObjectRef::cube(&body.name)),
        true,
    );
    broadcast(&state, &body.name);
    Ok(Json(CommitDto {
        version: outcome.version,
        elements_added: None,
    }))
}

/// `POST /api/v1/cubes/{cube}/elements` -> add elements and consolidation edges
/// to existing dimensions (append-only, idempotent). Requires cube `Write`.
pub(crate) async fn add_elements(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<AddElementsBody>,
) -> Result<Json<CommitDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Dimension,
        Some(&cube),
        AccessLevel::Write,
    )?;

    let mut elements = Vec::with_capacity(body.elements.len());
    for e in &body.elements {
        validate_name("element", &e.name)?;
        elements.push(ElementSpec {
            dimension: e.dimension.clone(),
            name: e.name.clone(),
            kind: parse_element_kind(&e.kind)?,
        });
    }
    let edges: Vec<EdgeSpec> = body
        .edges
        .iter()
        .map(|edge| EdgeSpec {
            dimension: edge.dimension.clone(),
            parent: edge.parent.clone(),
            child: edge.child.clone(),
            weight: edge.weight,
        })
        .collect();

    // Divergence guard (ADR-0024 v1): a cube-local edit to a dimension that is a
    // materialized reference to a shared dimension would let one cube's copy
    // drift from the library and every other referencing cube. Block it and
    // direct the caller to grow the shared dimension instead, which fans out to
    // all referencing cubes.
    for dim in elements
        .iter()
        .map(|e| e.dimension.as_str())
        .chain(edges.iter().map(|e| e.dimension.as_str()))
    {
        if let Some(id) = state.engine.dimension_backing(&cube, dim) {
            return Err(ApiError::conflict(format!(
                "dimension '{dim}' in cube '{cube}' is a shared dimension (#{}); edit it in the \
                 dimension library so every referencing cube stays consistent",
                id.0
            )));
        }
    }

    let (outcome, added) = state
        .engine
        .define_elements(&cube, None, &elements, &edges)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::cube(&cube)),
        true,
    );
    broadcast(&state, &cube);
    Ok(Json(CommitDto {
        version: outcome.version,
        elements_added: Some(added),
    }))
}

/// `PUT /api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}` -> define an
/// attribute on a dimension (idempotent; a different kind is a conflict).
/// Requires cube `Write`.
pub(crate) async fn define_attribute(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dimension, attribute)): Path<(String, String, String)>,
    Json(body): Json<AttributeBody>,
) -> Result<Json<CommitDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Dimension,
        Some(&cube),
        AccessLevel::Write,
    )?;
    validate_name("attribute", &attribute)?;
    let kind = parse_attribute_kind(&body.kind)?;

    let outcome = state
        .engine
        .define_attribute(&cube, None, &dimension, &attribute, kind)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(
            ObjectKind::Dimension,
            &cube,
            &dimension,
        )),
        true,
    );
    broadcast(&state, &cube);
    Ok(Json(CommitDto {
        version: outcome.version,
        elements_added: None,
    }))
}

/// `PUT /api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}/values` -> set an
/// attribute's value for one or more elements. The value is parsed according to
/// the attribute's declared kind. Requires cube `Write`.
pub(crate) async fn set_attribute_values(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dimension, attribute)): Path<(String, String, String)>,
    Json(body): Json<AttributeValuesBody>,
) -> Result<Json<CommitDto>, ApiError> {
    require_kind_access(
        &state,
        &auth,
        ObjectKind::Dimension,
        Some(&cube),
        AccessLevel::Write,
    )?;

    // Read the attribute's kind from the live snapshot to parse each value.
    let snap = state
        .engine
        .snapshot(&cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))?;
    let dim = snap
        .cube()
        .dimensions()
        .iter()
        .find(|d| d.name() == dimension)
        .ok_or_else(|| {
            ApiError::unprocessable(
                "UNKNOWN_DIMENSION",
                format!("cube '{cube}' has no dimension '{dimension}'"),
            )
        })?;
    let attr_kind = dim
        .attribute_defs()
        .iter()
        .find(|a| a.name == attribute)
        .map(|a| a.kind)
        .ok_or_else(|| {
            ApiError::unprocessable(
                "ATTRIBUTE_NOT_FOUND",
                format!("attribute '{attribute}' is not defined on dimension '{dimension}'"),
            )
        })?;

    let mut values = Vec::with_capacity(body.values.len());
    for v in &body.values {
        let value = match attr_kind {
            AttributeKind::Numeric => {
                AttributeValue::Numeric(Fixed::from_str(&v.value).map_err(|_| {
                    ApiError::unprocessable(
                        "INVALID_NUMBER",
                        format!("invalid number '{}'", v.value),
                    )
                })?)
            }
            AttributeKind::Text | AttributeKind::Alias => AttributeValue::Text(v.value.clone()),
        };
        values.push((v.element.clone(), value));
    }
    drop(snap);

    let outcome = state
        .engine
        .set_attribute_values(&cube, None, &dimension, &attribute, &values)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(
            ObjectKind::Dimension,
            &cube,
            &dimension,
        )),
        true,
    );
    broadcast(&state, &cube);
    Ok(Json(CommitDto {
        version: outcome.version,
        elements_added: None,
    }))
}
