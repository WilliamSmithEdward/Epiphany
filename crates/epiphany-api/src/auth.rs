//! Authentication: the session extractor and the auth endpoints.
//!
//! A handler that takes [`AuthPrincipal`] is gated behind a valid session
//! (bearer token or session cookie); extraction fails with a 401 envelope. M2
//! authorization is authenticated plus admin-or-not; per-object authorization is
//! Phase 7.

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use epiphany_security::{verify_password, AuditAction, Principal, SecurityError};

use crate::authz::audit;
use crate::{ApiError, AppState};

const SESSION_COOKIE: &str = "epiphany_session";

/// Upper bound on a submitted username or password, in bytes. A credential longer
/// than this can never authenticate, so it is rejected with 400 before the login
/// guard, the Argon2 hasher, or the audit log ever sees it. This bounds the
/// attacker-controlled strings an unauthenticated client can pin in memory (the
/// login-guard map and the audit records) and keeps a hostile body from bloating
/// the audit log (ADR-0017).
const MAX_CREDENTIAL_LEN: usize = 256;

/// Reject an over-long username/password (see [`MAX_CREDENTIAL_LEN`]) with 400.
fn check_credential_bounds(username: &str, password: &str) -> Result<(), ApiError> {
    if username.len() > MAX_CREDENTIAL_LEN || password.len() > MAX_CREDENTIAL_LEN {
        return Err(ApiError::bad_request("credential too long"));
    }
    Ok(())
}

/// An authenticated request: the verified principal plus the session token (so a
/// handler can revoke it on logout). Extracting it requires a valid session.
pub struct AuthPrincipal {
    pub principal: Principal,
    pub token: String,
}

impl AuthPrincipal {
    /// A synthetic principal carrying only a username, with no session token
    /// (ADR-0035). Used to authorize a scheduled flow run as the flow's recorded
    /// owner: every `require_*` gate re-resolves the caller's rights from the live
    /// security store by username (so a revoked grant takes effect immediately),
    /// so the synthetic `is_admin`/`groups` here are never consulted. Fail-closed:
    /// an unknown username resolves to no access.
    pub(crate) fn synthetic(username: impl Into<String>) -> Self {
        Self {
            principal: Principal {
                username: username.into(),
                is_admin: false,
                groups: Vec::new(),
            },
            token: String::new(),
        }
    }
}

impl FromRequestParts<AppState> for AuthPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = token_from_parts(parts)
            .ok_or_else(|| ApiError::unauthorized("missing session token"))?;
        let now = state.clock.now_millis();
        let principal = state
            .sessions
            .lock()
            .expect("session store mutex")
            .lookup(&token, now)
            .ok_or_else(|| ApiError::unauthorized("invalid or expired session"))?;
        // Enforce a pending password change (ADR-0017): until it is done, only the
        // minimal recovery routes are reachable. Re-resolved from the live store
        // so the gate lifts immediately on change, with no re-login.
        if !MUST_CHANGE_ALLOWED.contains(&parts.uri.path())
            && state
                .security
                .lock()
                .expect("security mutex")
                .must_change_password(&principal.username)
        {
            return Err(ApiError::forbidden("password change required"));
        }
        Ok(AuthPrincipal { principal, token })
    }
}

/// Routes a user with a pending forced password change may still reach: change
/// the password, see who they are, or log out.
const MUST_CHANGE_ALLOWED: [&str; 3] = [
    "/api/v1/auth/password",
    "/api/v1/auth/logout",
    "/api/v1/auth/me",
];

/// Pull the token from `Authorization: Bearer <t>`, else from the session cookie.
fn token_from_parts(parts: &Parts) -> Option<String> {
    if let Some(auth) = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            return Some(token.trim().to_string());
        }
    }
    let cookies = parts.headers.get(header::COOKIE)?.to_str().ok()?;
    let prefix = format!("{SESSION_COOKIE}=");
    cookies
        .split(';')
        .map(str::trim)
        .find_map(|pair| pair.strip_prefix(&prefix))
        .map(str::to_string)
}

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct UserInfo {
    username: String,
    is_admin: bool,
    must_change_password: bool,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
    user: UserInfo,
}

/// `POST /api/v1/auth/login` -> a session token plus the user summary. The token
/// is also set as an HttpOnly, SameSite=Strict cookie for browser convenience.
pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let now = state.clock.now_millis();
    // Bound the credential sizes before touching the guard, hasher, or audit log
    // (ADR-0017): an over-long credential can never authenticate anyway.
    check_credential_bounds(&req.username, &req.password)?;
    // Lockout check before verifying the password (ADR-0017): a locked account
    // never runs Argon2, removing a CPU/timing lever.
    if state
        .login_guard
        .lock()
        .expect("login guard mutex")
        .is_locked(&req.username, now)
    {
        audit(&state, &req.username, AuditAction::Login, None, false);
        return Err(ApiError::too_many_requests(
            "too many failed login attempts; try again later",
        ));
    }
    // The Argon2id verify is deliberately expensive (~50-100ms of CPU) and runs
    // even for an unknown username (a dummy verify, to remove the enumeration
    // timing channel, ADR-0017). Two hardening properties (A4):
    //
    // 1. It runs on the BLOCKING pool, not an async worker, so a burst of bogus
    //    logins cannot pin the runtime's worker threads and stall every other
    //    endpoint (the lock-free snapshot reads, health checks).
    // 2. It runs OFF the global security mutex: copy the stored PHC hash (or the
    //    fixed dummy hash for an unknown user) out under a brief lock, DROP the
    //    lock, then run the KDF against the copy via the lock-free `verify_password`
    //    seam. Previously the lock was held for the whole KDF, so a handful of
    //    concurrent bogus logins (rotating usernames to dodge the per-username
    //    lockout) serialized every request's authorization gate behind the KDF — an
    //    unauthenticated whole-API DoS. The dummy hash on the not-found path keeps
    //    the unknown-user cost identical to a wrong-password cost.
    let phc = state
        .security
        .lock()
        .expect("security mutex")
        .password_hash_for(&req.username)
        // An unknown user still yields a hash (the store returns the dummy), so this
        // is only ever `None` if the seam contract changes; treat that as a denial.
        .unwrap_or_default();
    let password = req.password.clone();
    let verified = tokio::task::spawn_blocking(move || verify_password(&password, &phc))
        .await
        .map_err(|_| ApiError::internal())?;
    // A verify against the dummy hash never succeeds (no real password can), so an
    // unknown user always lands on the denial arm; a known user with the correct
    // password re-resolves their live principal under a brief lock (admin flag and
    // groups may have changed since the hash was read).
    let principal = if verified {
        match state
            .security
            .lock()
            .expect("security mutex")
            .principal(&req.username)
        {
            Some(principal) => {
                state
                    .login_guard
                    .lock()
                    .expect("login guard mutex")
                    .record_success(&req.username);
                principal
            }
            // The user vanished between the hash read and here (an admin delete
            // raced the login): deny, fail-closed, as an unknown user would.
            None => {
                state
                    .login_guard
                    .lock()
                    .expect("login guard mutex")
                    .record_failure(&req.username, now);
                audit(&state, &req.username, AuditAction::Login, None, false);
                return Err(ApiError::unauthorized("invalid credentials"));
            }
        }
    } else {
        // Count the failure (may trip the lockout) and audit it (no password in the
        // record, RG-13).
        state
            .login_guard
            .lock()
            .expect("login guard mutex")
            .record_failure(&req.username, now);
        audit(&state, &req.username, AuditAction::Login, None, false);
        return Err(ApiError::unauthorized("invalid credentials"));
    };
    let must_change_password = state
        .security
        .lock()
        .expect("security mutex")
        .must_change_password(&principal.username);
    let token = state
        .sessions
        .lock()
        .expect("session store mutex")
        .create(principal.clone(), now);
    audit(&state, &principal.username, AuditAction::Login, None, true);

    let body = LoginResponse {
        token: token.clone(),
        user: UserInfo {
            username: principal.username,
            is_admin: principal.is_admin,
            must_change_password,
        },
    };
    let mut cookie = format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/");
    if state.secure_cookies {
        // Over HTTPS, keep the session cookie off any plain-HTTP request (RG-12).
        cookie.push_str("; Secure");
    }
    let mut response = Json(body).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    Ok(response)
}

/// `POST /api/v1/auth/logout` -> revoke the current session.
pub async fn logout(State(state): State<AppState>, auth: AuthPrincipal) -> StatusCode {
    state
        .sessions
        .lock()
        .expect("session store mutex")
        .revoke(&auth.token);
    audit(
        &state,
        &auth.principal.username,
        AuditAction::Logout,
        None,
        true,
    );
    StatusCode::NO_CONTENT
}

#[derive(Serialize)]
pub(crate) struct MeResponse {
    username: String,
    is_admin: bool,
    groups: Vec<String>,
    /// Whether a forced password change is still pending (ADR-0017). The web
    /// layer needs this to restore the forced-rotation screen on a page reload;
    /// `/auth/me` is in MUST_CHANGE_ALLOWED, so it resolves during a pending
    /// change. Computed from the live security store, as `login` does.
    must_change_password: bool,
    /// The caller's OWN effective persona (`business` | `modeler` | `admin`),
    /// derived server-side from their grants (ADR-0020). Lets the web shell
    /// progressively disclose modeler/admin chrome for a NON-admin without reading
    /// the admin-only `/acl/grants` list: a business/data-entry user is never shown
    /// Rules/Flows/Dimensions/MDX, a modeler is. Self-only and least-privilege — it
    /// reflects only this caller's capabilities. Re-resolved per request from the
    /// live store, so a granted/revoked capability takes effect immediately.
    persona: &'static str,
}

/// `GET /api/v1/auth/me` -> the current principal.
pub async fn me(State(state): State<AppState>, auth: AuthPrincipal) -> Json<MeResponse> {
    let must_change_password = state
        .security
        .lock()
        .expect("security mutex")
        .must_change_password(&auth.principal.username);
    // The persona is derived from the caller's own grants (self-only), so the web
    // shell can gate modeler/admin chrome without the admin-only grant list.
    let persona = crate::authz::caller_persona(&state, &auth.principal.username);
    Json(MeResponse {
        username: auth.principal.username,
        is_admin: auth.principal.is_admin,
        groups: auth.principal.groups,
        must_change_password,
        persona,
    })
}

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

/// `POST /api/v1/auth/password` -> change the current user's password.
pub async fn change_password(
    State(state): State<AppState>,
    auth: AuthPrincipal,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<StatusCode, ApiError> {
    let now = state.clock.now_millis();
    let username = auth.principal.username.clone();
    check_credential_bounds(&username, &req.new_password)?;
    // The current password is attacker-supplied over a hijacked session, so subject
    // it to the SAME lockout as /auth/login (ADR-0017): otherwise a stolen token
    // could brute-force the real password (the one credential a password change is
    // meant to rotate away) with no lockout ever tripping. Locked -> 429.
    if state
        .login_guard
        .lock()
        .expect("login guard mutex")
        .is_locked(&username, now)
    {
        audit(&state, &username, AuditAction::UserChange, None, false);
        return Err(ApiError::too_many_requests(
            "too many failed attempts; try again later",
        ));
    }
    // Verify the CURRENT password OFF the global security mutex (A4), the same
    // lock-free discipline as `login`: copy the caller's stored PHC hash (or the
    // dummy for a vanished user) under a brief lock, DROP it, then run the KDF via
    // `verify_password` on the blocking pool. The current password is
    // attacker-supplied over a hijacked session, so an online guess of it is the
    // expensive, DoS-relevant check; running it off-lock means a wrong guess never
    // serializes the authorization gate every other request needs, and never takes
    // the write lock at all.
    let phc = state
        .security
        .lock()
        .expect("security mutex")
        .password_hash_for(&username)
        .unwrap_or_default();
    let current = req.current_password.clone();
    let current_ok = tokio::task::spawn_blocking(move || verify_password(&current, &phc))
        .await
        .map_err(|_| ApiError::internal())?;
    if !current_ok {
        // Count the failure (may trip the lockout) and AUDIT it, so an online guess
        // of the current password is both rate-limited and visible to an operator
        // reviewing the log (no password in the record, RG-13). A vanished user
        // verifies against the dummy and lands here too (fail-closed).
        state
            .login_guard
            .lock()
            .expect("login guard mutex")
            .record_failure(&username, now);
        audit(&state, &username, AuditAction::UserChange, None, false);
        return Err(ApiError::unauthorized("current password is incorrect"));
    }
    // The current password is proven; apply the change on the blocking pool (a
    // rehash of the NEW password is one more Argon2 cost). The store re-verifies for
    // atomicity, enforces the strength policy, rehashes, and persists under commit.
    // A concurrent password change could make the re-verify fail (a benign race);
    // surface it as the same 401 as a wrong current password.
    let result = {
        let security = state.security.clone();
        let current = req.current_password.clone();
        let new = req.new_password.clone();
        let user = username.clone();
        tokio::task::spawn_blocking(move || {
            security
                .lock()
                .expect("security mutex")
                .change_password(&user, &current, &new)
        })
        .await
        .map_err(|_| ApiError::internal())?
    };
    if let Err(e) = result {
        return Err(match e {
            SecurityError::IncorrectPassword => {
                state
                    .login_guard
                    .lock()
                    .expect("login guard mutex")
                    .record_failure(&username, now);
                audit(&state, &username, AuditAction::UserChange, None, false);
                ApiError::unauthorized("current password is incorrect")
            }
            // The strength-policy reason is client-safe (no password material).
            SecurityError::WeakPassword(_) => ApiError::bad_request(e.to_string()),
            _ => ApiError::internal(),
        });
    }
    // A correct current password clears the failure counter (as a successful login
    // does), so a legitimate rotation after a few typos does not stay penalized.
    state
        .login_guard
        .lock()
        .expect("login guard mutex")
        .record_success(&username);
    // Revoke this user's OTHER sessions (ADR-0017): a password change invalidates
    // every token issued before it, so a stolen session elsewhere cannot outlive
    // the change. The caller's current session is kept (it just proved the correct
    // current password), so the change does not log them out of this device.
    state
        .sessions
        .lock()
        .expect("session mutex")
        .revoke_user_except(&auth.principal.username, &auth.token);
    audit(
        &state,
        &auth.principal.username,
        AuditAction::UserChange,
        None,
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}
