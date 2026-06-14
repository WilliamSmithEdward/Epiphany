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

use epiphany_security::{AuditAction, Principal, SecurityError};

use crate::authz::audit;
use crate::{ApiError, AppState};

const SESSION_COOKIE: &str = "epiphany_session";

/// An authenticated request: the verified principal plus the session token (so a
/// handler can revoke it on logout). Extracting it requires a valid session.
pub struct AuthPrincipal {
    pub principal: Principal,
    pub token: String,
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
        Ok(AuthPrincipal { principal, token })
    }
}

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
    let authenticated = state
        .security
        .lock()
        .expect("security mutex")
        .authenticate(&req.username, &req.password);
    let principal = match authenticated {
        Some(principal) => principal,
        None => {
            // Audit the failed attempt (no password in the record, RG-13).
            audit(&state, &req.username, AuditAction::Login, None, false);
            return Err(ApiError::unauthorized("invalid credentials"));
        }
    };
    let must_change_password = state
        .security
        .lock()
        .expect("security mutex")
        .must_change_password(&principal.username);
    let now = state.clock.now_millis();
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
    let cookie = format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/");
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
}

/// `GET /api/v1/auth/me` -> the current principal.
pub async fn me(auth: AuthPrincipal) -> Json<MeResponse> {
    Json(MeResponse {
        username: auth.principal.username,
        is_admin: auth.principal.is_admin,
        groups: auth.principal.groups,
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
    state
        .security
        .lock()
        .expect("security mutex")
        .change_password(
            &auth.principal.username,
            &req.current_password,
            &req.new_password,
        )
        .map_err(|e| match e {
            SecurityError::IncorrectPassword => {
                ApiError::unauthorized("current password is incorrect")
            }
            _ => ApiError::internal(),
        })?;
    audit(
        &state,
        &auth.principal.username,
        AuditAction::UserChange,
        None,
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}
