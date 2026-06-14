//! Security administration (Phase 7, admin only): users, groups, object and
//! element ACLs (ADR-0015), and the audit-log query (ADR-0010). Every route is
//! gated by [`require_admin`] and every mutation emits an audit record.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_security::{
    AccessLevel, AuditAction, AuditFilter, ObjectKind, ObjectRef, SecurityError, Subject,
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

// ---- object ACLs ----

#[derive(Serialize)]
pub(crate) struct ObjectGrantDto {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cube: Option<String>,
    name: String,
    subject_kind: String,
    subject: String,
    level: String,
}

#[derive(Serialize)]
pub(crate) struct ObjectGrantListDto {
    grants: Vec<ObjectGrantDto>,
}

/// `GET /api/v1/acl/objects` -> all object grants (admin).
pub(crate) async fn list_object_acls(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<ObjectGrantListDto>, ApiError> {
    require_admin(&state, &auth)?;
    let security = state.security.lock().expect("security mutex");
    let mut grants = Vec::new();
    for (obj, list) in security.object_acls() {
        for (subject, level) in &list.users {
            grants.push(ObjectGrantDto {
                kind: obj.kind.as_str().to_string(),
                cube: obj.cube.clone(),
                name: obj.name.clone(),
                subject_kind: "user".to_string(),
                subject: subject.clone(),
                level: level.as_str().to_string(),
            });
        }
        for (subject, level) in &list.groups {
            grants.push(ObjectGrantDto {
                kind: obj.kind.as_str().to_string(),
                cube: obj.cube.clone(),
                name: obj.name.clone(),
                subject_kind: "group".to_string(),
                subject: subject.clone(),
                level: level.as_str().to_string(),
            });
        }
    }
    Ok(Json(ObjectGrantListDto { grants }))
}

#[derive(Deserialize)]
pub(crate) struct ObjectGrantBody {
    kind: String,
    #[serde(default)]
    cube: Option<String>,
    name: String,
    subject_kind: String,
    subject: String,
    /// `none` revokes the grant.
    level: String,
}

/// `PUT /api/v1/acl/objects` -> grant or (with level `none`) revoke object access
/// (admin).
pub(crate) async fn put_object_acl(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Json(body): Json<ObjectGrantBody>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    let kind = ObjectKind::parse(&body.kind)
        .ok_or_else(|| ApiError::bad_request(format!("unknown object kind '{}'", body.kind)))?;
    let level = parse_level(&body.level)?;
    let subject = parse_subject(&body.subject_kind, &body.subject)?;
    let obj = ObjectRef {
        kind,
        cube: body.cube.clone(),
        name: body.name.clone(),
    };
    state
        .security
        .lock()
        .expect("security mutex")
        .set_object_access(obj.clone(), &subject, level)
        .map_err(map_security_err)?;
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
