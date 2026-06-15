//! Object-security gating acceptance (ADR-0015): a cube is open to authenticated
//! users until an admin adds a grant, after which only grantees (and admins) may
//! reach it, at their granted level. Grants are set directly on the kept security
//! handle (the admin REST surface is 7F).

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

fn harness() -> Harness {
    let dir = std::env::temp_dir().join(format!("epiphany-objsec-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    sec.create_user("bob", "pw", false).unwrap();
    let security = Arc::new(Mutex::new(sec));
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: security.clone(),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
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
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["token"].as_str().unwrap().to_string()
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
async fn cube_is_open_until_restricted_then_grants_govern() {
    let h = harness();
    let admin = login(&h.app, "admin").await;
    let ann = login(&h.app, "ann").await;
    let bob = login(&h.app, "bob").await;

    // Unmanaged cube: any authenticated user may read and write.
    assert_eq!(read_status(&h.app, &ann).await, StatusCode::OK);
    assert_eq!(write_status(&h.app, &ann).await, StatusCode::OK);

    // An admin restricts the cube by granting bob Read.
    h.security
        .lock()
        .unwrap()
        .set_object_access(
            ObjectRef::cube("Sales"),
            &Subject::User("bob".into()),
            AccessLevel::Read,
        )
        .unwrap();

    // Now ann (no grant) is denied both read and write.
    assert_eq!(read_status(&h.app, &ann).await, StatusCode::FORBIDDEN);
    assert_eq!(write_status(&h.app, &ann).await, StatusCode::FORBIDDEN);
    // bob may read but not write (granted Read only).
    assert_eq!(read_status(&h.app, &bob).await, StatusCode::OK);
    assert_eq!(write_status(&h.app, &bob).await, StatusCode::FORBIDDEN);
    // The admin keeps full access by bypass.
    assert_eq!(read_status(&h.app, &admin).await, StatusCode::OK);
    assert_eq!(write_status(&h.app, &admin).await, StatusCode::OK);
}
