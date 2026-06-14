//! Centralized authorization (ADR-0015) and audit emission (ADR-0010).
//!
//! [`require_access`] is the single object-security gate: it re-resolves the
//! caller's access against the live security store (composing the owner and
//! public fallbacks the API supplies from the model snapshot), and on denial
//! emits an `AccessDenied` audit record and returns 403. [`audit`] is the
//! shared, best-effort emit helper. Centralizing both keeps every handler's
//! denial shape and audit trail uniform.

use epiphany_security::{AccessLevel, AuditAction, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::{ApiError, AppState};

/// Emit one audit record (ADR-0010). Best-effort on the write path: a failed
/// append is swallowed so it can never fail the request (a full disk must not
/// lock the server out). The timestamp comes from the injected clock (ADR-0009).
pub(crate) fn audit(
    state: &AppState,
    actor: &str,
    action: AuditAction,
    obj: Option<&ObjectRef>,
    allowed: bool,
) {
    let ts = state.clock.now_millis();
    let (kind, target) = obj.map(audit_ref).unwrap_or_default();
    if let Ok(mut log) = state.audit.lock() {
        let _ = log.append(ts, actor, action, kind, target, allowed);
    }
}

/// The (object kind, target) strings for an audit record. The target carries the
/// cube prefix for a cube-scoped object, never any payload (RG-13).
fn audit_ref(obj: &ObjectRef) -> (String, String) {
    let target = match &obj.cube {
        Some(cube) => format!("{cube}/{}", obj.name),
        None => obj.name.clone(),
    };
    (obj.kind.as_str().to_string(), target)
}

/// The caller's cube-level access (ADR-0015 decision 2a), for filtering lists
/// without erroring.
pub(crate) fn cube_level(state: &AppState, username: &str, cube: &str) -> AccessLevel {
    state
        .security
        .lock()
        .expect("security mutex")
        .cube_access(username, cube)
}

/// Gate a request on cube-level access (ADR-0015 decision 2a: a cube is open
/// until an admin restricts it). Used for cube, cell, rule, and flow handlers,
/// which are cube-scoped. On denial this emits `AccessDenied` and returns 403.
pub(crate) fn require_cube_access(
    state: &AppState,
    auth: &AuthPrincipal,
    cube: &str,
    needed: AccessLevel,
) -> Result<(), ApiError> {
    if cube_level(state, &auth.principal.username, cube) >= needed {
        Ok(())
    } else {
        let obj = ObjectRef::cube(cube);
        audit(
            state,
            &auth.principal.username,
            AuditAction::AccessDenied,
            Some(&obj),
            false,
        );
        Err(ApiError::forbidden("you do not have access to this cube"))
    }
}

/// Gate a request on object access (ADR-0015). `owner`/`public` are the object's
/// owner and visibility from the snapshot (pass `None`/`false` for objects that
/// have neither). On denial this emits an `AccessDenied` record and returns 403.
pub(crate) fn require_access(
    state: &AppState,
    auth: &AuthPrincipal,
    obj: &ObjectRef,
    needed: AccessLevel,
    owner: Option<&str>,
    public: bool,
) -> Result<(), ApiError> {
    let level = {
        let security = state.security.lock().expect("security mutex");
        security.resolve_access(&auth.principal.username, obj, owner, public)
    };
    if level >= needed {
        Ok(())
    } else {
        audit(
            state,
            &auth.principal.username,
            AuditAction::AccessDenied,
            Some(obj),
            false,
        );
        Err(ApiError::forbidden("you do not have access to this object"))
    }
}
