//! Secret-store endpoints (ADR-0030), admin only: set a secret, delete a secret,
//! and list secret NAMES. A secret value is write-only over the API: it is never
//! returned, never logged, and never written to the audit stream (the audit
//! target is the secret name, via `ObjectKind::Secret`). HTTP connections
//! reference a secret by name; the value is resolved into an `Authorization`
//! header only at fetch time.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_security::{AuditAction, ObjectKind, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::authz::{audit, require_admin};
use crate::{ApiError, AppState};

/// The body of a secret-set request. The value is write-only and never echoed.
#[derive(Deserialize)]
pub(crate) struct SecretBody {
    pub value: String,
}

/// The list of secret names (never values).
#[derive(Serialize)]
pub(crate) struct SecretNamesDto {
    pub names: Vec<String>,
}

/// `GET /api/v1/secrets` -> the secret names (admin). Values are never listed.
pub(crate) async fn list_secrets(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<SecretNamesDto>, ApiError> {
    require_admin(&state, &auth)?;
    let names = state.secrets.lock().expect("secret store").names();
    Ok(Json(SecretNamesDto { names }))
}

/// `PUT /api/v1/secrets/{name}` -> set a secret value (admin). The value is
/// stored owner-only and never returned; only the name is audited.
pub(crate) async fn put_secret(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<SecretBody>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    if name.trim().is_empty() {
        return Err(ApiError::bad_request("a secret needs a name"));
    }
    if body.value.is_empty() {
        return Err(ApiError::bad_request("a secret needs a value"));
    }
    state
        .secrets
        .lock()
        .expect("secret store")
        .set(&name, &body.value)
        .map_err(|_| ApiError::internal())?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectUpdate,
        Some(&ObjectRef::global(ObjectKind::Secret, &name)),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/secrets/{name}` -> delete a secret (admin).
pub(crate) async fn delete_secret(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth)?;
    let existed = state
        .secrets
        .lock()
        .expect("secret store")
        .remove(&name)
        .map_err(|_| ApiError::internal())?;
    if !existed {
        return Err(ApiError::not_found(format!("no secret named '{name}'")));
    }
    audit(
        &state,
        &auth.principal.username,
        AuditAction::ObjectDelete,
        Some(&ObjectRef::global(ObjectKind::Secret, &name)),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}
