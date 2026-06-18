//! Centralized authorization (ADR-0023) and audit emission (ADR-0010).
//!
//! Every gate re-resolves the caller's access against the live security store
//! per request (so a revoked grant takes effect immediately) and, on denial,
//! emits an `AccessDenied` audit record and returns 403. The gates are
//! [`require_kind_access`] (modular per-object-kind grants), [`require_cube_access`]
//! (cube read/write at the `Cube` kind), [`require_manage_cubes`] (cube
//! lifecycle), [`require_admin`] (server-admin surface), and the element-level
//! checks ([`require_element_write`], [`element_mask`]). [`audit`] is the shared,
//! best-effort emit helper. Centralizing them keeps every handler's denial shape
//! and audit trail uniform.

use epiphany_core::ElementMask;
use epiphany_engine::ReadSnapshot;
use epiphany_security::{AccessLevel, AuditAction, ObjectKind, ObjectRef};

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

/// Whether `username` is a server admin, re-resolved from the live store. An
/// unknown user is not an admin (fail-closed). Used by the flow reader to decide
/// global-dimension member visibility for a run-as principal (ADR-0035).
pub(crate) fn is_admin(state: &AppState, username: &str) -> bool {
    state
        .security
        .lock()
        .expect("security mutex")
        .principal(username)
        .is_some_and(|p| p.is_admin)
}

/// The caller's cube-level access at the `Cube` kind (ADR-0023), for filtering
/// lists without erroring.
pub(crate) fn cube_level(state: &AppState, username: &str, cube: &str) -> AccessLevel {
    state
        .security
        .lock()
        .expect("security mutex")
        .cube_access(username, cube)
}

/// Gate a request on cube-level access (ADR-0023): `Cube:Read` to read, `Cube:Write`
/// to write cell data. Fail-closed -- an ungranted cube denies a non-admin; the
/// server admin bypasses. On denial this emits `AccessDenied` and returns 403.
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

/// Gate a request on per-object-kind access (ADR-0023): the caller must hold at
/// least `needed` on `kind` within `cube` (or globally when `cube` is `None`),
/// resolved from the live store via `effective()` (fail-closed; admin bypasses;
/// `Cube:Admin` over a cube confers `Write` on its kinds). On denial emits
/// `AccessDenied` tagged with the kind and returns 403.
pub(crate) fn require_kind_access(
    state: &AppState,
    auth: &AuthPrincipal,
    kind: ObjectKind,
    cube: Option<&str>,
    needed: AccessLevel,
) -> Result<(), ApiError> {
    let level = {
        let store = state.security.lock().expect("security mutex");
        match store.principal(&auth.principal.username) {
            Some(p) => store.effective(&p, kind, cube),
            None => AccessLevel::None,
        }
    };
    if level >= needed {
        Ok(())
    } else {
        let obj = match cube {
            Some(c) => ObjectRef::in_cube(kind, c, ""),
            None => ObjectRef::global(kind, ""),
        };
        audit(
            state,
            &auth.principal.username,
            AuditAction::AccessDenied,
            Some(&obj),
            false,
        );
        Err(ApiError::forbidden(format!(
            "you do not have {} access to {} objects here",
            needed.as_str(),
            kind.as_str()
        )))
    }
}

/// Gate cube lifecycle (create/delete) on the cube-management permission
/// (ADR-0023): a server admin, or a holder of a global `Cube:Admin` grant. On
/// denial emits `AccessDenied` and returns 403.
pub(crate) fn require_manage_cubes(state: &AppState, auth: &AuthPrincipal) -> Result<(), ApiError> {
    let ok = {
        let store = state.security.lock().expect("security mutex");
        store
            .principal(&auth.principal.username)
            .is_some_and(|p| store.can_manage_cubes(&p))
    };
    if ok {
        Ok(())
    } else {
        audit(
            state,
            &auth.principal.username,
            AuditAction::AccessDenied,
            None,
            false,
        );
        Err(ApiError::forbidden(
            "cube management requires administrator or a global cube-admin grant",
        ))
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
    element_mask_for(state, &auth.principal.username, snapshot)
}

/// As [`element_mask`], but for an arbitrary principal by username (ADR-0035): a
/// scheduled flow run reads and writes as the flow's recorded owner, not a request
/// caller, so the reader resolves that owner's mask. Same fail-closed semantics:
/// an unknown principal is masked against nothing here (the caller still gates
/// reads, and `denied_registry_elements`/writes deny an unknown principal).
pub(crate) fn element_mask_for(
    state: &AppState,
    username: &str,
    snapshot: &ReadSnapshot,
) -> Option<ElementMask> {
    let security = state.security.lock().expect("security mutex");
    let principal = security.principal(username)?;
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

/// The element NAMES of a registry dimension the caller may not see, as the UNION
/// across the dimension's referencing cubes (ADR-0033): a member denied by an
/// element ACL in ANY referencing cube is suppressed from the global dimension
/// read, fail-closed (hidden in one place means hidden globally). An admin gets
/// an empty set (sees everything); an unknown principal gets every name (deny
/// all, defense in depth). An unreferenced dimension has no cube ACL context, so
/// nothing is denied. `element_names` is the dimension's full member list.
pub(crate) fn denied_registry_elements(
    state: &AppState,
    auth: &AuthPrincipal,
    dim_name: &str,
    referencing: &[String],
    element_names: &[String],
) -> std::collections::HashSet<String> {
    let security = state.security.lock().expect("security mutex");
    let Some(principal) = security.principal(&auth.principal.username) else {
        return element_names.iter().cloned().collect();
    };
    if principal.is_admin {
        return std::collections::HashSet::new();
    }
    let mut denied = std::collections::HashSet::new();
    for cube in referencing {
        if !security.has_element_acls(cube, dim_name) {
            continue;
        }
        for name in element_names {
            if !security.element_readable(&principal, cube, dim_name, name) {
                denied.insert(name.clone());
            }
        }
    }
    denied
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
