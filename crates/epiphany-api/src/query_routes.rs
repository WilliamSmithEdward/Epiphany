//! Subset and view endpoints: CRUD, member/MDX preview, and view execution to a
//! cellset. All routes are name-addressed, gated behind [`AuthPrincipal`], and
//! enforce owner + visibility at this layer (public OR owned; admin bypass).
//!
//! Dynamic (MDX) subsets resolve through the injected `SetEvaluator` against the
//! pinned read snapshot, so reads stay lock-free and a cellset carries the
//! snapshot version. Per-cell `editable`/`kind` are re-derived here from the
//! resolved tuple members (core's cellset carries only values).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use epiphany_core::{
    execute_view, resolve_subset, Cellset, Cube, Sandbox, Subset, SubsetKind, View, Visibility,
};
use epiphany_engine::ReadSnapshot;
use epiphany_security::Principal;

use crate::auth::AuthPrincipal;
use crate::dto::{
    AxisMemberDto, AxisSpecBody, AxisSpecDto, CellsetCellDto, CellsetDto, ContextEntryDto,
    MdxPreviewRequest, MemberDto, MembersResponse, SubsetBody, SubsetDto, SubsetListResponse,
    SuppressedDto, ViewBody, ViewDto, ViewListResponse,
};
use crate::resolve::kind_str;
use crate::routes::map_batch_error;
use crate::sandbox_routes::{resolve_sandbox, SandboxSelector};
use crate::ws::ChangeEvent;
use crate::{ApiError, AppState};

// ---- shared helpers ----

fn snapshot(state: &AppState, cube: &str) -> Result<ReadSnapshot, ApiError> {
    state
        .engine
        .snapshot(cube)
        .ok_or_else(|| ApiError::not_found(format!("unknown cube '{cube}'")))
}

fn ensure_dimension(cube: &Cube, dim: &str) -> Result<(), ApiError> {
    if cube.dimensions().iter().any(|d| d.name() == dim) {
        Ok(())
    } else {
        Err(ApiError::unprocessable(
            "UNKNOWN_DIMENSION",
            format!("unknown dimension '{dim}' in cube '{}'", cube.name()),
        ))
    }
}

fn parse_visibility(value: &Option<String>) -> Result<Visibility, ApiError> {
    match value.as_deref() {
        None | Some("public") => Ok(Visibility::Public),
        Some("private") => Ok(Visibility::Private),
        Some(other) => Err(ApiError::bad_request(format!(
            "unknown visibility '{other}'"
        ))),
    }
}

fn vis_str(v: Visibility) -> &'static str {
    if v.is_public() {
        "public"
    } else {
        "private"
    }
}

/// May this principal see an object with the given owner and visibility?
fn can_read(p: &Principal, owner: &Option<String>, visibility: Visibility) -> bool {
    visibility.is_public() || p.is_admin || owner.as_deref() == Some(p.username.as_str())
}

/// May this principal modify or delete an object with the given owner?
fn can_modify(p: &Principal, owner: &Option<String>) -> bool {
    p.is_admin || owner.as_deref() == Some(p.username.as_str())
}

fn subset_from_body(
    name: String,
    dimension: String,
    owner: Option<String>,
    body: &SubsetBody,
) -> Result<Subset, ApiError> {
    let visibility = parse_visibility(&body.visibility)?;
    let kind = match body.kind.as_str() {
        "static" => SubsetKind::Static {
            members: body.members.clone(),
        },
        "dynamic" => {
            let mdx = body
                .mdx
                .clone()
                .ok_or_else(|| ApiError::bad_request("a dynamic subset requires 'mdx'"))?;
            SubsetKind::Dynamic { mdx }
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown subset kind '{other}'"
            )))
        }
    };
    Ok(Subset {
        name,
        dimension,
        owner,
        visibility,
        kind,
    })
}

fn subset_dto(s: &Subset) -> SubsetDto {
    let (kind, members, mdx) = match &s.kind {
        SubsetKind::Static { members } => ("static", members.clone(), None),
        SubsetKind::Dynamic { mdx } => ("dynamic", Vec::new(), Some(mdx.clone())),
    };
    SubsetDto {
        name: s.name.clone(),
        dimension: s.dimension.clone(),
        owner: s.owner.clone(),
        visibility: vis_str(s.visibility),
        kind,
        members,
        mdx,
    }
}

fn members_response(cube: &Cube, dim_name: &str, indices: &[u32]) -> MembersResponse {
    let dim = cube
        .dimensions()
        .iter()
        .find(|d| d.name() == dim_name)
        .expect("dimension validated");
    let members = indices
        .iter()
        .map(|&i| {
            let el = dim.element(i).expect("resolved index is valid");
            MemberDto {
                name: el.name.clone(),
                kind: kind_str(el.kind),
            }
        })
        .collect();
    MembersResponse { members }
}

// ---- subset endpoints ----

/// `POST /cubes/{cube}/dimensions/{dim}/subsets` -> create a subset.
pub(crate) async fn create_subset(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim)): Path<(String, String)>,
    Json(body): Json<SubsetBody>,
) -> Result<(StatusCode, Json<SubsetDto>), ApiError> {
    let name = body
        .name
        .clone()
        .ok_or_else(|| ApiError::bad_request("subset 'name' is required"))?;
    let snap = snapshot(&state, &cube)?;
    ensure_dimension(snap.cube(), &dim)?;
    if snap.subset(&dim, &name).is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "DUPLICATE_NAME",
            format!("subset '{name}' already exists in dimension '{dim}'"),
        ));
    }
    let subset = subset_from_body(
        name,
        dim.clone(),
        Some(auth.principal.username.clone()),
        &body,
    )?;
    // Validate it resolves (static members / dynamic MDX) before persisting.
    resolve_subset(snap.cube(), &subset, state.evaluator())?;
    let outcome = state
        .engine
        .define_subset(&cube, None, subset.clone())
        .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.clone(),
        version: outcome.version,
    });
    Ok((StatusCode::CREATED, Json(subset_dto(&subset))))
}

/// `GET /cubes/{cube}/dimensions/{dim}/subsets` -> the visible subsets.
pub(crate) async fn list_subsets(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim)): Path<(String, String)>,
) -> Result<Json<SubsetListResponse>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    ensure_dimension(snap.cube(), &dim)?;
    let subsets = snap
        .model()
        .subsets
        .iter()
        .filter(|((d, _), _)| d == &dim)
        .map(|(_, s)| s)
        .filter(|s| can_read(&auth.principal, &s.owner, s.visibility))
        .map(subset_dto)
        .collect();
    Ok(Json(SubsetListResponse { subsets }))
}

/// `GET /cubes/{cube}/dimensions/{dim}/subsets/{name}` -> one subset.
pub(crate) async fn get_subset(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim, name)): Path<(String, String, String)>,
) -> Result<Json<SubsetDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let s = visible_subset(&snap, &auth.principal, &dim, &name)?;
    Ok(Json(subset_dto(s)))
}

/// `PUT /cubes/{cube}/dimensions/{dim}/subsets/{name}` -> replace a subset.
pub(crate) async fn replace_subset(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim, name)): Path<(String, String, String)>,
    Json(body): Json<SubsetBody>,
) -> Result<Json<SubsetDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let existing = snap
        .subset(&dim, &name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "UNKNOWN_SUBSET", "no such subset"))?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    // Preserve the original owner across an edit.
    let subset = subset_from_body(name, dim.clone(), existing.owner.clone(), &body)?;
    resolve_subset(snap.cube(), &subset, state.evaluator())?;
    let outcome = state
        .engine
        .define_subset(&cube, None, subset.clone())
        .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.clone(),
        version: outcome.version,
    });
    Ok(Json(subset_dto(&subset)))
}

/// `DELETE /cubes/{cube}/dimensions/{dim}/subsets/{name}` -> delete a subset.
pub(crate) async fn delete_subset(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let existing = snap
        .subset(&dim, &name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "UNKNOWN_SUBSET", "no such subset"))?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    let outcome = state
        .engine
        .delete_subset(&cube, None, &dim, &name)
        .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube,
        version: outcome.version,
    });
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /cubes/{cube}/dimensions/{dim}/subsets/{name}/members` -> resolved members.
pub(crate) async fn subset_members(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim, name)): Path<(String, String, String)>,
) -> Result<Json<MembersResponse>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let s = visible_subset(&snap, &auth.principal, &dim, &name)?;
    let indices = resolve_subset(snap.cube(), s, state.evaluator())?;
    Ok(Json(members_response(snap.cube(), &dim, &indices)))
}

/// `POST /cubes/{cube}/dimensions/{dim}/subsets/preview` -> resolve an unsaved subset.
pub(crate) async fn preview_subset(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim)): Path<(String, String)>,
    Json(body): Json<SubsetBody>,
) -> Result<Json<MembersResponse>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    ensure_dimension(snap.cube(), &dim)?;
    let subset = subset_from_body("preview".to_string(), dim.clone(), None, &body)?;
    let indices = resolve_subset(snap.cube(), &subset, state.evaluator())?;
    Ok(Json(members_response(snap.cube(), &dim, &indices)))
}

/// `POST /cubes/{cube}/dimensions/{dim}/mdx/preview` -> resolve an MDX set.
pub(crate) async fn preview_mdx(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim)): Path<(String, String)>,
    Json(body): Json<MdxPreviewRequest>,
) -> Result<Json<MembersResponse>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    ensure_dimension(snap.cube(), &dim)?;
    let subset = Subset {
        name: "preview".to_string(),
        dimension: dim.clone(),
        owner: None,
        visibility: Visibility::Public,
        kind: SubsetKind::Dynamic { mdx: body.mdx },
    };
    let indices = resolve_subset(snap.cube(), &subset, state.evaluator())?;
    Ok(Json(members_response(snap.cube(), &dim, &indices)))
}

fn visible_subset<'a>(
    snap: &'a ReadSnapshot,
    principal: &Principal,
    dim: &str,
    name: &str,
) -> Result<&'a Subset, ApiError> {
    let s = snap
        .subset(dim, name)
        .filter(|s| can_read(principal, &s.owner, s.visibility))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "UNKNOWN_SUBSET", "no such subset"))?;
    Ok(s)
}

// ---- view endpoints ----

fn axis_from_body(specs: &[AxisSpecBody]) -> Result<Vec<epiphany_core::AxisSpec>, ApiError> {
    specs
        .iter()
        .map(|s| match s.spec_type.as_str() {
            "subset" => {
                let subset = s
                    .subset
                    .clone()
                    .ok_or_else(|| ApiError::bad_request("axis type 'subset' requires 'subset'"))?;
                Ok(epiphany_core::AxisSpec::Subset {
                    dimension: s.dimension.clone(),
                    subset,
                })
            }
            "members" => Ok(epiphany_core::AxisSpec::Members {
                dimension: s.dimension.clone(),
                members: s.members.clone(),
            }),
            other => Err(ApiError::bad_request(format!(
                "unknown axis type '{other}'"
            ))),
        })
        .collect()
}

fn view_from_body(
    name: String,
    cube: String,
    owner: Option<String>,
    body: &ViewBody,
) -> Result<View, ApiError> {
    Ok(View {
        name,
        cube,
        owner,
        visibility: parse_visibility(&body.visibility)?,
        rows: axis_from_body(&body.rows)?,
        columns: axis_from_body(&body.columns)?,
        context: body
            .context
            .iter()
            .map(|c| (c.dimension.clone(), c.member.clone()))
            .collect(),
        suppress_zeros: body.suppress_zeros,
    })
}

fn axis_spec_dto(spec: &epiphany_core::AxisSpec) -> AxisSpecDto {
    match spec {
        epiphany_core::AxisSpec::Subset { dimension, subset } => AxisSpecDto {
            dimension: dimension.clone(),
            spec_type: "subset",
            subset: Some(subset.clone()),
            members: Vec::new(),
        },
        epiphany_core::AxisSpec::Members { dimension, members } => AxisSpecDto {
            dimension: dimension.clone(),
            spec_type: "members",
            subset: None,
            members: members.clone(),
        },
    }
}

fn view_dto(v: &View) -> ViewDto {
    ViewDto {
        name: v.name.clone(),
        cube: v.cube.clone(),
        owner: v.owner.clone(),
        visibility: vis_str(v.visibility),
        suppress_zeros: v.suppress_zeros,
        rows: v.rows.iter().map(axis_spec_dto).collect(),
        columns: v.columns.iter().map(axis_spec_dto).collect(),
        context: v
            .context
            .iter()
            .map(|(dimension, member)| ContextEntryDto {
                dimension: dimension.clone(),
                member: member.clone(),
            })
            .collect(),
    }
}

/// `POST /cubes/{cube}/views` -> create a view.
pub(crate) async fn create_view(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    Json(body): Json<ViewBody>,
) -> Result<(StatusCode, Json<ViewDto>), ApiError> {
    let name = body
        .name
        .clone()
        .ok_or_else(|| ApiError::bad_request("view 'name' is required"))?;
    let snap = snapshot(&state, &cube)?;
    if snap.view(&name).is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "DUPLICATE_NAME",
            format!("view '{name}' already exists"),
        ));
    }
    let view = view_from_body(
        name,
        cube.clone(),
        Some(auth.principal.username.clone()),
        &body,
    )?;
    let outcome = state
        .engine
        .define_view(&cube, None, view.clone())
        .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.clone(),
        version: outcome.version,
    });
    Ok((StatusCode::CREATED, Json(view_dto(&view))))
}

/// `GET /cubes/{cube}/views` -> the visible views.
pub(crate) async fn list_views(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
) -> Result<Json<ViewListResponse>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let views = snap
        .model()
        .views
        .values()
        .filter(|v| can_read(&auth.principal, &v.owner, v.visibility))
        .map(view_dto)
        .collect();
    Ok(Json(ViewListResponse { views }))
}

/// `GET /cubes/{cube}/views/{name}` -> one view.
pub(crate) async fn get_view(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<Json<ViewDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let v = visible_view(&snap, &auth.principal, &name)?;
    Ok(Json(view_dto(v)))
}

/// `PUT /cubes/{cube}/views/{name}` -> replace a view.
pub(crate) async fn replace_view(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
    Json(body): Json<ViewBody>,
) -> Result<Json<ViewDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let existing = snap
        .view(&name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "UNKNOWN_VIEW", "no such view"))?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    let view = view_from_body(name, cube.clone(), existing.owner.clone(), &body)?;
    let outcome = state
        .engine
        .define_view(&cube, None, view.clone())
        .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube: cube.clone(),
        version: outcome.version,
    });
    Ok(Json(view_dto(&view)))
}

/// `DELETE /cubes/{cube}/views/{name}` -> delete a view.
pub(crate) async fn delete_view(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let existing = snap
        .view(&name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "UNKNOWN_VIEW", "no such view"))?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    let outcome = state
        .engine
        .delete_view(&cube, None, &name)
        .map_err(map_batch_error)?;
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube,
        version: outcome.version,
    });
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /cubes/{cube}/views/{name}/execute` -> execute a saved view.
pub(crate) async fn execute_saved_view(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
    selector: SandboxSelector,
) -> Result<Json<CellsetDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let view = visible_view(&snap, &auth.principal, &name)?;
    // An active sandbox overlays its what-if leaves, so the cellset recomputes
    // over them (ADR-0014); absent it, base.
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    // Values come through the injected resolver (rule-aware in the server).
    let resolver = state.cells.resolver_with(&snap, sandbox);
    let cellset = snap.model().execute(view, &*resolver, state.evaluator())?;
    Ok(Json(cellset_dto(
        snap.cube(),
        cellset,
        snap.version(),
        sandbox,
    )))
}

/// `POST /cubes/{cube}/cellset` -> execute an ad-hoc view spec without saving.
pub(crate) async fn execute_adhoc(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(body): Json<ViewBody>,
) -> Result<Json<CellsetDto>, ApiError> {
    let snap = snapshot(&state, &cube)?;
    let view = view_from_body("adhoc".to_string(), cube.clone(), None, &body)?;
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    let resolver = state.cells.resolver_with(&snap, sandbox);
    let cellset = execute_view(
        snap.cube(),
        &view,
        &*resolver,
        &|d, n| snap.subset(d, n),
        state.evaluator(),
    )?;
    Ok(Json(cellset_dto(
        snap.cube(),
        cellset,
        snap.version(),
        sandbox,
    )))
}

fn visible_view<'a>(
    snap: &'a ReadSnapshot,
    principal: &Principal,
    name: &str,
) -> Result<&'a View, ApiError> {
    snap.view(name)
        .filter(|v| can_read(principal, &v.owner, v.visibility))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "UNKNOWN_VIEW", "no such view"))
}

fn forbidden() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "FORBIDDEN",
        "you do not own this object",
    )
}

/// Build the API cellset from the core cellset, re-resolving each tuple member to
/// its element kind and deriving per-cell `editable` (a cell is editable only if
/// every member across its row tuple, column tuple, and the context is a leaf).
/// Reconstruct a cell's full coordinate (as element indices) from its row tuple,
/// column tuple, and the context, for checking against a sandbox's overrides.
/// `None` if any member does not resolve.
fn cell_coord_indices(
    cube: &Cube,
    row_dims: &[String],
    row_tuple: &[String],
    col_dims: &[String],
    col_tuple: &[String],
    context: &[(String, String)],
) -> Option<Vec<u32>> {
    let name_for = |dim: &str| -> Option<&str> {
        if let Some(p) = row_dims.iter().position(|d| d == dim) {
            return row_tuple.get(p).map(String::as_str);
        }
        if let Some(p) = col_dims.iter().position(|d| d == dim) {
            return col_tuple.get(p).map(String::as_str);
        }
        context
            .iter()
            .find(|(d, _)| d == dim)
            .map(|(_, m)| m.as_str())
    };
    let mut coord = Vec::with_capacity(cube.rank());
    for d in cube.dimensions() {
        coord.push(d.index_of(name_for(d.name())?)?);
    }
    Some(coord)
}

fn cellset_dto(cube: &Cube, cs: Cellset, version: u64, sandbox: Option<&Sandbox>) -> CellsetDto {
    let leaf_of = |dim_name: &str, member: &str| -> bool {
        cube.dimensions()
            .iter()
            .find(|d| d.name() == dim_name)
            .and_then(|d| d.index_of(member).and_then(|i| d.element(i).ok()))
            .map(|el| el.kind.is_leaf())
            .unwrap_or(false)
    };
    let member_dto = |dims: &[String], tuple: &[String]| -> Vec<AxisMemberDto> {
        tuple
            .iter()
            .enumerate()
            .map(|(k, name)| {
                let kind = cube
                    .dimensions()
                    .iter()
                    .find(|d| d.name() == dims[k])
                    .and_then(|d| d.index_of(name).and_then(|i| d.element(i).ok()))
                    .map(|el| kind_str(el.kind))
                    .unwrap_or("numeric");
                AxisMemberDto {
                    dimension: dims[k].clone(),
                    name: name.clone(),
                    kind,
                }
            })
            .collect()
    };
    let tuple_leaf = |dims: &[String], tuple: &[String]| {
        tuple.iter().enumerate().all(|(k, m)| leaf_of(&dims[k], m))
    };

    let context_leaf = cs.context.iter().all(|(d, m)| leaf_of(d, m));
    let row_leaf: Vec<bool> = cs
        .row_tuples
        .iter()
        .map(|t| tuple_leaf(&cs.row_dimensions, t))
        .collect();
    let col_leaf: Vec<bool> = cs
        .column_tuples
        .iter()
        .map(|t| tuple_leaf(&cs.column_dimensions, t))
        .collect();

    let ncols = cs.column_tuples.len().max(1);
    let cells = cs
        .cells
        .iter()
        .enumerate()
        .map(|(ordinal, value)| {
            let r = ordinal / ncols;
            let c = ordinal % ncols;
            // Flag a cell whose exact leaf is a what-if override in the active
            // sandbox (a consolidation that rolled one up is not flagged).
            let overlaid = sandbox.is_some_and(|sb| {
                cs.row_tuples
                    .get(r)
                    .zip(cs.column_tuples.get(c))
                    .and_then(|(rt, ct)| {
                        cell_coord_indices(
                            cube,
                            &cs.row_dimensions,
                            rt,
                            &cs.column_dimensions,
                            ct,
                            &cs.context,
                        )
                    })
                    .is_some_and(|ix| sb.cells.contains_key(&ix))
            });
            CellsetCellDto {
                value: Some(value.to_string()),
                kind: "numeric",
                editable: context_leaf
                    && row_leaf.get(r).copied().unwrap_or(false)
                    && col_leaf.get(c).copied().unwrap_or(false),
                ordinal,
                overlaid,
            }
        })
        .collect();

    let row_tuples = cs
        .row_tuples
        .iter()
        .map(|t| member_dto(&cs.row_dimensions, t))
        .collect();
    let column_tuples = cs
        .column_tuples
        .iter()
        .map(|t| member_dto(&cs.column_dimensions, t))
        .collect();

    CellsetDto {
        row_dimensions: cs.row_dimensions,
        column_dimensions: cs.column_dimensions,
        row_tuples,
        column_tuples,
        context: cs
            .context
            .into_iter()
            .map(|(dimension, member)| ContextEntryDto { dimension, member })
            .collect(),
        cells,
        version,
        suppressed: SuppressedDto {
            row_tuples: cs.suppressed_row_tuples.len(),
            column_tuples: cs.suppressed_column_tuples.len(),
        },
    }
}
