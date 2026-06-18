//! Model-editing REST surface (ADR-0021), end to end: create a cube, add members
//! and consolidation roll-ups, define and set attributes, and confirm that
//! authorization is enforced, invalid structure is rejected, and a created cube
//! plus its edits survive a restart (reopen from disk).

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
use epiphany_mdx::MdxEvaluator;
use epiphany_persist::Store;
use epiphany_security::{AuditLog, SecurityStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn seed_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    region.add_leaf("South");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Amount");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

fn data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-modeledit-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a router over the cubes under `dir`, creating the seed "Sales" cube on
/// first boot. Calling it again over the same `dir` reopens every cube from
/// disk, which is how the test simulates a restart. Cube creation is enabled
/// (the engine knows `dir`).
fn build_app(dir: &Path) -> Router {
    let mut stores = BTreeMap::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("snapshot.model").is_file())
        .collect();
    entries.sort();
    for path in &entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        stores.insert(name, Store::open(path).unwrap());
    }
    if stores.is_empty() {
        stores.insert(
            "Sales".to_string(),
            Store::create(dir.join("Sales"), seed_cube()).unwrap(),
        );
    }

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    // ann is a non-admin with no grants; the closed default denies her.
    sec.create_user("ann", "pw", false).unwrap();
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())).with_cubes_dir(dir),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Default::default(),
        automation: Arc::new(Mutex::new(
            epiphany_persist::AutomationStore::open(std::env::temp_dir().join(format!(
                "epiphany-test-auto-{}-model_editing-0",
                std::process::id()
            )))
            .unwrap(),
        )),
        http: Default::default(),
        sql: Default::default(),
    };
    build_router(state)
}

async fn login(app: &Router, user: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "username": user, "password": "pw" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Value>(&bytes).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> StatusCode {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    app.clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap()
        .status()
}

async fn get_json(app: &Router, uri: &str, token: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_cube_add_structure_and_recover() {
    let dir = data_dir("recover");
    let app = build_app(&dir);
    let admin = login(&app, "admin").await;

    // Create a Budget cube with an Account dimension carrying a Profit total.
    let create = json!({
        "name": "Budget",
        "dimensions": [
            {
                "name": "Account",
                "elements": [
                    { "name": "Sales", "kind": "numeric" },
                    { "name": "Costs", "kind": "numeric" },
                    { "name": "Profit", "kind": "consolidated" }
                ],
                "edges": [
                    { "parent": "Profit", "child": "Sales", "weight": 1 },
                    { "parent": "Profit", "child": "Costs", "weight": -1 }
                ]
            },
            { "name": "Period", "elements": [{ "name": "Jan", "kind": "numeric" }] }
        ]
    });
    assert_eq!(
        send(&app, "POST", "/api/v1/cubes", &admin, Some(create)).await,
        StatusCode::OK
    );

    // It appears in the listing and its structure reads back.
    let cubes = get_json(&app, "/api/v1/cubes", &admin).await;
    let names: Vec<&str> = cubes["cubes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"Budget"),
        "Budget should be listed: {names:?}"
    );

    // Add a member and a roll-up to the existing Account dimension.
    let add = json!({
        "elements": [{ "dimension": "Account", "name": "Tax", "kind": "numeric" }],
        "edges": [{ "dimension": "Account", "parent": "Profit", "child": "Tax", "weight": -1 }]
    });
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Budget/elements",
            &admin,
            Some(add)
        )
        .await,
        StatusCode::OK
    );

    // Define and set an attribute on Account.
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Budget/dimensions/Account/attributes/Code",
            &admin,
            Some(json!({ "kind": "text" })),
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Budget/dimensions/Account/attributes/Code/values",
            &admin,
            Some(json!({ "values": [{ "element": "Sales", "value": "REV" }] })),
        )
        .await,
        StatusCode::OK
    );

    // Restart: reopen every cube from disk. Budget and its added member persist.
    drop(app);
    let app = build_app(&dir);
    let admin = login(&app, "admin").await;
    let detail = get_json(&app, "/api/v1/cubes/Budget", &admin).await;
    let account = detail["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "Account")
        .expect("Account dimension survived restart");
    let members: Vec<&str> = account["elements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        members.contains(&"Tax"),
        "added member persisted: {members:?}"
    );
    assert!(members.contains(&"Profit"));
}

#[tokio::test]
async fn authorization_and_validation_are_enforced() {
    let dir = data_dir("authz");
    let app = build_app(&dir);
    let admin = login(&app, "admin").await;
    let ann = login(&app, "ann").await;

    let valid = json!({
        "name": "Forecast",
        "dimensions": [{ "name": "D", "elements": [{ "name": "a", "kind": "numeric" }] }]
    });

    // A non-admin cannot create a cube, and cannot edit an existing cube's
    // structure (closed default denies her).
    assert_eq!(
        send(&app, "POST", "/api/v1/cubes", &ann, Some(valid.clone())).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Sales/elements",
            &ann,
            Some(json!({ "elements": [{ "dimension": "Region", "name": "East", "kind": "numeric" }] })),
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // Admin creates it; a second create with the same name is a conflict.
    assert_eq!(
        send(&app, "POST", "/api/v1/cubes", &admin, Some(valid.clone())).await,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, "POST", "/api/v1/cubes", &admin, Some(valid)).await,
        StatusCode::CONFLICT
    );

    // Invalid structure is rejected (a leaf cannot be a roll-up parent).
    let bad = json!({
        "name": "Bad",
        "dimensions": [{
            "name": "D",
            "elements": [{ "name": "p", "kind": "numeric" }, { "name": "c", "kind": "numeric" }],
            "edges": [{ "parent": "p", "child": "c", "weight": 1 }]
        }]
    });
    assert_eq!(
        send(&app, "POST", "/api/v1/cubes", &admin, Some(bad)).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // Adding to an unknown dimension is rejected (422), and an empty name too.
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Sales/elements",
            &admin,
            Some(json!({ "elements": [{ "dimension": "Nope", "name": "x", "kind": "numeric" }] })),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes",
            &admin,
            Some(json!({ "name": "  ", "dimensions": [{ "name": "D", "elements": [{ "name": "a", "kind": "numeric" }] }] })),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}
