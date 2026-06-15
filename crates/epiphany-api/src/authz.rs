//! Centralized authorization (ADR-0015) and audit emission (ADR-0010).
//!
//! [`require_access`] is the single object-security gate: it re-resolves the
//! caller's access against the live security store (composing the owner and
//! public fallbacks the API supplies from the model snapshot), and on denial
//! emits an `AccessDenied` audit record and returns 403. [`audit`] is the
//! shared, best-effort emit helper. Centralizing both keeps every handler's
//! denial shape and audit trail uniform.

use epiphany_core::ElementMask;
use epiphany_engine::ReadSnapshot;
use epiphany_security::{AccessLevel, AuditAction, ObjectRef};

use crate::auth::AuthPrincipal;
use crate::dto::CoordMap;
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

/// Gate a request on administrator status, re-resolved from the live store so a
/// demoted admin loses access immediately. On denial emits `AccessDenied` and
/// returns 403. Used for the server-global security-admin surface.
pub(crate) fn require_admin(state: &AppState, auth: &AuthPrincipal) -> Result<(), ApiError> {
    let is_admin = state
        .security
        .lock()
        .expect("security mutex")
        .principal(&auth.principal.username)
        .is_some_and(|p| p.is_admin);
    if is_admin {
        Ok(())
    } else {
        audit(
            state,
            &auth.principal.username,
            AuditAction::AccessDenied,
            None,
            false,
        );
        Err(ApiError::forbidden("administrator access required"))
    }
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

/// Build the caller's element deny mask for a cube snapshot (ADR-0015 decision
/// 5), resolved once under a single security lock. Returns `None` -- the common,
/// zero-cost case -- when the caller is an admin (bypass), unknown, or no element
/// ACL denies them any element of this cube, so the hot path skips the check
/// entirely. The mask is leaf-centric: denying a leaf taints every rollup that
/// includes it (deny-the-rollup); a denied consolidated member is honored only
/// when directly addressed or enumerated on an axis.
pub(crate) fn element_mask(
    state: &AppState,
    auth: &AuthPrincipal,
    snapshot: &ReadSnapshot,
) -> Option<ElementMask> {
    let security = state.security.lock().expect("security mutex");
    let principal = security.principal(&auth.principal.username)?;
    if principal.is_admin {
        return None;
    }
    let cube = snapshot.cube();
    let cube_name = cube.name();
    let mut counts = Vec::with_capacity(cube.rank());
    let mut denied: Vec<Vec<u32>> = Vec::with_capacity(cube.rank());
    let mut any = false;
    for d in 0..cube.rank() {
        let dim = cube.dimension(d);
        counts.push(dim.len());
        let mut dim_denied = Vec::new();
        if security.has_element_acls(cube_name, dim.name()) {
            for (idx, el) in dim.iter_elements().enumerate() {
                if !security.element_readable(&principal, cube_name, dim.name(), &el.name) {
                    dim_denied.push(idx as u32);
                    any = true;
                }
            }
        }
        denied.push(dim_denied);
    }
    if !any {
        return None;
    }
    Some(ElementMask::from_denied(&counts, &denied))
}

/// Gate a write on element-level access (ADR-0015): a write to a coordinate whose
/// every component the caller may not write is rejected with 403. A write targets
/// a leaf, so each component is checked directly (no rollup). On denial emits an
/// `AccessDenied` audit record. An admin (or any cube with no element ACLs)
/// passes.
pub(crate) fn require_element_write(
    state: &AppState,
    auth: &AuthPrincipal,
    cube: &str,
    coord: &CoordMap,
) -> Result<(), ApiError> {
    let denied = {
        let security = state.security.lock().expect("security mutex");
        match security.principal(&auth.principal.username) {
            Some(p) if p.is_admin => false,
            Some(p) => coord
                .iter()
                .any(|(dim, element)| !security.element_writable(&p, cube, dim, element)),
            None => true,
        }
    };
    if denied {
        audit(
            state,
            &auth.principal.username,
            AuditAction::AccessDenied,
            Some(&ObjectRef::cube(cube)),
            false,
        );
        return Err(ApiError::forbidden("you do not have access to this cell"));
    }
    Ok(())
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
