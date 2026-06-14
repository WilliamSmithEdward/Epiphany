//! Epiphany API: the REST + WebSocket surface (Axum).
//!
//! [`build_router`] assembles the router from an [`AppState`]; the server binary
//! and every integration test use the same builder, so tested behavior matches
//! served behavior. Responses are clean modern JSON (not OData); errors use the
//! shared [`ApiError`] envelope. Every route except `/healthz` and the login
//! endpoint requires a valid session (the [`auth::AuthPrincipal`] extractor).

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Serialize;

use epiphany_core::SetEvaluator;
use epiphany_determinism::Clock;
use epiphany_engine::Engine;
use epiphany_security::SecurityStore;
use tokio::sync::broadcast;

mod auth;
mod dto;
mod error;
mod openapi;
mod query_routes;
mod resolve;
mod routes;
mod session;
mod ws;

pub use error::ApiError;
pub use session::SessionStore;
pub use ws::ChangeEvent;

/// Stable crate identifier.
pub const CRATE: &str = "epiphany-api";

/// Shared application state, cheap to clone into every handler.
#[derive(Clone)]
pub struct AppState {
    /// The concurrent cube engine (snapshot reads, atomic batch commits).
    pub engine: Engine,
    /// The injected clock (real in production, manual in tests).
    pub clock: Arc<dyn Clock>,
    /// Users, groups, and password hashes.
    pub security: Arc<Mutex<SecurityStore>>,
    /// Live session tokens (in memory; lost on restart, by design).
    pub sessions: Arc<Mutex<SessionStore>>,
    /// Broadcaster of change events to WebSocket clients.
    pub events: broadcast::Sender<ChangeEvent>,
    /// The MDX set evaluator for dynamic subsets (the composition-root injects a
    /// real one; tests inject a `NoSetEvaluator` or the real evaluator as needed).
    pub mdx: Arc<dyn SetEvaluator + Send + Sync>,
}

impl AppState {
    /// The injected set evaluator as a plain trait reference (drops the auto
    /// traits the core query functions do not require).
    pub(crate) fn evaluator(&self) -> &dyn SetEvaluator {
        self.mdx.as_ref()
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

/// Build the application router. Used by both the server binary and tests, so
/// what is tested is what is served.
pub fn build_router(state: AppState) -> Router {
    // Protected routes require a valid session via the AuthPrincipal extractor.
    let protected = Router::new()
        .route("/api/v1/cubes", get(list_cubes))
        .route("/api/v1/cubes/{cube}", get(routes::get_cube))
        .route("/api/v1/cubes/{cube}/cells/read", post(routes::read_cells))
        .route("/api/v1/cubes/{cube}/cell", put(routes::write_cell))
        .route(
            "/api/v1/cubes/{cube}/cells/batch",
            post(routes::batch_write),
        )
        // Subsets (dimension-scoped).
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets",
            get(query_routes::list_subsets).post(query_routes::create_subset),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/preview",
            post(query_routes::preview_subset),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/mdx/preview",
            post(query_routes::preview_mdx),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/{name}",
            get(query_routes::get_subset)
                .put(query_routes::replace_subset)
                .delete(query_routes::delete_subset),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/{name}/members",
            get(query_routes::subset_members),
        )
        // Views (cube-scoped).
        .route(
            "/api/v1/cubes/{cube}/views",
            get(query_routes::list_views).post(query_routes::create_view),
        )
        .route(
            "/api/v1/cubes/{cube}/views/{name}",
            get(query_routes::get_view)
                .put(query_routes::replace_view)
                .delete(query_routes::delete_view),
        )
        .route(
            "/api/v1/cubes/{cube}/views/{name}/execute",
            post(query_routes::execute_saved_view),
        )
        .route(
            "/api/v1/cubes/{cube}/cellset",
            post(query_routes::execute_adhoc),
        )
        .route("/api/v1/ws", get(ws::ws))
        .route("/api/v1/auth/me", get(auth::me))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/auth/password", post(auth::change_password));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/openapi.json", get(openapi::openapi_json))
        .route("/api/v1/auth/login", post(auth::login))
        .merge(protected)
        .fallback(not_found)
        .with_state(state)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// One cube in the cube list.
#[derive(Debug, Serialize)]
pub struct CubeSummary {
    pub name: String,
    pub rank: usize,
    pub cell_count: usize,
    pub string_cell_count: usize,
}

#[derive(Serialize)]
struct CubeListResponse {
    cubes: Vec<CubeSummary>,
}

async fn list_cubes(
    _auth: auth::AuthPrincipal,
    State(state): State<AppState>,
) -> Json<CubeListResponse> {
    let cubes = state
        .engine
        .cube_names()
        .into_iter()
        .map(|name| {
            let (rank, cell_count, string_cell_count) = state
                .engine
                .snapshot(&name)
                .map(|s| {
                    (
                        s.cube().rank(),
                        s.cube().cell_count(),
                        s.cube().string_cell_count(),
                    )
                })
                .unwrap_or((0, 0, 0));
            CubeSummary {
                name,
                rank,
                cell_count,
                string_cell_count,
            }
        })
        .collect();
    Json(CubeListResponse { cubes })
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use epiphany_core::{Cube, Dimension};
    use epiphany_determinism::{IdGen, ManualClock};
    use epiphany_engine::Engine;
    use epiphany_persist::Store;
    use http_body_util::BodyExt;
    use std::collections::BTreeMap;

    const TTL: u64 = 60_000;

    fn test_state_with_clock(name: &str, clock: Arc<dyn Clock>) -> AppState {
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        let cube = Cube::new("Sales", vec![region]).unwrap();
        let dir = std::env::temp_dir().join(format!("epiphany-api-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::create(dir, cube).unwrap();
        let mut stores = BTreeMap::new();
        stores.insert("Sales".to_string(), store);
        AppState {
            engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
            clock,
            security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
            sessions: Arc::new(Mutex::new(SessionStore::new(TTL))),
            events: broadcast::channel(16).0,
            mdx: Arc::new(epiphany_core::NoSetEvaluator),
        }
    }

    fn test_state(name: &str) -> AppState {
        test_state_with_clock(name, Arc::new(ManualClock::new(1_000)))
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn login(router: &Router, user: &str, pass: &str) -> (StatusCode, serde_json::Value) {
        let body = serde_json::json!({ "username": user, "password": pass }).to_string();
        let resp = router
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_is_public() {
        let app = build_router(test_state("healthz"));
        let resp = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn login_then_access_protected_route() {
        let app = build_router(test_state("login-ok"));

        let (status, json) = login(&app, "admin", "pw").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["user"]["is_admin"], true);
        let token = json["token"].as_str().unwrap().to_string();

        // With the bearer token, the protected cube list is reachable.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/v1/cubes")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["cubes"][0]["name"], "Sales");
    }

    #[tokio::test]
    async fn bad_credentials_are_401() {
        let app = build_router(test_state("login-bad"));
        let (status, json) = login(&app, "admin", "wrong").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    }

    #[tokio::test]
    async fn protected_route_without_token_is_401() {
        let app = build_router(test_state("no-token"));
        let resp = app
            .oneshot(Request::get("/api/v1/cubes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logout_revokes_the_session() {
        let app = build_router(test_state("logout"));
        let (_, json) = login(&app, "admin", "pw").await;
        let token = json["token"].as_str().unwrap().to_string();
        let auth = format!("Bearer {token}");

        let logout = app
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/logout")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::NO_CONTENT);

        // The revoked token no longer authenticates.
        let resp = app
            .oneshot(
                Request::get("/api/v1/auth/me")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn session_expires_after_ttl() {
        // Keep a typed handle to the manual clock so the test can advance it.
        let manual = Arc::new(ManualClock::new(1_000));
        let app = build_router(test_state_with_clock("expiry", manual.clone()));
        let (_, json) = login(&app, "admin", "pw").await;
        let token = json["token"].as_str().unwrap().to_string();

        // Advance past the TTL: the token is now rejected.
        manual.advance(TTL + 1);

        let resp = app
            .oneshot(
                Request::get("/api/v1/auth/me")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
