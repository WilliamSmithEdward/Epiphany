//! Epiphany API: the REST + WebSocket surface (Axum).
//!
//! [`build_router`] assembles the router from an [`AppState`]; the server binary
//! and every integration test use the same builder, so tested behavior matches
//! served behavior. Responses are clean modern JSON (not OData); errors use the
//! shared [`ApiError`] envelope.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use epiphany_engine::Engine;

mod error;
pub use error::ApiError;

/// Stable crate identifier.
pub const CRATE: &str = "epiphany-api";

/// Shared application state, cheap to clone into every handler.
#[derive(Clone, Debug)]
pub struct AppState {
    /// The concurrent cube engine (snapshot reads, atomic batch commits).
    pub engine: Engine,
}

/// Build the application router. Used by both the server binary and tests, so
/// what is tested is what is served.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/cubes", get(list_cubes))
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

async fn list_cubes(State(state): State<AppState>) -> Json<CubeListResponse> {
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
    use epiphany_determinism::IdGen;
    use epiphany_persist::Store;
    use http_body_util::BodyExt;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_state(name: &str) -> AppState {
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        let cube = Cube::new("Sales", vec![region]).unwrap();
        let dir = std::env::temp_dir().join(format!("epiphany-api-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::create(dir, cube).unwrap();
        let mut stores = BTreeMap::new();
        stores.insert("Sales".to_string(), store);
        let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));
        AppState { engine }
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let app = build_router(test_state("healthz"));
        let resp = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["status"], "ok");
    }

    #[tokio::test]
    async fn lists_the_demo_cube() {
        let app = build_router(test_state("cubes"));
        let resp = app
            .oneshot(Request::get("/api/v1/cubes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["cubes"][0]["name"], "Sales");
        assert_eq!(json["cubes"][0]["rank"], 1);
    }

    #[tokio::test]
    async fn unknown_route_returns_the_error_envelope() {
        let app = build_router(test_state("notfound"));
        let resp = app
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_json(resp).await["error"]["code"], "NOT_FOUND");
    }
}
