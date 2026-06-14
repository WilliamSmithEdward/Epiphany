//! Sandbox lifecycle and ownership acceptance (ADR-0014): create, list, get,
//! discard, and commit over the real router, plus the per-user ownership
//! control (a non-admin may use only their own sandboxes; an admin may use any).

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
use epiphany_security::SecurityStore;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-sb-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let n = region.add_leaf("North");
    let t = region.add_consolidated("Total");
    region.add_child(t, n, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

/// A router with three users: admin, plus the non-admins ann and bob.
fn router(dir: &Path) -> Router {
    let store = Store::create(dir.to_path_buf(), sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    sec.create_user("bob", "pw", false).unwrap();
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
    };
    build_router(state)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

async fn login(app: &Router, user: &str) -> String {
    let body = json!({ "username": user, "password": "pw" }).to_string();
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
    let resp = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

#[tokio::test]
async fn sandbox_lifecycle_and_ownership() {
    let dir = scratch("lifecycle");
    let app = router(&dir);
    let ann = login(&app, "ann").await;
    let bob = login(&app, "bob").await;
    let admin = login(&app, "admin").await;

    // ann creates a sandbox.
    let (s, v) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &ann,
        Some(json!({ "name": "wi" })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["owner"], "ann");
    assert_eq!(v["cell_count"], 0);

    // A duplicate name is a conflict; a blank name is a bad request.
    let (s, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &ann,
        Some(json!({ "name": "wi" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &ann,
        Some(json!({ "name": "  " })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // ann sees and can read her own sandbox.
    let (s, list) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes", &ann, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["sandboxes"].as_array().unwrap().len(), 1);
    let (s, _) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes/wi", &ann, None).await;
    assert_eq!(s, StatusCode::OK);

    // bob neither sees nor can read ann's sandbox.
    let (s, blist) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes", &bob, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(blist["sandboxes"].as_array().unwrap().len(), 0);
    let (s, _) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes/wi", &bob, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &app,
        "DELETE",
        "/api/v1/cubes/Sales/sandboxes/wi",
        &bob,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // An admin may access any sandbox.
    let (s, _) = call(
        &app,
        "GET",
        "/api/v1/cubes/Sales/sandboxes/wi",
        &admin,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // An unknown sandbox is a 404.
    let (s, _) = call(
        &app,
        "GET",
        "/api/v1/cubes/Sales/sandboxes/ghost",
        &ann,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Committing an empty sandbox is a no-op that reports zero committed cells.
    let (s, c) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes/wi/commit",
        &ann,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["committed"], 0);

    // ann discards her sandbox; the list is then empty.
    let (s, _) = call(
        &app,
        "DELETE",
        "/api/v1/cubes/Sales/sandboxes/wi",
        &ann,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, list) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes", &ann, None).await;
    assert_eq!(list["sandboxes"].as_array().unwrap().len(), 0);

    std::fs::remove_dir_all(&dir).ok();
}
