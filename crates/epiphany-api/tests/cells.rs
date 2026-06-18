//! Integration tests for the cube-detail and cell endpoints (2D/2E), driven
//! through the real router via tower oneshot under deterministic mode.

use std::collections::BTreeMap;
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

const TTL: u64 = 60_000;

/// A `Region(North, South, Total) x Measure(Actual, Budget, Variance, Note)` cube.
fn state(name: &str) -> AppState {
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
    measure.add_string("Note");

    let cube = Cube::new("Sales", vec![region, measure]).unwrap();
    let dir = std::env::temp_dir().join(format!("epiphany-api-cell-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, cube).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
        sessions: Arc::new(Mutex::new(SessionStore::new(TTL))),
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
    }
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn token(app: &Router) -> String {
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

#[tokio::test]
async fn get_cube_returns_dimensions_and_elements() {
    let app = build_router(state("detail"));
    let t = token(&app).await;
    let (status, json) = call(&app, "GET", "/api/v1/cubes/Sales", &t, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["name"], "Sales");
    assert_eq!(json["dimensions"][0]["name"], "Region");
    let region_elements = json["dimensions"][0]["elements"].as_array().unwrap();
    assert!(region_elements
        .iter()
        .any(|e| e["name"] == "Total" && e["kind"] == "consolidated"));
    let measure_elements = json["dimensions"][1]["elements"].as_array().unwrap();
    assert!(measure_elements
        .iter()
        .any(|e| e["name"] == "Note" && e["kind"] == "string"));
}

#[tokio::test]
async fn writing_a_leaf_updates_the_consolidation() {
    let app = build_router(state("write"));
    let t = token(&app).await;

    let (s, cell) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &t,
        Some(json!({ "coord": { "Region": "North", "Measure": "Actual" }, "value": "100" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cell["value"], "100");
    assert_eq!(cell["editable"], true);

    call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &t,
        Some(json!({ "coord": { "Region": "South", "Measure": "Actual" }, "value": "50" })),
    )
    .await;

    // The consolidated Total/Actual reflects both leaves and is not editable.
    let (s, read) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &t,
        Some(json!({ "coords": [{ "Region": "Total", "Measure": "Actual" }] })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(read["cells"][0]["value"], "150");
    assert_eq!(read["cells"][0]["editable"], false);
    assert_eq!(read["cells"][0]["kind"], "numeric");
}

#[tokio::test]
async fn batch_write_is_atomic() {
    let app = build_router(state("batch"));
    let t = token(&app).await;
    call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &t,
        Some(json!({ "coord": { "Region": "North", "Measure": "Actual" }, "value": "100" })),
    )
    .await;

    // A batch whose second write targets a consolidated coordinate is rejected
    // whole, and nothing is applied.
    let (s, err) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/batch",
        &t,
        Some(json!({ "writes": [
            { "coord": { "Region": "South", "Measure": "Actual" }, "value": "50" },
            { "coord": { "Region": "Total", "Measure": "Actual" }, "value": "5" }
        ] })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err["error"]["code"], "WRITE_TO_NON_LEAF");

    // Base data unchanged: Total/Actual is still 100, South was not written.
    let (_, read) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &t,
        Some(json!({ "coords": [{ "Region": "Total", "Measure": "Actual" }] })),
    )
    .await;
    assert_eq!(read["cells"][0]["value"], "100");

    // A fully valid batch applies atomically and bumps the version.
    let (s, ok) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/batch",
        &t,
        Some(json!({ "writes": [
            { "coord": { "Region": "South", "Measure": "Actual" }, "value": "50" },
            { "coord": { "Region": "North", "Measure": "Budget" }, "value": "30" }
        ] })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ok["applied"], 2);

    let (_, read) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &t,
        Some(json!({ "coords": [
            { "Region": "Total", "Measure": "Actual" },
            { "Region": "Total", "Measure": "Variance" }
        ] })),
    )
    .await;
    assert_eq!(read["cells"][0]["value"], "150"); // 100 + 50
    assert_eq!(read["cells"][1]["value"], "120"); // (100 - 30) + (50 - 0)
}

#[tokio::test]
async fn string_cell_round_trips() {
    let app = build_router(state("string"));
    let t = token(&app).await;
    let (s, cell) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &t,
        Some(json!({ "coord": { "Region": "North", "Measure": "Note" }, "value": "on track" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cell["kind"], "string");
    assert_eq!(cell["value"], "on track");

    let (_, read) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &t,
        Some(json!({ "coords": [{ "Region": "North", "Measure": "Note" }] })),
    )
    .await;
    assert_eq!(read["cells"][0]["kind"], "string");
    assert_eq!(read["cells"][0]["value"], "on track");
}

#[tokio::test]
async fn unknown_element_is_422() {
    let app = build_router(state("unknown"));
    let t = token(&app).await;
    let (s, err) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &t,
        Some(json!({ "coords": [{ "Region": "Nowhere", "Measure": "Actual" }] })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err["error"]["code"], "UNKNOWN_ELEMENT");
}

#[tokio::test]
async fn cell_endpoints_require_auth() {
    let app = build_router(state("noauth"));
    let resp = app
        .oneshot(
            Request::get("/api/v1/cubes/Sales")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn openapi_is_public_and_documents_the_routes() {
    let app = build_router(state("openapi"));
    // No auth required for the spec.
    let resp = app
        .oneshot(
            Request::get("/api/v1/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = body_json(resp).await;
    assert_eq!(doc["openapi"], "3.1.0");
    let paths = doc["paths"].as_object().unwrap();
    assert!(paths.contains_key("/api/v1/cubes/{cube}/cell"));
    assert!(paths.contains_key("/api/v1/cubes/{cube}/cells/batch"));
    assert!(paths.contains_key("/api/v1/auth/login"));
    assert!(
        doc["paths"]["/api/v1/cubes/{cube}/cells/batch"]["post"]["responses"]["422"].is_object()
    );
}

#[tokio::test]
async fn write_broadcasts_a_change_event() {
    let app_state = state("ws-event");
    let mut rx = app_state.events.subscribe();
    let app = build_router(app_state);
    let t = token(&app).await;

    let (s, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &t,
        Some(json!({ "coord": { "Region": "North", "Measure": "Actual" }, "value": "42" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // The committed write broadcast one cells_changed event with the leaf coord.
    let event = rx.recv().await.unwrap();
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "cells_changed");
    assert_eq!(json["cube"], "Sales");
    assert_eq!(json["coords"][0]["Region"], "North");
    assert_eq!(json["coords"][0]["Measure"], "Actual");
}
