//! M2 acceptance suite: "It's a product" (end of Phase 2).
//!
//! Proves the Phase 2 definition of done end to end over the REAL router, under
//! deterministic mode (fixed admin, ManualClock, seeded IdGen, tempdir-backed
//! Store): from a clean start a user logs in, opens a cube, edits a cell, sees
//! the consolidation update, a batch applies all-or-nothing, and the change
//! survives a server restart (while in-memory sessions do not).
//!
//! The browser flow over this same contract is covered separately by E2E; this
//! Rust suite is the binding, non-flaky gate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, SessionStore};
use epiphany_core::{Cube, Dimension};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::Engine;
use epiphany_persist::Store;
use epiphany_security::{AuditLog, SecurityStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tower::ServiceExt;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-m2-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// A Sales cube: Region(North, South, Total) x Measure(Actual, Budget, Variance).
fn sample_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let north = region.add_leaf("North");
    let south = region.add_leaf("South");
    let total = region.add_consolidated("Total");
    region.add_child(total, north, 1).unwrap();
    region.add_child(total, south, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    let actual = measure.add_leaf("Actual");
    let budget = measure.add_leaf("Budget");
    let variance = measure.add_consolidated("Variance");
    measure.add_child(variance, actual, 1).unwrap();
    measure.add_child(variance, budget, -1).unwrap();
    Cube::new("Sales", vec![region, measure]).unwrap()
}

/// Build the router over a Store at `dir/cubes/Sales` (created if absent, else
/// reopened), with a fixed admin and a fresh in-memory session store.
fn router_for(dir: &Path) -> (Router, Arc<Mutex<SessionStore>>) {
    let sales_dir = dir.join("cubes").join("Sales");
    let store = if sales_dir.join("snapshot.model").is_file() {
        Store::open(sales_dir.clone()).unwrap()
    } else {
        Store::create(sales_dir.clone(), sample_cube()).unwrap()
    };
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));
    let sessions = Arc::new(Mutex::new(SessionStore::new(60_000)));
    let state = AppState {
        engine,
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
        sessions: sessions.clone(),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: broadcast::channel(16).0,
        mdx: Arc::new(epiphany_core::NoSetEvaluator),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Default::default(),
        http: Default::default(),
        sql: Default::default(),
    };
    (build_router(state), sessions)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn login(app: &Router) -> String {
    let body = json!({ "username": "admin", "password": "pw" }).to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    body_json(resp).await["token"].as_str().unwrap().to_string()
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn total_actual(app: &Router, token: &str) -> String {
    let (status, read) = call(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        token,
        Some(json!({ "coords": [{ "Region": "Total", "Measure": "Actual" }] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    read["cells"][0]["value"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn m2_definition_of_done() {
    let dir = scratch("dod");

    // --- Session 1: log in, open, edit, consolidate, batch ---
    let token = {
        let (app, _sessions) = router_for(&dir);

        // 1. Log in.
        let token = login(&app).await;

        // 2. Open the cube.
        let (status, detail) = call(&app, "GET", "/api/v1/cubes/Sales", &token, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["name"], "Sales");

        // 3. Baseline consolidated Total/Actual is zero.
        assert_eq!(total_actual(&app, &token).await, "0");

        // 4. Edit a leaf cell.
        let (status, _) = call(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/cell",
            &token,
            Some(json!({ "coord": { "Region": "North", "Measure": "Actual" }, "value": "100" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // 5. The consolidation updates exactly.
        assert_eq!(total_actual(&app, &token).await, "100");

        // 6a. A batch with an invalid mid-batch write is rejected; base unchanged.
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/cells/batch",
            &token,
            Some(json!({ "writes": [
                { "coord": { "Region": "South", "Measure": "Actual" }, "value": "50" },
                { "coord": { "Region": "Total", "Measure": "Actual" }, "value": "5" }
            ] })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            total_actual(&app, &token).await,
            "100",
            "rejected batch changed nothing"
        );

        // 6b. A valid batch applies atomically.
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/cells/batch",
            &token,
            Some(json!({ "writes": [
                { "coord": { "Region": "South", "Measure": "Actual" }, "value": "50" }
            ] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(total_actual(&app, &token).await, "150");

        token
    };

    // --- Session 2: restart over the same data directory ---
    {
        let (app, sessions) = router_for(&dir);

        // In-memory sessions do not survive a restart (by design).
        assert_eq!(sessions.lock().unwrap().len(), 0);
        let (status, _) = call(&app, "GET", "/api/v1/cubes/Sales", &token, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "old token is gone after restart"
        );

        // But the data did: re-login and confirm the edit persisted.
        let token = login(&app).await;
        assert_eq!(
            total_actual(&app, &token).await,
            "150",
            "the edit survived the restart"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
