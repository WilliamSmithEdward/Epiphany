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
use epiphany_engine::{Engine, ReadSnapshot};
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef, Principal};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, element_mask, require_cube_access};
use crate::calc_factory::RuleCoverage;
use crate::dto::{
    AxisMemberDto, AxisSpecBody, AxisSpecDto, CellsetCellDto, CellsetDto, ContextEntryDto,
    MdxPreviewRequest, MdxQueryRequest, MemberDto, MembersResponse, SubsetBody, SubsetDto,
    SubsetListResponse, SuppressedDto, ViewBody, ViewDto, ViewListResponse,
};
use crate::resolve::kind_str;
use crate::routes::{blocking_commit, snapshot};
use crate::sandbox_routes::{resolve_sandbox, SandboxSelector};
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

/// The `(cube name, current version)` of every OTHER cube the target cube's rules
/// read across cubes, for the view-cache key (item 1: cross-cube staleness).
///
/// A rule like `Sales.Revenue = Units * FX!Rate` reads cube `FX`; a write to `FX`
/// bumps `FX`'s version, not `Sales`', so keying only on `Sales`' version would
/// serve the pre-write `FX` value indefinitely (there is no TTL). The referenced
/// cube NAMES are carried on the parsed rule AST (`CellRef.cube = Some(name)`), so
/// parsing the target's own rule source recovers them WITHOUT compiling every
/// cube's rules (the expensive `PinnedRegistry` build) -- the same parse the
/// resolver does for the target cube on a miss, and cheap on a hit. A cube named by
/// a rule that no longer exists (or one that fails to resolve) is skipped for the
/// version lookup: it cannot contribute a value, and the target read itself will
/// surface the compile error. The result is deterministic (the key normalizes
/// order); duplicates are harmless.
///
/// This parses on every cellset read (hit or miss). Rule sources are small, so the
/// cost is negligible next to executing (or the resolver's own compile on a miss);
/// a per-(cube, version) memo would remove even that, and is a clean follow-on if a
/// profile ever shows it.
fn cross_cube_dep_versions(snap: &ReadSnapshot, engine: &Engine) -> Vec<(String, u64)> {
    let source = &snap.rules().source;
    if source.trim().is_empty() {
        return Vec::new();
    }
    let doc = match epiphany_calc::rules::parse(source) {
        Ok(doc) => doc,
        // A source that no longer parses has no analyzable dependency set; the read
        // path fails loud on it anyway (fail-loud rule drop). Treat as no deps.
        Err(_) => return Vec::new(),
    };
    let target = snap.cube().name();
    let mut names: Vec<String> = Vec::new();
    for rule in &doc.rules {
        collect_ref_cubes(&rule.formula, target, &mut names);
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| engine.version(&name).map(|v| (name, v)))
        .collect()
}

/// Collect the cube names of cross-cube cell references in a rule formula
/// (`CellRef.cube == Some(name)` for a cube other than `target`). Walks the AST;
/// same-cube references (`cube == None`) and references to `target` are ignored.
fn collect_ref_cubes(expr: &epiphany_calc::rules::Expr, target: &str, out: &mut Vec<String>) {
    use epiphany_calc::rules::{Condition, Expr, FuncArg};
    match expr {
        Expr::Cell(cell) => {
            if let Some(cube) = &cell.cube {
                if cube != target {
                    out.push(cube.clone());
                }
            }
        }
        Expr::Neg(inner) => collect_ref_cubes(inner, target, out),
        Expr::Bin { left, right, .. } => {
            collect_ref_cubes(left, target, out);
            collect_ref_cubes(right, target, out);
        }
        Expr::If {
            cond,
            then,
            otherwise,
        } => {
            collect_cond_cubes(cond, target, out);
            collect_ref_cubes(then, target, out);
            if let Some(o) = otherwise {
                collect_ref_cubes(o, target, out);
            }
        }
        Expr::Func(call) => {
            for arg in &call.args {
                if let FuncArg::Expr(e) = arg {
                    collect_ref_cubes(e, target, out);
                }
            }
        }
        Expr::Number(_) | Expr::Str(_) => {}
    }
    // A condition can only appear inside `If`; walk it there.
    fn collect_cond_cubes(cond: &Condition, target: &str, out: &mut Vec<String>) {
        match cond {
            Condition::And(a, b) | Condition::Or(a, b) => {
                collect_cond_cubes(a, target, out);
                collect_cond_cubes(b, target, out);
            }
            Condition::Not(c) => collect_cond_cubes(c, target, out),
            Condition::Compare { left, op: _, right } => {
                collect_ref_cubes(left, target, out);
                collect_ref_cubes(right, target, out);
            }
        }
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
    // Run the commit (writer lock + fsync) off the async workers (A2).
    let (cube_c, subset_c) = (cube.clone(), subset.clone());
    let outcome = blocking_commit(&state.engine, move |engine| {
        engine.define_subset(&cube_c, None, subset_c)
    })
    .await?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectCreate,
        Some(&ObjectRef::in_cube(ObjectKind::Subset, &cube, &subset.name)),
        true,
    );
    // A1: the commit-ordered change feed emits the change from inside the engine
    // commit (cube-read-filtered per subscriber); the handler no longer sends it.
    let _ = outcome.version;
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
    // Authorize BEFORE any existence probe so an unauthorized caller cannot
    // distinguish 404 (no such subset) from 403, and use the visibility-filtered
    // lookup so a private subset a caller cannot see 404s uniformly rather than
    // leaking its existence via a 403 ownership error (matches the GET path).
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    let snap = snapshot(&state, &cube)?;
    let existing = visible_subset(&snap, &auth.principal, &dim, &name)?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    // Preserve the original owner across an edit.
    let subset = subset_from_body(name, dim.clone(), existing.owner.clone(), &body)?;
    resolve_subset(snap.cube(), &subset, state.evaluator())?;
    // Run the commit (writer lock + fsync) off the async workers (A2).
    let (cube_c, subset_c) = (cube.clone(), subset.clone());
    let outcome = blocking_commit(&state.engine, move |engine| {
        engine.define_subset(&cube_c, None, subset_c)
    })
    .await?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(ObjectKind::Subset, &cube, &subset.name)),
        true,
    );
    // A1: the commit-ordered change feed emits the change from inside the engine
    // commit (cube-read-filtered per subscriber); the handler no longer sends it.
    let _ = outcome.version;
    Ok(Json(subset_dto(&subset)))
}

/// `DELETE /cubes/{cube}/dimensions/{dim}/subsets/{name}` -> delete a subset.
pub(crate) async fn delete_subset(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, dim, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    // Authorize before the existence probe, and hide an invisible subset behind a
    // uniform 404 (see `replace_subset`).
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    let snap = snapshot(&state, &cube)?;
    let existing = visible_subset(&snap, &auth.principal, &dim, &name)?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    // Run the commit (writer lock + fsync) off the async workers (A2).
    let (cube_c, dim_c, name_c) = (cube.clone(), dim.clone(), name.clone());
    let outcome = blocking_commit(&state.engine, move |engine| {
        engine.delete_subset(&cube_c, None, &dim_c, &name_c)
    })
    .await?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::in_cube(ObjectKind::Subset, &cube, &name)),
        true,
    );
    // A1: the commit-ordered change feed emits the change from inside the engine
    // commit (cube-read-filtered per subscriber); the handler no longer sends it.
    let _ = outcome.version;
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
    // Resolve the split zero-suppression flags, honoring a legacy `suppress_zeros`
    // in the request body for input back-compat (ADR-0003).
    let (suppress_zero_rows, suppress_zero_columns) = body.suppression();
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
        suppress_zero_rows,
        suppress_zero_columns,
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
        suppress_zero_rows: v.suppress_zero_rows,
        suppress_zero_columns: v.suppress_zero_columns,
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
    // Run the commit (writer lock + fsync) off the async workers (A2).
    let (cube_c, view_c) = (cube.clone(), view.clone());
    let outcome = blocking_commit(&state.engine, move |engine| {
        engine.define_view(&cube_c, None, view_c)
    })
    .await?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectCreate,
        Some(&ObjectRef::in_cube(ObjectKind::View, &cube, &view.name)),
        true,
    );
    // A1: the commit-ordered change feed emits the change from inside the engine
    // commit (cube-read-filtered per subscriber); the handler no longer sends it.
    let _ = outcome.version;
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
    // Authorize before the existence probe, and hide an invisible view behind a
    // uniform 404 (see `replace_subset`).
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    let snap = snapshot(&state, &cube)?;
    let existing = visible_view(&snap, &auth.principal, &name)?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    let view = view_from_body(name, cube.clone(), existing.owner.clone(), &body)?;
    // Run the commit (writer lock + fsync) off the async workers (A2).
    let (cube_c, view_c) = (cube.clone(), view.clone());
    let outcome = blocking_commit(&state.engine, move |engine| {
        engine.define_view(&cube_c, None, view_c)
    })
    .await?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(ObjectKind::View, &cube, &view.name)),
        true,
    );
    // A1: the commit-ordered change feed emits the change from inside the engine
    // commit (cube-read-filtered per subscriber); the handler no longer sends it.
    let _ = outcome.version;
    Ok(Json(view_dto(&view)))
}

/// `DELETE /cubes/{cube}/views/{name}` -> delete a view.
pub(crate) async fn delete_view(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    // Authorize before the existence probe, and hide an invisible view behind a
    // uniform 404 (see `replace_subset`).
    require_cube_access(&state, &auth, &cube, AccessLevel::Write)?;
    let snap = snapshot(&state, &cube)?;
    let existing = visible_view(&snap, &auth.principal, &name)?;
    if !can_modify(&auth.principal, &existing.owner) {
        return Err(forbidden());
    }
    // Run the commit (writer lock + fsync) off the async workers (A2).
    let (cube_c, name_c) = (cube.clone(), name.clone());
    let outcome = blocking_commit(&state.engine, move |engine| {
        engine.delete_view(&cube_c, None, &name_c)
    })
    .await?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::in_cube(ObjectKind::View, &cube, &name)),
        true,
    );
    // A1: the commit-ordered change feed emits the change from inside the engine
    // commit (cube-read-filtered per subscriber); the handler no longer sends it.
    let _ = outcome.version;
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
    // Cross-cube rule dependencies are part of the key: a write to a referenced
    // cube must invalidate this entry even though the target's version is unchanged.
    let dep_versions = cross_cube_dep_versions(&snap, &state.engine);
    let cellset = state.view_cache.get_or_compute(
        crate::view_cache::ViewRead {
            cube: &cube,
            version: snap.version(),
            dep_versions,
            view,
            sandbox,
            mask: mask.as_ref(),
            is_adhoc: false,
        },
        || {
            let resolver = state.cells.resolver_with(&snap, sandbox, mask.as_ref());
            // Resolve axis subsets through the visibility filter (ADR-0015): a
            // private subset the caller cannot see is treated as unknown so its
            // membership never leaks through the cellset's tuples.
            execute_view(
                snap.cube(),
                view,
                &*resolver,
                &visible_subset_lookup(&snap, &auth.principal),
                state.evaluator(),
                mask.as_ref(),
            )
        },
    )?;
    // Rule coverage drives the per-cell `editable` flag (ADR-0040) so the grid
    // renders rule-calculated cells read-only, matching the write path. Cheap
    // no-op for a cube with no rules; not part of the cached cellset.
    let coverage = RuleCoverage::build(&state.engine, &snap);
    Ok(Json(cellset_dto(
        snap.cube(),
        &cellset,
        snap.version(),
        sandbox,
        &coverage,
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
    execute_adhoc_view(&state, &auth, &cube, &snap, &selector, &view)
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
    execute_adhoc_view(&state, &auth, &cube, &snap, &selector, &view)
}

/// Resolve the active sandbox + element mask and execute an ad-hoc `view` through
/// the view cache's ad-hoc pool (ADR-0028), returning the cellset DTO. Shared by
/// [`execute_adhoc`] and [`execute_mdx`], which differ only in how they build the
/// [`View`] (`view_from_body` vs `view_from_mdx`); the `Read` gate and snapshot are
/// applied by each caller before the view is built. The resolver is constructed
/// lazily inside the cache closure, so a cache hit skips rule compilation.
fn execute_adhoc_view(
    state: &AppState,
    auth: &AuthPrincipal,
    cube: &str,
    snap: &ReadSnapshot,
    selector: &SandboxSelector,
    view: &View,
) -> Result<Json<CellsetDto>, ApiError> {
    let sandbox_name = resolve_sandbox(snap, &auth.principal, selector)?;
    let sandbox = sandbox_name
        .as_deref()
        .and_then(|n| snap.model().sandbox(n));
    let mask = element_mask(state, auth, snap);
    // Cross-cube rule dependencies join the key so a write to a referenced cube
    // invalidates this ad-hoc entry (item 1), same as the saved-view path.
    let dep_versions = cross_cube_dep_versions(snap, &state.engine);
    let cellset = state.view_cache.get_or_compute(
        crate::view_cache::ViewRead {
            cube,
            version: snap.version(),
            dep_versions,
            view,
            sandbox,
            mask: mask.as_ref(),
            is_adhoc: true,
        },
        || {
            let resolver = state.cells.resolver_with(snap, sandbox, mask.as_ref());
            // A private subset the caller cannot see is treated as unknown so its
            // membership never leaks through an ad-hoc view's tuples (ADR-0015).
            execute_view(
                snap.cube(),
                view,
                &*resolver,
                &visible_subset_lookup(snap, &auth.principal),
                state.evaluator(),
                mask.as_ref(),
            )
        },
    )?;
    // Per-cell `editable` reflects rule coverage (ADR-0040), same as the saved
    // path; a no-rules cube pays nothing.
    let coverage = RuleCoverage::build(&state.engine, snap);
    Ok(Json(cellset_dto(
        snap.cube(),
        &cellset,
        snap.version(),
        sandbox,
        &coverage,
    )))
}

/// A subset-resolving closure for the execute paths that hides subsets the caller
/// may not see: a private subset owned by another user resolves to `None` (an
/// `UnknownSubset` at the call site), so an ad-hoc or saved view cannot enumerate
/// its membership through the resulting tuples (ADR-0015). Mirrors the visibility
/// gate the direct subset endpoints enforce via [`visible_subset`].
fn visible_subset_lookup<'a>(
    snap: &'a ReadSnapshot,
    principal: &'a Principal,
) -> impl Fn(&str, &str) -> Option<&'a Subset> + 'a {
    move |dim: &str, name: &str| {
        snap.subset(dim, name)
            .filter(|s| can_read(principal, &s.owner, s.visibility))
    }
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
        suppress_zero_rows: false,
        suppress_zero_columns: false,
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

/// Precompute the `overlaid` flag for every cell of the grid (row-major, indexed
/// by ordinal): true when a cell's exact leaf coordinate is a what-if override in
/// `sandbox`. Absent a sandbox, or if the cube position of an axis dimension
/// cannot be found, every flag is `false`.
///
/// Resolution is O(R + C), not O(R x C): the cube position of each axis dimension
/// and the context contribution are computed once; each row tuple and each column
/// tuple is resolved to element indices once; then each cell merges its row and
/// column halves into a reused scratch coordinate (name-then-alias via
/// `Dimension::resolve`, matching execution) for a single `BTreeMap` membership
/// check, with no per-cell allocation.
fn overlaid_grid(cube: &Cube, cs: &Cellset, sandbox: Option<&Sandbox>) -> Vec<bool> {
    let Some(sb) = sandbox else {
        return vec![false; cs.cells.len()];
    };
    // A cell is overlaid iff its exact leaf is a what-if override (ADR-0014).
    cell_coord_flags(cube, cs, |coord| sb.cells.contains_key(coord))
}

/// Resolve each cell's full element coordinate (context + row tuple + column
/// tuple) once per row/column — O(R + C) resolutions, not O(R x C) — and test it
/// with `hit`, returning a per-cell flag grid. A cell whose context/tuple fails to
/// resolve is `false`. Shared by the overlaid-flag grid and the rule-coverage grid
/// (ADR-0040) so both compute the same coordinates the same cheap way.
fn cell_coord_flags(cube: &Cube, cs: &Cellset, mut hit: impl FnMut(&[u32]) -> bool) -> Vec<bool> {
    // The cube position each axis/context dimension writes into the coordinate.
    let pos_of = |dim: &str| cube.dimensions().iter().position(|d| d.name() == dim);
    let Some(row_pos) = cs
        .row_dimensions
        .iter()
        .map(|d| pos_of(d))
        .collect::<Option<Vec<_>>>()
    else {
        return vec![false; cs.cells.len()];
    };
    let Some(col_pos) = cs
        .column_dimensions
        .iter()
        .map(|d| pos_of(d))
        .collect::<Option<Vec<_>>>()
    else {
        return vec![false; cs.cells.len()];
    };

    // The base coordinate carries the context (resolved once); row/col slots are
    // overwritten per cell. If any context member fails to resolve, no cell can be
    // an exact override, so every flag is false.
    let mut base = vec![0u32; cube.rank()];
    for (dim, member) in &cs.context {
        let Some(p) = pos_of(dim) else {
            return vec![false; cs.cells.len()];
        };
        let Some(idx) = cube.dimension(p).resolve(member) else {
            return vec![false; cs.cells.len()];
        };
        base[p] = idx;
    }

    // Resolve each row tuple and each column tuple to element indices once. A
    // tuple that fails to resolve yields `None` and its cells are never flagged.
    let resolve_tuples = |tuples: &[Vec<String>], positions: &[usize]| -> Vec<Option<Vec<u32>>> {
        tuples
            .iter()
            .map(|tuple| {
                tuple
                    .iter()
                    .enumerate()
                    .map(|(k, name)| cube.dimension(positions[k]).resolve(name))
                    .collect::<Option<Vec<u32>>>()
            })
            .collect()
    };
    let row_idx = resolve_tuples(&cs.row_tuples, &row_pos);
    let col_idx = resolve_tuples(&cs.column_tuples, &col_pos);

    let ncols = cs.column_tuples.len().max(1);
    let mut flags = vec![false; cs.cells.len()];
    let mut scratch = base.clone();
    for (r, row) in row_idx.iter().enumerate() {
        let Some(row) = row else { continue };
        // Fill this row's slots once; column slots are patched per cell below.
        scratch.copy_from_slice(&base);
        for (k, &idx) in row.iter().enumerate() {
            scratch[row_pos[k]] = idx;
        }
        for (c, col) in col_idx.iter().enumerate() {
            let ordinal = r * ncols + c;
            if ordinal >= flags.len() {
                continue;
            }
            let Some(col) = col else { continue };
            for (k, &idx) in col.iter().enumerate() {
                scratch[col_pos[k]] = idx;
            }
            flags[ordinal] = hit(scratch.as_slice());
        }
    }
    flags
}

fn cellset_dto(
    cube: &Cube,
    cs: &Cellset,
    version: u64,
    sandbox: Option<&Sandbox>,
    coverage: &RuleCoverage,
) -> CellsetDto {
    // Resolve member names name-then-alias (`Dimension::resolve`), matching the
    // resolution `execute_view` uses at execution time: a context or axis member
    // pinned by its alias must map to the same element here, or editability and the
    // overlaid marker are silently wrong for an alias-addressed cell.
    let leaf_of = |dim_name: &str, member: &str| -> bool {
        cube.dimensions()
            .iter()
            .find(|d| d.name() == dim_name)
            .and_then(|d| d.resolve(member).and_then(|i| d.element(i).ok()))
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
                    .and_then(|d| d.resolve(name).and_then(|i| d.element(i).ok()))
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
    // Whether a member is a String-kind element (name-then-alias, matching
    // execution): a cell whose context/row/column tuple contains one is a string
    // cell and renders `kind:"string"` even when unpopulated, never a numeric zero.
    let string_of = |dim_name: &str, member: &str| -> bool {
        cube.dimensions()
            .iter()
            .find(|d| d.name() == dim_name)
            .and_then(|d| d.resolve(member).and_then(|i| d.element(i).ok()))
            .map(|el| el.kind.is_string())
            .unwrap_or(false)
    };
    let tuple_string = |dims: &[String], tuple: &[String]| {
        tuple
            .iter()
            .enumerate()
            .any(|(k, m)| string_of(&dims[k], m))
    };

    let context_leaf = cs.context.iter().all(|(d, m)| leaf_of(d, m));
    let context_string = cs.context.iter().any(|(d, m)| string_of(d, m));
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
    let row_string: Vec<bool> = cs
        .row_tuples
        .iter()
        .map(|t| tuple_string(&cs.row_dimensions, t))
        .collect();
    let col_string: Vec<bool> = cs
        .column_tuples
        .iter()
        .map(|t| tuple_string(&cs.column_dimensions, t))
        .collect();

    // Precompute the per-cell `overlaid` flags in O(R + C) coordinate
    // resolutions instead of O(R x C): the same row tuple is otherwise re-resolved
    // once per column and vice versa. Each half-coordinate (context + one row
    // tuple, and one column tuple) is resolved once, then merged per cell into a
    // reused scratch buffer with no per-cell allocation (ADR-0014: only the exact
    // overridden leaf is flagged). Absent a sandbox every cell is `false`.
    let overlaid_flags = overlaid_grid(cube, cs, sandbox);
    // Rule-covered leaves render read-only (ADR-0040) so the `editable` flag the
    // client reads matches what the write path accepts. Computed only when the
    // cube actually has rules (a no-rules cube keeps the pure leaf-ness flag and
    // pays nothing), reusing the same O(R + C) coordinate walk as `overlaid`.
    let covered_flags = if coverage.has_rules() {
        cell_coord_flags(cube, cs, |coord| coverage.covers(cube, coord))
    } else {
        Vec::new()
    };

    let ncols = cs.column_tuples.len().max(1);
    let cells = cs
        .cells
        .iter()
        .enumerate()
        .map(|(ordinal, value)| {
            let r = ordinal / ncols;
            let c = ordinal % ncols;
            let overlaid = overlaid_flags.get(ordinal).copied().unwrap_or(false);
            let leaf_editable = context_leaf
                && row_leaf.get(r).copied().unwrap_or(false)
                && col_leaf.get(c).copied().unwrap_or(false);
            // The string and error channels are parallel to `cells` (core
            // `Cellset`): a populated string renders as `kind:"string"` with its
            // text (never a fabricated numeric zero), and a per-cell error renders
            // as `kind:"error"` with the message so one failing cell does not blank
            // the grid. An error takes precedence over a value; a string element is
            // never editable through the numeric write path.
            if let Some(err) = cs.cell_errors.get(ordinal).and_then(|e| e.clone()) {
                CellsetCellDto {
                    value: None,
                    kind: "error",
                    editable: false,
                    ordinal,
                    overlaid,
                    error: Some(err),
                }
            } else if let Some(text) = cs.cell_strings.get(ordinal).and_then(|s| s.clone()) {
                CellsetCellDto {
                    value: Some(text),
                    kind: "string",
                    editable: false,
                    ordinal,
                    overlaid,
                    error: None,
                }
            } else if context_string
                || row_string.get(r).copied().unwrap_or(false)
                || col_string.get(c).copied().unwrap_or(false)
            {
                // An unpopulated string cell: text `null`, still `kind:"string"`
                // (not a numeric zero) and not numerically editable.
                CellsetCellDto {
                    value: None,
                    kind: "string",
                    editable: false,
                    ordinal,
                    overlaid,
                    error: None,
                }
            } else {
                // A rule-covered leaf (ADR-0040) is read-only: the rule computes
                // it, so a stored write would be shadowed on the next read.
                let covered = covered_flags.get(ordinal).copied().unwrap_or(false);
                CellsetCellDto {
                    value: Some(value.to_string()),
                    kind: "numeric",
                    editable: leaf_editable && !covered,
                    ordinal,
                    overlaid,
                    error: None,
                }
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
    use epiphany_core::{AttributeKind, AttributeValue, AxisSpec, Cube, Dimension, Fixed};

    /// `cellset_dto` must resolve context/member names name-then-alias, matching
    /// execution: a cell whose context pins a leaf by its ALIAS must still be
    /// `editable` and, under a sandbox override, `overlaid`. Before the fix these
    /// used `index_of` (name-only), so an alias-addressed context silently made the
    /// whole grid read-only and dropped the what-if marker.
    #[test]
    fn cellset_dto_resolves_context_alias_for_editable_and_overlaid() {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        region.add_attribute("Alias", AttributeKind::Alias);
        region
            .set_attribute(north, "Alias", AttributeValue::Text("N.A.".into()))
            .unwrap();
        let mut measure = Dimension::new("Measure");
        let sales = measure.add_leaf("Sales");
        let cube = Cube::new("Sales", vec![region, measure]).unwrap();

        // A 1x1 cellset: Measure/Sales on rows, Region pinned by its ALIAS in the
        // context (a single empty column tuple = one column).
        let cs = Cellset {
            row_dimensions: vec!["Measure".into()],
            column_dimensions: vec![],
            row_tuples: vec![vec!["Sales".into()]],
            column_tuples: vec![vec![]],
            context: vec![("Region".into(), "N.A.".into())],
            cells: vec![Fixed::from(7)],
            cell_strings: vec![None],
            cell_errors: vec![None],
            suppressed_row_tuples: vec![],
            suppressed_column_tuples: vec![],
        };

        // No sandbox: the alias-addressed context still resolves to a leaf, so the
        // cell is editable (was false before the fix).
        let dto = cellset_dto(&cube, &cs, 1, None, &RuleCoverage::none());
        assert!(dto.cells[0].editable);
        assert!(!dto.cells[0].overlaid);

        // With a sandbox override on (North, Sales), the alias-addressed cell is
        // flagged overlaid (was false before the fix).
        let mut sb = Sandbox::new("wi", "ann", 1);
        sb.cells.insert(vec![north, sales], Fixed::from(9));
        let dto = cellset_dto(&cube, &cs, 1, Some(&sb), &RuleCoverage::none());
        assert!(dto.cells[0].overlaid);
        assert!(dto.cells[0].editable);
    }

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

        assert_eq!(
            view.context,
            vec![("Measure".to_string(), "Sales".to_string())]
        );
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
