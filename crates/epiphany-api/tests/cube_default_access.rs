//! Default cube-access posture (ADR-0015 decision 2a). The secure default is
//! fail-closed: an ungranted cube is denied to non-admins, and access is opened
//! only by an explicit grant. A deployment may opt into the trusted-single-org
//! "open" posture, in which an ungranted cube is writable by any authenticated
//! user; this is exercised too so both branches are covered.

use std::collections::BTreeMap;
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
use epiphany_security::{AccessLevel, AuditLog, ObjectRef, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

struct Harness {
    app: Router,
    security: Arc<Mutex<SecurityStore>>,
}

/// Build a harness with the given ungranted-cube posture (`open = false` is the
/// secure default).
fn harness(name: &str, open: bool) -> Harness {
    let dir = std::env::temp_dir().join(format!("epiphany-cubedef-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    sec.set_default_cube_open(open);
    let security = Arc::new(Mutex::new(sec));
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: security.clone(),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
    };
    Harness {
        app: build_router(state),
        security,
    }
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

async fn read_status(app: &Router, token: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/cubes/Sales/cells/read")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "coords": [{ "Region": "North", "Measure": "Sales" }] }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn write_status(app: &Router, token: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/cubes/Sales/cell")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "coord": { "Region": "North", "Measure": "Sales" }, "value": "5" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn ungranted_cube_is_closed_to_non_admins_by_default() {
    let h = harness("closed", false);
    let admin = login(&h.app, "admin").await;
    let ann = login(&h.app, "ann").await;

    // Secure default: a non-admin is denied an ungranted cube; the admin is not.
    assert_eq!(read_status(&h.app, &ann).await, StatusCode::FORBIDDEN);
    assert_eq!(write_status(&h.app, &ann).await, StatusCode::FORBIDDEN);
    assert_eq!(read_status(&h.app, &admin).await, StatusCode::OK);

    // Access is opened only by an explicit grant.
    h.security
        .lock()
        .unwrap()
        .set_object_access(
            ObjectRef::cube("Sales"),
            &Subject::User("ann".into()),
            AccessLevel::Read,
        )
        .unwrap();
    assert_eq!(read_status(&h.app, &ann).await, StatusCode::OK);
    // The grant was Read only, so a write is still denied.
    assert_eq!(write_status(&h.app, &ann).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn open_posture_is_opt_in() {
    let h = harness("open", true);
    let ann = login(&h.app, "ann").await;
    // With the opt-in open posture an ungranted cube is writable by any
    // authenticated user.
    assert_eq!(read_status(&h.app, &ann).await, StatusCode::OK);
    assert_eq!(write_status(&h.app, &ann).await, StatusCode::OK);
}
