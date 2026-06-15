//! Global cube grants and explicit deny (ADR-0016), end to end over the REST
//! surface. Exercises the motivating scenario: a broad Read baseline across all
//! cubes, Write on one cube, and a deny on another, expressed for a group, with
//! admin bypass remaining absolute. Specificity wins (a per-cube grant overrides
//! the global one), deny overrides allow, and the closed default still denies an
//! unrelated user.

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
use epiphany_security::{AuditLog, SecurityStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn cube(name: &str) -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new(name, vec![region, measure]).unwrap()
}

struct Harness {
    app: Router,
}

fn harness(tag: &str) -> Harness {
    let dir =
        std::env::temp_dir().join(format!("epiphany-cubegrants-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let mut stores = BTreeMap::new();
    for name in ["Sales", "Budget", "Salaries"] {
        let store = Store::create(dir.join(name), cube(name)).unwrap();
        stores.insert(name.to_string(), store);
    }
    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    // ann is an analyst; dave is an unrelated authenticated user. The secure
    // closed default is in force (no set_default_cube_open).
    sec.create_user_with_groups("ann", "pw", false, &["analysts".to_string()])
        .unwrap();
    sec.create_user("dave", "pw", false).unwrap();
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
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

/// PUT a cube grant as the given token; returns the status.
async fn put_cube_grant(app: &Router, token: &str, body: Value) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/acl/cube-grants")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn list_cube_grants(app: &Router, token: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/acl/cube-grants")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Value>(&bytes).unwrap()
}

async fn read_status(app: &Router, token: &str, cube: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/cubes/{cube}/cells/read"))
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

async fn write_status(app: &Router, token: &str, cube: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/cubes/{cube}/cell"))
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
async fn global_read_with_per_cube_write_and_deny() {
    let h = harness("scenario");
    let admin = login(&h.app, "admin").await;
    let ann = login(&h.app, "ann").await;
    let dave = login(&h.app, "dave").await;

    // Admin sets, over REST: global Read for analysts, Write on Budget, deny on
    // Salaries.
    assert_eq!(
        put_cube_grant(
            &h.app,
            &admin,
            json!({ "subject_kind": "group", "subject": "analysts", "level": "read" }),
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        put_cube_grant(
            &h.app,
            &admin,
            json!({ "scope": "Budget", "subject_kind": "group", "subject": "analysts", "level": "write" }),
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        put_cube_grant(
            &h.app,
            &admin,
            json!({ "scope": "Salaries", "subject_kind": "group", "subject": "analysts", "level": "deny" }),
        )
        .await,
        StatusCode::NO_CONTENT
    );

    // ann (analyst): global Read everywhere, Write only on Budget, denied Salaries.
    assert_eq!(read_status(&h.app, &ann, "Sales").await, StatusCode::OK);
    assert_eq!(
        write_status(&h.app, &ann, "Sales").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(read_status(&h.app, &ann, "Budget").await, StatusCode::OK);
    assert_eq!(write_status(&h.app, &ann, "Budget").await, StatusCode::OK);
    assert_eq!(
        read_status(&h.app, &ann, "Salaries").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        write_status(&h.app, &ann, "Salaries").await,
        StatusCode::FORBIDDEN
    );

    // Admin bypass is absolute, even on the denied cube.
    assert_eq!(
        read_status(&h.app, &admin, "Salaries").await,
        StatusCode::OK
    );

    // dave is not an analyst: the closed default still denies him everywhere.
    assert_eq!(
        read_status(&h.app, &dave, "Sales").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read_status(&h.app, &dave, "Budget").await,
        StatusCode::FORBIDDEN
    );

    // The listing reflects the three grants (a global allow, a per-cube allow,
    // and a per-cube deny).
    let grants = list_cube_grants(&h.app, &admin).await;
    let rows = grants["grants"].as_array().unwrap();
    assert!(rows.iter().any(|g| g["scope"].is_null()
        && g["effect"] == "allow"
        && g["level"] == "read"
        && g["subject"] == "analysts"));
    assert!(rows
        .iter()
        .any(|g| g["scope"] == "Budget" && g["effect"] == "allow" && g["level"] == "write"));
    assert!(rows
        .iter()
        .any(|g| g["scope"] == "Salaries" && g["effect"] == "deny"));
}

#[tokio::test]
async fn revoking_the_global_grant_restores_the_closed_default() {
    let h = harness("revoke");
    let admin = login(&h.app, "admin").await;
    let ann = login(&h.app, "ann").await;

    put_cube_grant(
        &h.app,
        &admin,
        json!({ "subject_kind": "group", "subject": "analysts", "level": "read" }),
    )
    .await;
    assert_eq!(read_status(&h.app, &ann, "Sales").await, StatusCode::OK);

    // Revoke with level "none": the closed default denies ann again.
    assert_eq!(
        put_cube_grant(
            &h.app,
            &admin,
            json!({ "subject_kind": "group", "subject": "analysts", "level": "none" }),
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        read_status(&h.app, &ann, "Sales").await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn specific_allow_overrides_a_global_deny() {
    let h = harness("specific");
    let admin = login(&h.app, "admin").await;
    let ann = login(&h.app, "ann").await;

    // Deny analysts everywhere, then allow Read on Sales: the specific allow wins.
    put_cube_grant(
        &h.app,
        &admin,
        json!({ "subject_kind": "group", "subject": "analysts", "level": "deny" }),
    )
    .await;
    put_cube_grant(
        &h.app,
        &admin,
        json!({ "scope": "Sales", "subject_kind": "group", "subject": "analysts", "level": "read" }),
    )
    .await;

    assert_eq!(read_status(&h.app, &ann, "Sales").await, StatusCode::OK);
    assert_eq!(
        read_status(&h.app, &ann, "Budget").await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn non_admin_cannot_set_cube_grants() {
    let h = harness("nonadmin");
    let ann = login(&h.app, "ann").await;
    assert_eq!(
        put_cube_grant(
            &h.app,
            &ann,
            json!({ "subject_kind": "user", "subject": "ann", "level": "admin" }),
        )
        .await,
        StatusCode::FORBIDDEN
    );
}
