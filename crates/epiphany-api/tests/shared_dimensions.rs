//! Shared dimension library REST surface (ADR-0024), end to end: register a
//! reusable dimension, create a cube that references it (materialized copy), grow
//! the shared dimension and watch the change fan out to the referencing cube,
//! block a cube-local edit to a registry-backed dimension (divergence guard),
//! refuse to delete a referenced dimension, and confirm the library and its
//! references survive a restart. Also checks the modular permission (ADR-0023):
//! the library is gated on a *global* `Dimension` grant, not just admin.

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
use epiphany_security::{AccessLevel, AuditLog, ObjectKind, Scope, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn seed_cube() -> Cube {
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Amount");
    Cube::new("Seed", vec![measure]).unwrap()
}

fn data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-shareddim-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a router over `dir` with both cube creation and the shared-dimension
/// registry enabled, reopening every cube on disk (a second call simulates a
/// restart). `modeler` holds a global `Dimension:Write` grant; `ann` holds none.
fn build_app(dir: &Path) -> Router {
    let cubes_dir = dir.join("cubes");
    std::fs::create_dir_all(&cubes_dir).unwrap();
    let mut stores = BTreeMap::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&cubes_dir)
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
            "Seed".to_string(),
            Store::create(cubes_dir.join("Seed"), seed_cube()).unwrap(),
        );
    }

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("modeler", "pw", false).unwrap();
    sec.create_user("ann", "pw", false).unwrap();
    // The modular permission (ADR-0023): a global Dimension:Write grant, not admin.
    sec.set_grant(
        &Subject::User("modeler".into()),
        Scope::Global,
        ObjectKind::Dimension,
        AccessLevel::Write,
    )
    .unwrap();
    // modeler also needs to create cubes for the by-reference test.
    sec.set_grant(
        &Subject::User("modeler".into()),
        Scope::Global,
        ObjectKind::Cube,
        AccessLevel::Admin,
    )
    .unwrap();

    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default()))
            .with_cubes_dir(cubes_dir)
            .with_dimensions_dir(dir.join("dimensions")),
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

/// Send a request and return both status and parsed JSON body (or Null).
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn cube_dimension<'a>(detail: &'a Value, name: &str) -> &'a Value {
    detail["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == name)
        .expect("dimension present")
}

fn member_names(dim: &Value) -> Vec<String> {
    dim["elements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn register_reference_grow_and_recover() {
    let dir = data_dir("lifecycle");
    let app = build_app(&dir);
    let modeler = login(&app, "modeler").await;
    // Reading a specific cube's structure needs cube access, which the
    // fail-closed default denies a non-admin; admin reads the cubes back.
    let admin = login(&app, "admin").await;

    // Register a reusable Product dimension in the library.
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/dimensions",
        &modeler,
        Some(json!({
            "name": "Product",
            "elements": [
                { "name": "Widget", "kind": "numeric" },
                { "name": "Gadget", "kind": "numeric" },
                { "name": "AllProducts", "kind": "consolidated" }
            ],
            "edges": [
                { "parent": "AllProducts", "child": "Widget", "weight": 1 },
                { "parent": "AllProducts", "child": "Gadget", "weight": 1 }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let product_id = body["id"].as_u64().unwrap();
    assert_eq!(body["generation"], 0);

    // It appears in the library listing.
    let (_, list) = call(&app, "GET", "/api/v1/dimensions", &modeler, None).await;
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["name"] == "Product" && d["id"].as_u64() == Some(product_id)));

    // Create two cubes that each reference the shared Product dimension.
    for cube in ["Sales", "Budget"] {
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes",
            &modeler,
            Some(json!({
                "name": cube,
                "dimensions": [
                    { "ref": product_id },
                    { "name": "Measure", "elements": [{ "name": "Amount", "kind": "numeric" }] }
                ]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create {cube} by reference");
    }

    // The library now reports both cubes as references.
    let (_, detail) = call(
        &app,
        "GET",
        &format!("/api/v1/dimensions/{product_id}"),
        &modeler,
        None,
    )
    .await;
    let refs: Vec<&str> = detail["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert_eq!(refs, vec!["Budget", "Sales"]);

    // Each cube materialized a copy of Product with its members.
    let (_, sales) = call(&app, "GET", "/api/v1/cubes/Sales", &admin, None).await;
    assert!(member_names(cube_dimension(&sales, "Product")).contains(&"Widget".to_string()));

    // Grow the shared dimension: a new member fans out to both cubes.
    let (status, grown) = call(
        &app,
        "POST",
        &format!("/api/v1/dimensions/{product_id}/elements"),
        &modeler,
        Some(json!({
            "elements": [{ "name": "Gizmo", "kind": "numeric" }],
            "edges": [{ "parent": "AllProducts", "child": "Gizmo", "weight": 1 }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(grown["generation"], 1);
    let mut fanned: Vec<&str> = grown["fanned_out_to"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    fanned.sort_unstable();
    assert_eq!(fanned, vec!["Budget", "Sales"]);

    for cube in ["Sales", "Budget"] {
        let (_, d) = call(&app, "GET", &format!("/api/v1/cubes/{cube}"), &admin, None).await;
        assert!(
            member_names(cube_dimension(&d, "Product")).contains(&"Gizmo".to_string()),
            "{cube} received the fanned-out member"
        );
    }

    // Divergence guard: a cube-local edit to the shared dimension is blocked.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/elements",
        &modeler,
        Some(json!({
            "elements": [{ "dimension": "Product", "name": "Sprocket", "kind": "numeric" }]
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "cube-local shared-dim edit blocked"
    );

    // A still-referenced dimension cannot be deleted.
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/dimensions/{product_id}"),
        &modeler,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "referenced dimension not deletable"
    );

    // An unreferenced dimension is deletable.
    let (_, body) = call(
        &app,
        "POST",
        "/api/v1/dimensions",
        &modeler,
        Some(json!({ "name": "Scratch", "elements": [{ "name": "x", "kind": "numeric" }] })),
    )
    .await;
    let scratch_id = body["id"].as_u64().unwrap();
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/dimensions/{scratch_id}"),
        &modeler,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Restart: reopen from disk. The library recovers Product at generation 1
    // with both references, and Scratch stays deleted.
    drop(app);
    let app = build_app(&dir);
    let modeler = login(&app, "modeler").await;
    let (_, detail) = call(
        &app,
        "GET",
        &format!("/api/v1/dimensions/{product_id}"),
        &modeler,
        None,
    )
    .await;
    assert_eq!(detail["generation"], 1);
    assert!(member_names(&detail).contains(&"Gizmo".to_string()));
    let (status, _) = call(
        &app,
        "GET",
        &format!("/api/v1/dimensions/{scratch_id}"),
        &modeler,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "deleted dimension stays gone"
    );
}

#[tokio::test]
async fn library_is_gated_on_the_dimension_permission() {
    let dir = data_dir("authz");
    let app = build_app(&dir);
    let ann = login(&app, "ann").await;
    let modeler = login(&app, "modeler").await;

    let body = json!({ "name": "Org", "elements": [{ "name": "HQ", "kind": "numeric" }] });

    // ann has no Dimension grant: register, list, and delete are all denied.
    assert_eq!(
        call(&app, "POST", "/api/v1/dimensions", &ann, Some(body.clone()))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, "GET", "/api/v1/dimensions", &ann, None).await.0,
        StatusCode::FORBIDDEN
    );

    // modeler holds a global Dimension:Write grant (not admin) and may register.
    let (status, registered) = call(&app, "POST", "/api/v1/dimensions", &modeler, Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    let id = registered["id"].as_u64().unwrap();

    // ann still cannot grow or delete it.
    assert_eq!(
        call(
            &app,
            "POST",
            &format!("/api/v1/dimensions/{id}/elements"),
            &ann,
            Some(json!({ "elements": [{ "name": "Branch", "kind": "numeric" }] })),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("/api/v1/dimensions/{id}"),
            &ann,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
