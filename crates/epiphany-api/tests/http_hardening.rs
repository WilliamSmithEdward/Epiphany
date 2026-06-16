//! HTTP-surface hardening (ADR-0018): defensive response headers on every
//! response and an explicit request body-size limit.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, LoginGuard, SessionStore};
use epiphany_core::{Cube, Dimension};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::Engine;
use epiphany_mdx::MdxEvaluator;
use epiphany_persist::Store;
use epiphany_security::{AuditLog, SecurityStore};
use tower::ServiceExt;

fn app(tag: &str) -> Router {
    let dir = std::env::temp_dir().join(format!("epiphany-httphard-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    let cube = Cube::new("Sales", vec![region, measure]).unwrap();
    let store = Store::create(dir, cube).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Default::default(),
        http: Default::default(),
    };
    build_router(state)
}

#[tokio::test]
async fn responses_carry_defensive_security_headers() {
    let app = app("t1");
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(h.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(h.get("referrer-policy").unwrap(), "no-referrer");
    assert!(h.contains_key("strict-transport-security"));
    let csp = h.get("content-security-policy").unwrap().to_str().unwrap();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("frame-ancestors 'none'"));
}

#[tokio::test]
async fn an_oversized_request_body_is_rejected() {
    let app = app("t2");
    // Well over the 8 MiB cap; rejected before the handler runs.
    let big = vec![b'x'; 9 * 1024 * 1024];
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(big))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_normal_request_body_is_accepted() {
    let app = app("t3");
    // A small login body passes the size gate (credentials are wrong, so 401,
    // but crucially not 413).
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "username": "admin", "password": "nope" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
