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

/// Emit one audit record (ADR-0010) timestamped from the injected clock
/// (ADR-0009). The request path uses this; the scheduler uses [`audit_at`] with
/// the frozen fire time instead.
pub(crate) fn audit(
    state: &AppState,
    actor: &str,
    action: AuditAction,
    obj: Option<&ObjectRef>,
    allowed: bool,
) {
    audit_at(state, actor, action, obj, allowed, state.clock.now_millis());
}

/// Emit one audit record at a caller-supplied timestamp. The reconcile loop
/// passes the frozen `fire_millis` so a scheduled firing's audit timestamp is the
/// recorded fire time, never a fresh clock read (ADR-0013 decisions 0 and 9), so
/// it is reproducible under a `ManualClock`. Best-effort: a failed append is
/// swallowed so it can never fail the operation (a full disk must not lock the
/// server out).
pub(crate) fn audit_at(
    state: &AppState,
    actor: &str,
    action: AuditAction,
    obj: Option<&ObjectRef>,
    allowed: bool,
    timestamp_millis: u64,
) {
    let (kind, target) = obj.map(audit_ref).unwrap_or_default();
    if let Ok(mut log) = state.audit.lock() {
        let _ = log.append(timestamp_millis, actor, action, kind, target, allowed);
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

/// Deny a modeler/diagnostic action that evaluates over live cell data (rule and
/// flow test runs, feeder diagnostics) when the caller has any element
/// restriction on the cube (ADR-0015). Such tools surface values and coordinates
/// across the whole cube, so an element-restricted reader could observe a denied
/// member through them; rather than partially redact, deny the action. Admins and
/// callers with no element restriction on this cube pass.
pub(crate) fn deny_if_element_restricted(
    state: &AppState,
    auth: &AuthPrincipal,
    snap: &ReadSnapshot,
) -> Result<(), ApiError> {
    if element_mask(state, auth, snap).is_some() {
        audit(
            state,
            &auth.principal.username,
            AuditAction::AccessDenied,
            Some(&ObjectRef::cube(snap.cube().name())),
            false,
        );
        return Err(ApiError::forbidden(
            "element-restricted users may not run model tests or diagnostics on this cube",
        ));
    }
    Ok(())
}

/// Gate a set of (already-resolved) write coordinates on element-level access
/// (ADR-0015), re-checked against the live store. Used when committing a sandbox:
/// a cell staged when the owner could write it must still be writable now, so an
/// element ACL added after staging blocks the commit. Each coordinate is a leaf,
/// so each component is checked directly. On denial emits `AccessDenied` and 403.
pub(crate) fn require_element_write_indices(
    state: &AppState,
    auth: &AuthPrincipal,
    cube: &str,
    snap: &ReadSnapshot,
    coords: &[Vec<u32>],
) -> Result<(), ApiError> {
    let denied = {
        let security = state.security.lock().expect("security mutex");
        match security.principal(&auth.principal.username) {
            Some(p) if p.is_admin => false,
            Some(p) => {
                let cube_ref = snap.cube();
                coords.iter().any(|coord| {
                    coord.iter().enumerate().any(|(d, &idx)| {
                        let dim = cube_ref.dimension(d);
                        match dim.element(idx) {
                            Ok(el) => !security.element_writable(&p, cube, dim.name(), &el.name),
                            Err(_) => false,
                        }
                    })
                })
            }
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
        return Err(ApiError::forbidden(
            "you do not have access to a cell staged in this sandbox",
        ));
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
