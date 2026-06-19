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
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef, Principal};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, element_mask, require_cube_access};
use crate::dto::{
    AxisMemberDto, AxisSpecBody, AxisSpecDto, CellsetCellDto, CellsetDto, ContextEntryDto,
    MdxPreviewRequest, MdxQueryRequest, MemberDto, MembersResponse, SubsetBody, SubsetDto,
    SubsetListResponse, SuppressedDto, ViewBody, ViewDto, ViewListResponse,
};
use crate::resolve::kind_str;
use crate::routes::{map_batch_error, snapshot};
use crate::sandbox_routes::{resolve_sandbox, SandboxSelector};
use crate::ws::ChangeEvent;
use crate::{ApiError, AppState};

// ---- shared helpers ----

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
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
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
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectCreate,
        Some(&ObjectRef::in_cube(ObjectKind::Subset, &cube, &subset.name)),
        true,
    );
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
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
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(ObjectKind::Subset, &cube, &subset.name)),
        true,
    );
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    let outcome = state
        .engine
        .delete_subset(&cube, None, &dim, &name)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::in_cube(ObjectKind::Subset, &cube, &name)),
        true,
    );
    let _ = state.events.send(ChangeEvent::ObjectsChanged {
        cube,
        version: outcome.version,
    });
    Ok(StatusCode::NO_CONTENT)
}

/// Drop members the caller may not see (ADR-0015 element security): a denied
/// member, or one rolling up a denied leaf, is omitted from an enumeration -- like
/// zero-suppression -- so the member's existence never leaks through a member or
/// preview listing. Admin and ACL-free cubes keep every member (no mask).
fn suppress_denied_members(
    state: &AppState,
    auth: &AuthPrincipal,
    snap: &ReadSnapshot,
    dim: &str,
    indices: Vec<u32>,
) -> Vec<u32> {
    let Some(mask) = element_mask(state, auth, snap) else {
        return indices;
    };
    let cube = snap.cube();
    let Some(pos) = cube.dimensions().iter().position(|d| d.name() == dim) else {
        return indices;
    };
    indices
        .into_iter()
        .filter(|&i| !mask.denies_member(cube, pos, i))
        .collect()
}

/// `GET /cubes/{cube}/dimensions/{dim}/subsets/{name}/members` -> resolved members.
pub(crate) async fn subset_members(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim, name)): Path<(String, String, String)>,
) -> Result<Json<MembersResponse>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let s = visible_subset(&snap, &auth.principal, &dim, &name)?;
    let indices = resolve_subset(snap.cube(), s, state.evaluator())?;
    let indices = suppress_denied_members(&state, &auth, &snap, &dim, indices);
    Ok(Json(members_response(snap.cube(), &dim, &indices)))
}

/// `POST /cubes/{cube}/dimensions/{dim}/subsets/preview` -> resolve an unsaved subset.
pub(crate) async fn preview_subset(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim)): Path<(String, String)>,
    Json(body): Json<SubsetBody>,
) -> Result<Json<MembersResponse>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    ensure_dimension(snap.cube(), &dim)?;
    let subset = subset_from_body("preview".to_string(), dim.clone(), None, &body)?;
    let indices = resolve_subset(snap.cube(), &subset, state.evaluator())?;
    let indices = suppress_denied_members(&state, &auth, &snap, &dim, indices);
    Ok(Json(members_response(snap.cube(), &dim, &indices)))
}

/// `POST /cubes/{cube}/dimensions/{dim}/mdx/preview` -> resolve an MDX set.
pub(crate) async fn preview_mdx(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim)): Path<(String, String)>,
    Json(body): Json<MdxPreviewRequest>,
) -> Result<Json<MembersResponse>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
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
    let indices = suppress_denied_members(&state, &auth, &snap, &dim, indices);
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
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
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectCreate,
        Some(&ObjectRef::in_cube(ObjectKind::View, &cube, &view.name)),
        true,
    );
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    let view = view_from_body(name, cube.clone(), existing.owner.clone(), &body)?;
    let outcome = state
        .engine
        .define_view(&cube, None, view.clone())
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(ObjectKind::View, &cube, &view.name)),
        true,
    );
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    let outcome = state
        .engine
        .delete_view(&cube, None, &name)
        .map_err(map_batch_error)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::in_cube(ObjectKind::View, &cube, &name)),
        true,
    );
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let view = visible_view(&snap, &auth.principal, &name)?;
    // An active sandbox overlays its what-if leaves, so the cellset recomputes
    // over them (ADR-0014); absent it, base.
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    // Values come through the injected resolver (rule-aware in the server),
    // carrying the caller's element deny mask (ADR-0015): denied members are
    // suppressed from the axes and a cell rolling up a denied leaf is denied.
    let mask = element_mask(&state, &auth, &snap);
    // Read-through the view cache (ADR-0028). The resolver (which compiles every
    // cube's rules) is built lazily inside the closure, so a cache hit skips it.
    let cellset = state.view_cache.get_or_compute(
        crate::view_cache::ViewRead {
            cube: &cube,
            version: snap.version(),
            view,
            sandbox,
            mask: mask.as_ref(),
            is_adhoc: false,
        },
        || {
            let resolver = state.cells.resolver_with(&snap, sandbox, mask.as_ref());
            snap.model()
                .execute(view, &*resolver, state.evaluator(), mask.as_ref())
        },
    )?;
    Ok(Json(cellset_dto(
        snap.cube(),
        &cellset,
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
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let view = view_from_body("adhoc".to_string(), cube.clone(), None, &body)?;
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    let mask = element_mask(&state, &auth, &snap);
    // Read-through the view cache (ADR-0028), ad-hoc pool. The resolver is built
    // lazily inside the closure so a cache hit skips rule compilation.
    let cellset = state.view_cache.get_or_compute(
        crate::view_cache::ViewRead {
            cube: &cube,
            version: snap.version(),
            view: &view,
            sandbox,
            mask: mask.as_ref(),
            is_adhoc: true,
        },
        || {
            let resolver = state.cells.resolver_with(&snap, sandbox, mask.as_ref());
            execute_view(
                snap.cube(),
                &view,
                &*resolver,
                &|d, n| snap.subset(d, n),
                state.evaluator(),
                mask.as_ref(),
            )
        },
    )?;
    Ok(Json(cellset_dto(
        snap.cube(),
        &cellset,
        snap.version(),
        sandbox,
    )))
}

/// `POST /cubes/{cube}/mdx` -> parse and execute a full MDX `SELECT` query to a
/// cellset. Mirrors [`execute_adhoc`] exactly (same `Read` gate, sandbox overlay,
/// element mask, and ad-hoc cache pool); only the request shape differs: the MDX
/// text is parsed and lowered to a [`View`] before execution.
pub(crate) async fn execute_mdx(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(cube): Path<String>,
    selector: SandboxSelector,
    Json(body): Json<MdxQueryRequest>,
) -> Result<Json<CellsetDto>, ApiError> {
    require_cube_access(&state, &auth, &cube, AccessLevel::Read)?;
    let snap = snapshot(&state, &cube)?;
    let query = epiphany_mdx::parse_query(&body.mdx)
        .map_err(|e| ApiError::unprocessable("MDX_PARSE_ERROR", e.to_string()))?;
    let view = view_from_mdx(snap.cube(), &cube, &query)?;
    let sandbox_name = resolve_sandbox(&snap, &auth.principal, &selector)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    let mask = element_mask(&state, &auth, &snap);
    // Read-through the view cache (ADR-0028), ad-hoc pool. The resolver is built
    // lazily inside the closure so a cache hit skips rule compilation.
    let cellset = state.view_cache.get_or_compute(
        crate::view_cache::ViewRead {
            cube: &cube,
            version: snap.version(),
            view: &view,
            sandbox,
            mask: mask.as_ref(),
            is_adhoc: true,
        },
        || {
            let resolver = state.cells.resolver_with(&snap, sandbox, mask.as_ref());
            execute_view(
                snap.cube(),
                &view,
                &*resolver,
                &|d, n| snap.subset(d, n),
                state.evaluator(),
                mask.as_ref(),
            )
        },
    )?;
    Ok(Json(cellset_dto(
        snap.cube(),
        &cellset,
        snap.version(),
        sandbox,
    )))
}

/// Lower a parsed MDX [`Query`](epiphany_mdx::Query) onto `cube` into a core
/// [`View`]. Each axis's set is flattened into its per-dimension component sets,
/// each component is evaluated to element indices over its dimension, and those
/// are mapped to member names for an [`AxisSpec::Members`](epiphany_core::AxisSpec).
/// Crossjoin component order is preserved (first = outermost), matching the
/// engine's first-slowest tuple convention.
///
/// Only `COLUMNS` and `ROWS` axes are supported. Missing dimensions are not
/// auto-filled into the context: `execute_view`'s coverage check surfaces a 422
/// if the MDX omits one (as for an ad-hoc view).
fn view_from_mdx(
    cube: &Cube,
    cube_name: &str,
    query: &epiphany_mdx::Query,
) -> Result<View, ApiError> {
    if query.cube != cube_name {
        return Err(ApiError::unprocessable(
            "MDX_CUBE_MISMATCH",
            format!(
                "the query targets cube '{}' but the request is for cube '{cube_name}'",
                query.cube
            ),
        ));
    }

    let mut rows: epiphany_core::Axis = Vec::new();
    let mut columns: epiphany_core::Axis = Vec::new();
    for (axis, set) in &query.axes {
        let target = match axis {
            epiphany_mdx::AxisName::Columns => &mut columns,
            epiphany_mdx::AxisName::Rows => &mut rows,
            epiphany_mdx::AxisName::Ordinal(_) => {
                return Err(ApiError::unprocessable(
                    "MDX_UNSUPPORTED_AXIS",
                    "only COLUMNS and ROWS axes are supported",
                ));
            }
        };
        // The parser rejects a repeated COLUMNS/ROWS axis, but guard anyway so a
        // duplicate never silently overwrites the first.
        if !target.is_empty() {
            return Err(ApiError::unprocessable(
                "MDX_UNSUPPORTED_AXIS",
                "only COLUMNS and ROWS axes are supported",
            ));
        }
        for component in flatten_crossjoin(set) {
            let name = axis_dimension(component).ok_or_else(|| {
                ApiError::unprocessable(
                    "MDX_EVAL_ERROR",
                    "cannot determine the dimension for an axis set; qualify members as [Dim].[Member]",
                )
            })?;
            let dim = cube
                .dimensions()
                .iter()
                .find(|d| d.name() == name)
                .ok_or_else(|| {
                    ApiError::unprocessable(
                        "UNKNOWN_DIMENSION",
                        format!("unknown dimension '{name}' in cube '{}'", cube.name()),
                    )
                })?;
            let indices = epiphany_mdx::evaluate(component, dim)
                .map_err(|e| ApiError::unprocessable("MDX_EVAL_ERROR", e.to_string()))?;
            let members = indices
                .iter()
                .map(|&i| {
                    dim.element(i)
                        .map(|el| el.name.clone())
                        .map_err(|e| ApiError::unprocessable("MDX_EVAL_ERROR", e.to_string()))
                })
                .collect::<Result<Vec<String>, ApiError>>()?;
            target.push(epiphany_core::AxisSpec::Members {
                dimension: name.to_string(),
                members,
            });
        }
    }

    let context = query
        .slicer
        .iter()
        .map(|m| {
            if m.path.len() >= 2 {
                Ok((m.path[0].clone(), m.name().to_string()))
            } else {
                Err(ApiError::unprocessable(
                    "MDX_EVAL_ERROR",
                    "WHERE members must be qualified as [Dim].[Member]",
                ))
            }
        })
        .collect::<Result<Vec<(String, String)>, ApiError>>()?;

    Ok(View {
        name: "mdx".to_string(),
        cube: cube_name.to_string(),
        owner: None,
        visibility: Visibility::Public,
        rows,
        columns,
        context,
        suppress_zeros: false,
    })
}

/// Flatten an axis [`SetExpr`](epiphany_mdx::SetExpr) into its per-dimension
/// component sets: a `Crossjoin(l, r)` becomes `flatten(l) ++ flatten(r)` (the
/// parser left-folds N-ary crossjoins into nested binaries, so this recovers the
/// original component order, first = outermost), anything else is a single
/// component. Each component is a single-dimension set the evaluator accepts.
fn flatten_crossjoin(expr: &epiphany_mdx::SetExpr) -> Vec<&epiphany_mdx::SetExpr> {
    match expr {
        epiphany_mdx::SetExpr::Crossjoin(l, r) => {
            let mut out = flatten_crossjoin(l);
            out.extend(flatten_crossjoin(r));
            out
        }
        other => vec![other],
    }
}

/// Determine the dimension a single-dimension axis set selects from, by walking to
/// the first member reference and reading its dimension qualifier. For
/// `Member`/`Children`/`Descendants` the qualifier is `path[0]` (the pivot UI
/// always emits `[Dim].[Member]`); for `Members` the reference *is* the dimension,
/// so its name is used. `Set`/`Filter`/`Order` recurse into their first inner set.
/// `None` when no qualifier can be found (an unqualified member, or an empty set).
fn axis_dimension(expr: &epiphany_mdx::SetExpr) -> Option<&str> {
    match expr {
        epiphany_mdx::SetExpr::Members(r) => Some(r.name()),
        epiphany_mdx::SetExpr::Member(r)
        | epiphany_mdx::SetExpr::Children(r)
        | epiphany_mdx::SetExpr::Descendants(r) => {
            if r.path.len() >= 2 {
                Some(r.path[0].as_str())
            } else {
                None
            }
        }
        epiphany_mdx::SetExpr::Set(items) => items.iter().find_map(axis_dimension),
        epiphany_mdx::SetExpr::Filter(inner, _) | epiphany_mdx::SetExpr::Order(inner, _, _) => {
            axis_dimension(inner)
        }
        // A Crossjoin spans dimensions; callers flatten before asking, so this is
        // only reached for a malformed nested set. Take the left component's dim.
        epiphany_mdx::SetExpr::Crossjoin(l, _) => axis_dimension(l),
    }
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

fn cellset_dto(cube: &Cube, cs: &Cellset, version: u64, sandbox: Option<&Sandbox>) -> CellsetDto {
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
        row_dimensions: cs.row_dimensions.clone(),
        column_dimensions: cs.column_dimensions.clone(),
        row_tuples,
        column_tuples,
        context: cs
            .context
            .iter()
            .cloned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::{AxisSpec, Cube, Dimension};

    /// A 3-dimension cube (`Region`, `Product`, `Measure`) for lowering tests.
    fn cube() -> Cube {
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        region.add_leaf("South");
        let mut product = Dimension::new("Product");
        product.add_leaf("Widgets");
        product.add_leaf("Gadgets");
        let mut measure = Dimension::new("Measure");
        measure.add_leaf("Sales");
        Cube::new("Sales", vec![region, product, measure]).unwrap()
    }

    fn members(spec: &AxisSpec) -> (&str, &[String]) {
        match spec {
            AxisSpec::Members { dimension, members } => (dimension.as_str(), members.as_slice()),
            other => panic!("expected Members, got {other:?}"),
        }
    }

    #[test]
    fn lowers_single_dimension_axes_and_slicer() {
        let q = epiphany_mdx::parse_query(
            "SELECT { [Region].[North], [Region].[South] } ON COLUMNS, \
             { [Product].[Widgets] } ON ROWS FROM [Sales] WHERE ( [Measure].[Sales] )",
        )
        .unwrap();
        let view = view_from_mdx(&cube(), "Sales", &q).unwrap();

        assert_eq!(view.columns.len(), 1);
        let (dim, ms) = members(&view.columns[0]);
        assert_eq!(dim, "Region");
        assert_eq!(ms, &["North".to_string(), "South".to_string()]);

        let (dim, ms) = members(&view.rows[0]);
        assert_eq!(dim, "Product");
        assert_eq!(ms, &["Widgets".to_string()]);

        assert_eq!(view.context, vec![("Measure".to_string(), "Sales".to_string())]);
    }

    #[test]
    fn flattens_crossjoin_preserving_component_order() {
        let q = epiphany_mdx::parse_query(
            "SELECT CrossJoin({ [Region].[North] }, { [Product].[Widgets], [Product].[Gadgets] }) \
             ON COLUMNS, { [Measure].[Sales] } ON ROWS FROM [Sales]",
        )
        .unwrap();
        let view = view_from_mdx(&cube(), "Sales", &q).unwrap();

        // Two components on the column axis, outermost (Region) first.
        assert_eq!(view.columns.len(), 2);
        assert_eq!(members(&view.columns[0]).0, "Region");
        assert_eq!(members(&view.columns[1]).0, "Product");
        assert_eq!(
            members(&view.columns[1]).1,
            &["Widgets".to_string(), "Gadgets".to_string()]
        );
    }

    #[test]
    fn rejects_cube_mismatch() {
        let q = epiphany_mdx::parse_query(
            "SELECT { [Region].[North] } ON COLUMNS, { [Measure].[Sales] } ON ROWS FROM [Other]",
        )
        .unwrap();
        let err = view_from_mdx(&cube(), "Sales", &q).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn rejects_unknown_dimension() {
        let q = epiphany_mdx::parse_query(
            "SELECT { [Nope].[X] } ON COLUMNS, { [Measure].[Sales] } ON ROWS FROM [Sales]",
        )
        .unwrap();
        let err = view_from_mdx(&cube(), "Sales", &q).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn axis_dimension_reads_the_qualifier() {
        use epiphany_mdx::{MemberRef, SetExpr};
        let member = SetExpr::Member(MemberRef::new(vec!["Region".into(), "North".into()]));
        assert_eq!(axis_dimension(&member), Some("Region"));

        let set = SetExpr::Set(vec![member.clone()]);
        assert_eq!(axis_dimension(&set), Some("Region"));

        let members = SetExpr::Members(MemberRef::new(vec!["Region".into()]));
        assert_eq!(axis_dimension(&members), Some("Region"));

        // Unqualified bare member: no dimension can be determined.
        let bare = SetExpr::Member(MemberRef::new(vec!["North".into()]));
        assert_eq!(axis_dimension(&bare), None);
    }
}
