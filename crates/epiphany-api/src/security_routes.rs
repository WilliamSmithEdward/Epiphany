//! Security administration (admin only): users, groups, the modular per-object-
//! kind grants (ADR-0023), element ACLs (ADR-0015), and the audit-log query
//! (ADR-0010). Every route is gated by [`require_admin`] and every mutation emits
//! an audit record.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_security::{
    AccessLevel, AuditAction, AuditFilter, ObjectKind, ObjectRef, Scope, SecurityError, Subject,
};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_admin};
use crate::{ApiError, AppState};

/// Map a security-store error to the HTTP envelope. The name-collision and
/// not-found messages are safe to surface; every other cause (I/O, hashing,
/// parse) is logged-and-generic so no internal detail leaks (RG-12).
fn map_security_err(e: SecurityError) -> ApiError {
    match e {
        SecurityError::UserExists(_) => ApiError::conflict(e.to_string()),
        SecurityError::UserNotFound(_) => ApiError::not_found(e.to_string()),
        // The strength-policy reason is client-safe (no password material).
        SecurityError::WeakPassword(_) => ApiError::bad_request(e.to_string()),
        _ => ApiError::internal(),
    }
}

fn parse_subject(kind: &str, name: &str) -> Result<Subject, ApiError> {
    match kind {
        "user" => Ok(Subject::User(name.to_string())),
        "group" => Ok(Subject::Group(name.to_string())),
        other => Err(ApiError::bad_request(format!(
            "subject_kind must be 'user' or 'group', got '{other}'"
        ))),
    }
}

fn parse_level(s: &str) -> Result<AccessLevel, ApiError> {
    AccessLevel::parse(s).ok_or_else(|| {
        ApiError::bad_request(format!("level must be none/read/write/admin, got '{s}'"))
    })
}

// ---- users ----

#[derive(Serialize)]
pub(crate) struct UserDto {
    username: String,
    is_admin: bool,
    groups: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct UserListDto {
    users: Vec<UserDto>,
}

/// `GET /api/v1/users` -> all users (admin).
pub(crate) async fn list_users(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<UserListDto>, ApiError> {
    require_admin(&state, &auth)?;
    let users = state
        .security
        .lock()
        .expect("security mutex")
        .list_users()
        .into_iter()
        .map(|u| UserDto {
            username: u.username,
            is_admin: u.is_admin,
            groups: u.groups,
        })
        .collect();
    Ok(Json(UserListDto { users }))
}

#[derive(Deserialize)]
pub(crate) struct CreateUserBody {
    username: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
    #[serde(default)]
    groups: Vec<String>,
}

/// `POST /api/v1/users` -> create a user (admin).
pub(crate) async fn create_user(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Json(body): Json<CreateUserBody>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    if body.username.trim().is_empty() || body.password.is_empty() {
        return Err(ApiError::bad_request("username and password are required"));
    }
    state
        .security
        .lock()
        .expect("security mutex")
        .create_user_with_groups(&body.username, &body.password, body.is_admin, &body.groups)
        .map_err(map_security_err)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::UserChange,
        Some(&ObjectRef::global(ObjectKind::User, &body.username)),
        true,
    );
    Ok(StatusCode::CREATED)
}

#[derive(Deserialize)]
pub(crate) struct PatchUserBody {
    #[serde(default)]
    is_admin: Option<bool>,
    #[serde(default)]
    groups: Option<Vec<String>>,
    #[serde(default)]
    password: Option<String>,
}

/// `PATCH /api/v1/users/{username}` -> update a user's admin flag, groups, or
/// password (admin).
pub(crate) async fn patch_user(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(username): Path<String>,
    Json(body): Json<PatchUserBody>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    {
        let mut security = state.security.lock().expect("security mutex");
        if let Some(is_admin) = body.is_admin {
            security
                .set_user_admin(&username, is_admin)
                .map_err(map_security_err)?;
        }
        if let Some(groups) = &body.groups {
            security
                .set_user_groups(&username, groups)
                .map_err(map_security_err)?;
        }
        if let Some(password) = &body.password {
            security
                .reset_password(&username, password)
                .map_err(map_security_err)?;
        }
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::UserChange,
        Some(&ObjectRef::global(ObjectKind::User, &username)),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/users/{username}` -> delete a user (admin).
pub(crate) async fn delete_user(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(username): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    let removed = state
        .security
        .lock()
        .expect("security mutex")
        .delete_user(&username)
        .map_err(map_security_err)?;
    if !removed {
        return Err(ApiError::not_found(format!("no user '{username}'")));
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::UserChange,
        Some(&ObjectRef::global(ObjectKind::User, &username)),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---- groups ----

#[derive(Serialize)]
pub(crate) struct GroupListDto {
    groups: Vec<String>,
}

/// `GET /api/v1/groups` -> all groups (admin).
pub(crate) async fn list_groups(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<GroupListDto>, ApiError> {
    require_admin(&state, &auth)?;
    let groups = state.security.lock().expect("security mutex").list_groups();
    Ok(Json(GroupListDto { groups }))
}

#[derive(Deserialize)]
pub(crate) struct CreateGroupBody {
    name: String,
}

/// `POST /api/v1/groups` -> create a group (admin).
pub(crate) async fn create_group(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Json(body): Json<CreateGroupBody>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("group name is required"));
    }
    state
        .security
        .lock()
        .expect("security mutex")
        .create_group(&body.name)
        .map_err(map_security_err)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::GroupChange,
        Some(&ObjectRef::global(ObjectKind::Group, &body.name)),
        true,
    );
    Ok(StatusCode::CREATED)
}

/// `DELETE /api/v1/groups/{name}` -> delete a group (admin).
pub(crate) async fn delete_group(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    let removed = state
        .security
        .lock()
        .expect("security mutex")
        .delete_group(&name)
        .map_err(map_security_err)?;
    if !removed {
        return Err(ApiError::not_found(format!("no group '{name}'")));
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::GroupChange,
        Some(&ObjectRef::global(ObjectKind::Group, &name)),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---- modular per-object-kind grants (ADR-0023) ----

#[derive(Serialize)]
pub(crate) struct GrantDto {
    subject_kind: String,
    subject: String,
    scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cube: Option<String>,
    kind: String,
    level: String,
}

#[derive(Serialize)]
pub(crate) struct GrantListDto {
    grants: Vec<GrantDto>,
}

#[derive(Deserialize)]
pub(crate) struct GrantBody {
    subject_kind: String,
    subject: String,
    /// `global` or `cube`.
    scope: String,
    #[serde(default)]
    cube: Option<String>,
    kind: String,
    /// `none` revokes.
    level: String,
}

/// `GET /api/v1/acl/grants` -> all modular per-kind grants (admin; ADR-0023).
pub(crate) async fn list_grants(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<GrantListDto>, ApiError> {
    require_admin(&state, &auth)?;
    let store = state.security.lock().expect("security mutex");
    let mut grants = Vec::new();
    for ((scope, kind), list) in store.grants() {
        let (scope_tag, cube) = match scope {
            Scope::Global => ("global", None),
            Scope::Cube(c) => ("cube", Some(c.clone())),
        };
        for (user, level) in &list.users {
            grants.push(GrantDto {
                subject_kind: "user".to_string(),
                subject: user.clone(),
                scope: scope_tag.to_string(),
                cube: cube.clone(),
                kind: kind.as_str().to_string(),
                level: level.as_str().to_string(),
            });
        }
        for (group, level) in &list.groups {
            grants.push(GrantDto {
                subject_kind: "group".to_string(),
                subject: group.clone(),
                scope: scope_tag.to_string(),
                cube: cube.clone(),
                kind: kind.as_str().to_string(),
                level: level.as_str().to_string(),
            });
        }
    }
    Ok(Json(GrantListDto { grants }))
}

/// `PUT /api/v1/acl/grants` -> set or (with level `none`) revoke a per-kind grant
/// for a user or group (admin; ADR-0023).
pub(crate) async fn put_grant(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Json(body): Json<GrantBody>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    let subject = parse_subject(&body.subject_kind, &body.subject)?;
    let kind = ObjectKind::parse(&body.kind)
        .ok_or_else(|| ApiError::bad_request(format!("unknown object kind '{}'", body.kind)))?;
    let level = parse_level(&body.level)?;
    let scope = match body.scope.as_str() {
        "global" => Scope::Global,
        "cube" => {
            let cube = body
                .cube
                .clone()
                .ok_or_else(|| ApiError::bad_request("scope 'cube' requires a cube name"))?;
            Scope::Cube(cube)
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "scope must be 'global' or 'cube', got '{other}'"
            )))
        }
    };
    state
        .security
        .lock()
        .expect("security mutex")
        .set_grant(&subject, scope.clone(), kind, level)
        .map_err(map_security_err)?;
    let obj = match &scope {
        Scope::Global => ObjectRef::global(kind, ""),
        Scope::Cube(c) => ObjectRef::in_cube(kind, c, ""),
    };
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&obj),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---- element ACLs ----

#[derive(Serialize)]
pub(crate) struct ElementGrantDto {
    cube: String,
    dimension: String,
    element: String,
    subject_kind: String,
    subject: String,
    level: String,
}

#[derive(Serialize)]
pub(crate) struct ElementGrantListDto {
    grants: Vec<ElementGrantDto>,
}

/// `GET /api/v1/acl/elements` -> all element grants (admin).
pub(crate) async fn list_element_acls(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<ElementGrantListDto>, ApiError> {
    require_admin(&state, &auth)?;
    let security = state.security.lock().expect("security mutex");
    let mut grants = Vec::new();
    for ((cube, dimension, element), list) in security.element_acls() {
        for (subject, level) in &list.users {
            grants.push(ElementGrantDto {
                cube: cube.clone(),
                dimension: dimension.clone(),
                element: element.clone(),
                subject_kind: "user".to_string(),
                subject: subject.clone(),
                level: level.as_str().to_string(),
            });
        }
        for (subject, level) in &list.groups {
            grants.push(ElementGrantDto {
                cube: cube.clone(),
                dimension: dimension.clone(),
                element: element.clone(),
                subject_kind: "group".to_string(),
                subject: subject.clone(),
                level: level.as_str().to_string(),
            });
        }
    }
    Ok(Json(ElementGrantListDto { grants }))
}

#[derive(Deserialize)]
pub(crate) struct ElementGrantBody {
    cube: String,
    dimension: String,
    element: String,
    subject_kind: String,
    subject: String,
    /// `none` revokes the grant.
    level: String,
}

/// `PUT /api/v1/acl/elements` -> grant or (with level `none`) revoke element
/// access (admin).
pub(crate) async fn put_element_acl(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Json(body): Json<ElementGrantBody>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    let level = parse_level(&body.level)?;
    let subject = parse_subject(&body.subject_kind, &body.subject)?;
    state
        .security
        .lock()
        .expect("security mutex")
        .set_element_access(&body.cube, &body.dimension, &body.element, &subject, level)
        .map_err(map_security_err)?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::in_cube(
            ObjectKind::Dimension,
            &body.cube,
            &body.dimension,
        )),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---- audit query ----

#[derive(Deserialize)]
pub(crate) struct AuditQuery {
    actor: Option<String>,
    action: Option<String>,
    target: Option<String>,
    outcome: Option<String>,
    from: Option<u64>,
    to: Option<u64>,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct AuditRecordDto {
    seq: u64,
    timestamp_millis: u64,
    actor: String,
    action: String,
    object_kind: String,
    target: String,
    allowed: bool,
}

#[derive(Serialize)]
pub(crate) struct AuditListDto {
    records: Vec<AuditRecordDto>,
}

/// `GET /api/v1/audit` -> query the audit log with filters (admin).
pub(crate) async fn query_audit(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<AuditListDto>, ApiError> {
    require_admin(&state, &auth)?;
    let filter = AuditFilter {
        actor: q.actor,
        action: q.action.as_deref().and_then(AuditAction::parse),
        target: q.target,
        allowed: q.outcome.as_deref().map(|o| o == "allowed"),
        from: q.from,
        to: q.to,
        offset: q.offset,
        limit: q.limit,
    };
    let records = state
        .audit
        .lock()
        .expect("audit mutex")
        .query(&filter)
        .into_iter()
        .map(|r| AuditRecordDto {
            seq: r.seq,
            timestamp_millis: r.timestamp_millis,
            actor: r.actor,
            action: r.action.as_str().to_string(),
            object_kind: r.object_kind,
            target: r.target,
            allowed: r.allowed,
        })
        .collect();
    Ok(Json(AuditListDto { records }))
}
