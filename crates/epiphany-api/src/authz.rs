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

/// The caller's own effective PERSONA (ADR-0020), derived server-side from their
/// grants so the web shell can progressively disclose chrome for non-admins without
/// reading the admin-only grant list. Least-privilege and self-only: it reports the
/// caller's OWN capability class, never anyone else's.
///
/// - `admin`: a server administrator, or a holder of the global cube-management
///   permission (can create/delete cubes) — the full admin/modeler chrome.
/// - `modeler`: holds at least `Write` on a MODELING object kind (Dimension, Rule,
///   or Flow) at any scope (global or any cube) — the review's "any dimension/rule/
///   flow write grant => modeler" rule (major-64). Sees the modeler chrome.
/// - `business`: everyone else (data-entry / view consumers) — no modeler or admin
///   machinery.
pub(crate) fn caller_persona(state: &AppState, username: &str) -> &'static str {
    let store = state.security.lock().expect("security mutex");
    let Some(principal) = store.principal(username) else {
        // An unknown principal has no capabilities (fail-closed): plain business.
        return "business";
    };
    if principal.is_admin || store.can_manage_cubes(&principal) {
        return "admin";
    }
    // Modeler iff the caller holds >= Write on any modeling kind at any scope. The
    // grant map is keyed `(scope, kind)`; a modeling-kind row that grants this
    // principal (by user or group) at least Write makes them a modeler.
    let is_modeler = store.grants().iter().any(|((_, kind), list)| {
        matches!(
            kind,
            ObjectKind::Dimension | ObjectKind::Rule | ObjectKind::Flow
        ) && list.level_for(&principal.username, &principal.groups) >= AccessLevel::Write
    });
    if is_modeler {
        "modeler"
    } else {
        "business"
    }
}

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
/// caller, so the reader resolves that owner's mask. Fail-closed: an **unknown**
/// principal (e.g. a deleted user who still owns a scheduled flow) gets an
/// all-deny mask, never `None` (which would mean *unrestricted* reads) — treating
/// an unknown principal as fully denied, matching ADR-0033 and the
/// `denied_registry_elements` union. `None` is returned only for the genuine
/// no-restriction cases: an admin (bypass) or a known principal with no element
/// ACL denying them any element of this cube.
pub(crate) fn element_mask_for(
    state: &AppState,
    username: &str,
    snapshot: &ReadSnapshot,
) -> Option<ElementMask> {
    let security = state.security.lock().expect("security mutex");
    let cube = snapshot.cube();
    let Some(principal) = security.principal(username) else {
        // Unknown principal: deny every element of every dimension (defense in
        // depth), so a read masked by this can never expose a cell.
        let counts: Vec<u32> = (0..cube.rank()).map(|d| cube.dimension(d).len()).collect();
        let denied: Vec<Vec<u32>> = counts.iter().map(|&n| (0..n).collect()).collect();
        return Some(ElementMask::from_denied(&counts, &denied));
    };
    if principal.is_admin {
        return None;
    }
    let cube_name = cube.name();
    let mut counts = Vec::with_capacity(cube.rank());
    let mut denied: Vec<Vec<u32>> = Vec::with_capacity(cube.rank());
    let mut any = false;
    for d in 0..cube.rank() {
        let dim = cube.dimension(d);
        counts.push(dim.len());
        let mut dim_denied = Vec::new();
        // Visit ONLY the ACL'd elements of this dimension, not every element
        // (ADR-0015's "O(1) per coordinate component" force). `element_acls_for`
        // range-probes the sorted flat store for just this `(cube, dim)` band, so
        // one ACL on a 500k-element dimension costs O(log n + k) here instead of a
        // scan of every element with three fresh `String` allocations per probe.
        // An element carrying an ACL is denied iff its list does not grant the
        // caller at least `Read` — the same result `element_readable` yields for an
        // ACL'd element (an element with no ACL is unrestricted and never appears).
        for (el_name, list) in security.element_acls_for(cube_name, dim.name()) {
            if list.level_for(&principal.username, &principal.groups) < AccessLevel::Read {
                if let Some(idx) = dim.index_of(el_name) {
                    dim_denied.push(idx);
                    any = true;
                }
            }
        }
        // `element_acls_for` returns entries in sorted element-name order; the mask
        // builder does not require the denied indices sorted, but keep them ordered
        // for a deterministic, easy-to-reason-about mask.
        dim_denied.sort_unstable();
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
    // Visit ONLY the ACL'd elements of `(cube, dim_name)` in each referencing cube,
    // not every member (ADR-0015/0033). `element_acls_for` range-probes the sorted
    // store for just this band, so the cost is O(log n + k) in the ACL count per
    // referencing cube — not O(members) with three `String` allocations per member,
    // which on a large dimension dominated the global dimension read. An ACL'd
    // element is denied iff its list does not grant the caller at least `Read` (an
    // element with no ACL is unrestricted and never appears here). `element_names`
    // is still the guard that a denied name is actually a member of this dimension.
    let mut denied = std::collections::HashSet::new();
    for cube in referencing {
        for (el_name, list) in security.element_acls_for(cube, dim_name) {
            if list.level_for(&principal.username, &principal.groups) < AccessLevel::Read {
                // Only suppress a name the dimension actually carries (a stale ACL
                // on a since-removed member must not fabricate a phantom denial).
                if let Some(name) = element_names.iter().find(|n| n.as_str() == el_name) {
                    denied.insert(name.clone());
                }
            }
        }
    }
    denied
}

/// Gate a write on element-level access (ADR-0015): a write to a coordinate is
/// rejected with 403 if ANY component dimension element is not writable by the
/// caller (fail-closed). A write targets a leaf, so each component is checked
/// directly (no rollup). On denial emits an `AccessDenied` audit record. An admin
/// (or any cube with no element ACLs) passes.
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
                            // An index that no longer resolves to an element is
                            // treated as DENIED (fail-closed, matching this
                            // module's convention). The engine's later range
                            // validation still returns a precise 422 when apt.
                            Err(_) => true,
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
