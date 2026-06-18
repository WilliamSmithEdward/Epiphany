//! HTTP-connector and secret-store integration tests (ADR-0030) over the real
//! router. They prove the fail-closed gates (capability + host allowlist), that
//! secrets are write-only (names listed, values never), that a connection
//! references a secret by name, and that the fetch path is feature-gated.
//!
//! The live fetch itself is covered in epiphany-connect (a localhost server);
//! here the default build has no `http` feature, so a preview reports the
//! connector is not built, which exercises the gate.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, HttpConnectorConfig, SessionStore};
use epiphany_core::{Cube, Dimension};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::Engine;
use epiphany_persist::Store;
use epiphany_security::{AuditLog, SecretStore, SecurityStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    Cube::new("Sales", vec![region]).unwrap()
}

fn harness(name: &str, http: HttpConnectorConfig, secrets: &[(&str, &str)]) -> Router {
    let dir = std::env::temp_dir().join(format!("epiphany-http-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let auto_dir =
        std::env::temp_dir().join(format!("epiphany-http-auto-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&auto_dir).ok();
    let store = Store::create(dir, cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));

    let security = SecurityStore::with_admin("admin", "pw", true);
    let mut secret_store = SecretStore::in_memory();
    for (k, v) in secrets {
        secret_store.set(*k, *v).unwrap();
    }

    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(security)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(epiphany_mdx::MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Arc::new(Mutex::new(secret_store)),
        automation: Arc::new(Mutex::new(
            epiphany_persist::AutomationStore::open(auto_dir).unwrap(),
        )),
        http,
        sql: Default::default(),
    };
    build_router(state)
}

fn http_enabled() -> HttpConnectorConfig {
    HttpConnectorConfig {
        enabled: true,
        allowed_hosts: vec!["api.example.com".to_string()],
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
    serde_json::from_slice::<Value>(&bytes).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

fn http_conn(url: &str, secret: Option<&str>) -> Value {
    let mut body = json!({ "kind": "http", "url": url, "format": "csv" });
    if let Some(s) = secret {
        body["auth"] = json!({ "kind": "bearer", "secret": s });
    }
    body
}

#[tokio::test]
async fn secrets_are_write_only_and_admin_only() {
    let app = harness("secrets", HttpConnectorConfig::default(), &[]);
    let admin = login(&app, "admin").await;

    // Admin sets a secret; the response carries no value.
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/secrets/token",
        &admin,
        Some(json!({ "value": "super-secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The listing exposes the name but never the value.
    let (status, body) = call(&app, "GET", "/api/v1/secrets", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["names"], json!(["token"]));
    assert!(
        !body.to_string().contains("super-secret"),
        "the value must never be returned"
    );

    // Delete it.
    let (status, _) = call(&app, "DELETE", "/api/v1/secrets/token", &admin, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = call(&app, "GET", "/api/v1/secrets", &admin, None).await;
    assert_eq!(body["names"], json!([]));
}

#[tokio::test]
async fn http_connection_requires_the_capability() {
    // Capability off (default): defining an http connection is forbidden.
    let app = harness("disabled", HttpConnectorConfig::default(), &[]);
    let admin = login(&app, "admin").await;
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(http_conn("https://api.example.com/data.csv", None)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn http_connection_host_must_be_allowlisted() {
    let app = harness("allowlist", http_enabled(), &[]);
    let admin = login(&app, "admin").await;
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(http_conn("https://evil.example.net/data.csv", None)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "non-allowlisted host is refused"
    );
}

#[tokio::test]
async fn http_connection_unknown_secret_is_rejected() {
    let app = harness("nosecret", http_enabled(), &[]);
    let admin = login(&app, "admin").await;
    let (status, body) = call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(http_conn(
            "https://api.example.com/data.csv",
            Some("missing"),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "UNKNOWN_SECRET");
}

#[tokio::test]
async fn http_connection_defines_and_round_trips() {
    let app = harness("define", http_enabled(), &[("tok", "abc123")]);
    let admin = login(&app, "admin").await;
    let (status, body) = call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(http_conn("https://api.example.com/data.csv", Some("tok"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "http");
    assert_eq!(body["url"], "https://api.example.com/data.csv");
    assert_eq!(body["auth"]["kind"], "bearer");
    assert_eq!(body["auth"]["secret"], "tok");

    // It comes back from GET, with the secret referenced by name (never a value).
    let (status, got) = call(&app, "GET", "/api/v1/connections/feed", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["auth"]["secret"], "tok");
    assert!(!got.to_string().contains("abc123"));
}

// In the default build (no `http` feature) a preview of a fully-valid, gated
// http connection reports the connector is not compiled in, proving the feature
// gate. With `--features http` the fetch would run instead (covered in connect).
#[cfg(not(feature = "http"))]
#[tokio::test]
async fn http_preview_reports_not_built_without_the_feature() {
    let app = harness("notbuilt", http_enabled(), &[("tok", "abc123")]);
    let admin = login(&app, "admin").await;
    call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(http_conn("https://api.example.com/data.csv", Some("tok"))),
    )
    .await;
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/connections/feed/preview",
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "HTTP_NOT_BUILT");
}
