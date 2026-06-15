//! Authentication and credential hardening (ADR-0017), end to end over REST:
//! per-username login lockout (trigger and clock-driven release) and the
//! must-change-password gate (data routes blocked until the password is changed).

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

fn engine_with_sales(tag: &str) -> Engine {
    let dir = std::env::temp_dir().join(format!("epiphany-authhard-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    Engine::from_stores(stores, Arc::new(IdGen::default()))
}

/// Build an app over a given security store, clock, and login guard.
fn app_with(
    security: SecurityStore,
    clock: Arc<ManualClock>,
    guard: LoginGuard,
    tag: &str,
) -> Router {
    let state = AppState {
        engine: engine_with_sales(tag),
        clock,
        security: Arc::new(Mutex::new(security)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000_000))),
        login_guard: Arc::new(Mutex::new(guard)),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
    };
    build_router(state)
}

async fn login_status(app: &Router, user: &str, password: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "username": user, "password": password }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// Log in and return (status, parsed-body).
async fn login_full(app: &Router, user: &str, password: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "username": user, "password": password }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn login_locks_out_after_repeated_failures_and_releases_after_cooldown() {
    let clock = Arc::new(ManualClock::new(1_000));
    let security = SecurityStore::with_admin("ann", "right", false);
    let app = app_with(
        security,
        clock.clone(),
        LoginGuard::new(5, 900_000),
        "lockout",
    );

    // Five wrong-password attempts: each is a 401.
    for _ in 0..5 {
        assert_eq!(
            login_status(&app, "ann", "wrong").await,
            StatusCode::UNAUTHORIZED
        );
    }
    // Now locked: even the correct password is refused with 429.
    assert_eq!(
        login_status(&app, "ann", "right").await,
        StatusCode::TOO_MANY_REQUESTS
    );

    // Within the cooldown it stays locked.
    clock.advance(899_000);
    assert_eq!(
        login_status(&app, "ann", "right").await,
        StatusCode::TOO_MANY_REQUESTS
    );

    // Past the cooldown the correct password works again.
    clock.advance(2_000); // now 1_000 + 901_000 > locked_until (1_000 + 900_000)
    assert_eq!(login_status(&app, "ann", "right").await, StatusCode::OK);
}

#[tokio::test]
async fn a_successful_login_resets_the_failure_counter() {
    let clock = Arc::new(ManualClock::new(1_000));
    let security = SecurityStore::with_admin("ann", "right", false);
    let app = app_with(
        security,
        clock.clone(),
        LoginGuard::new(3, 900_000),
        "reset",
    );

    assert_eq!(
        login_status(&app, "ann", "wrong").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login_status(&app, "ann", "wrong").await,
        StatusCode::UNAUTHORIZED
    );
    // A success clears the counter, so the next failures start fresh.
    assert_eq!(login_status(&app, "ann", "right").await, StatusCode::OK);
    assert_eq!(
        login_status(&app, "ann", "wrong").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login_status(&app, "ann", "wrong").await,
        StatusCode::UNAUTHORIZED
    );
    // Still not locked (only 2 in the fresh window, threshold is 3).
    assert_eq!(login_status(&app, "ann", "right").await, StatusCode::OK);
}

#[tokio::test]
async fn password_change_revokes_other_sessions_but_keeps_current() {
    let clock = Arc::new(ManualClock::new(1_000));
    let security = SecurityStore::with_admin("admin", "pw", true);
    let app = app_with(
        security,
        clock,
        LoginGuard::new(5, 900_000),
        "revoke-sessions",
    );

    // Two independent sessions for the same user.
    let (_, a) = login_full(&app, "admin", "pw").await;
    let token_a = a["token"].as_str().unwrap().to_string();
    let (_, b) = login_full(&app, "admin", "pw").await;
    let token_b = b["token"].as_str().unwrap().to_string();

    let me = |token: String| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/auth/me")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };

    // Change the password using session B.
    let changed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/password")
                .header("authorization", format!("Bearer {token_b}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "current_password": "pw", "new_password": "a-strong-pass-1" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::NO_CONTENT);

    // Session A (the other session) is revoked; session B (current) survives.
    assert_eq!(me(token_a).await, StatusCode::UNAUTHORIZED);
    assert_eq!(me(token_b).await, StatusCode::OK);
}

#[tokio::test]
async fn must_change_password_blocks_data_routes_until_changed() {
    let clock = Arc::new(ManualClock::new(1_000));
    // A bootstrap admin is created with must_change_password = true.
    let dir = std::env::temp_dir().join(format!("epiphany-authhard-sec-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let path = dir.join("security.model");
    let (security, _generated) = SecurityStore::open_or_bootstrap(path, true, Some("pw")).unwrap();
    let app = app_with(
        security,
        clock.clone(),
        LoginGuard::new(5, 900_000),
        "mustchange",
    );

    // Login succeeds and reports the pending change.
    let (status, body) = login_full(&app, "admin", "pw").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["must_change_password"], json!(true));
    let token = body["token"].as_str().unwrap().to_string();

    // A data route is blocked with 403 until the password is changed.
    let blocked = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/cubes")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);

    // Changing the password is allowed even while gated.
    let changed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/password")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "current_password": "pw", "new_password": "a-better-secret" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::NO_CONTENT);

    // The same session now reaches data routes (the gate lifted immediately).
    let ok = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/cubes")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
}
