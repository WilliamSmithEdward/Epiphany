//! SQL-connector integration tests (ADR-0034) over the real router. They prove
//! the fail-closed gates (capability + host allowlist), that a referenced
//! password secret must exist, that a connection round-trips referencing the
//! secret by NAME (never a value), and that the fetch path is feature-gated.
//!
//! The live query itself needs a real database (the documented impure boundary),
//! so the default build has no `postgres` feature: a preview of a fully-valid,
//! gated connection then reports the connector is not built, exercising the gate.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, SessionStore, SqlConnectorConfig};
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

fn harness(name: &str, sql: SqlConnectorConfig, secrets: &[(&str, &str)]) -> Router {
    let dir = std::env::temp_dir().join(format!("epiphany-sql-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let auto_dir =
        std::env::temp_dir().join(format!("epiphany-sql-auto-{}-{name}", std::process::id()));
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
        secure_cookies: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Arc::new(Mutex::new(secret_store)),
        automation: Arc::new(Mutex::new(
            epiphany_persist::AutomationStore::open(auto_dir).unwrap(),
        )),
        http: Default::default(),
        sql,
    };
    build_router(state)
}

fn sql_enabled() -> SqlConnectorConfig {
    SqlConnectorConfig {
        enabled: true,
        allowed_hosts: vec!["db.internal".to_string()],
    }
}

fn sql_conn(host: &str, secret: Option<&str>) -> Value {
    let mut body = json!({
        "kind": "sql",
        "engine": "postgres",
        "host": host,
        "port": 5432,
        "database": "analytics",
        "user": "reporting",
        "query": "SELECT region, amount::text FROM sales",
        "ssl_mode": "require",
    });
    if let Some(s) = secret {
        body["password_secret"] = json!(s);
    }
    body
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

#[tokio::test]
async fn sql_connection_requires_the_capability() {
    // Capability off (default): defining a sql connection is forbidden.
    let app = harness("disabled", SqlConnectorConfig::default(), &[]);
    let admin = login(&app, "admin").await;
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(sql_conn("db.internal", None)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn sql_connection_host_must_be_allowlisted() {
    let app = harness("allowlist", sql_enabled(), &[]);
    let admin = login(&app, "admin").await;
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(sql_conn("db.evil.example.net", None)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a non-allowlisted database host is refused"
    );
}

#[tokio::test]
async fn sql_connection_unknown_secret_is_rejected() {
    let app = harness("nosecret", sql_enabled(), &[]);
    let admin = login(&app, "admin").await;
    let (status, body) = call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(sql_conn("db.internal", Some("missing"))),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "UNKNOWN_SECRET");
}

#[tokio::test]
async fn sql_connection_defines_and_round_trips() {
    let app = harness("define", sql_enabled(), &[("pw", "s3cret")]);
    let admin = login(&app, "admin").await;
    let (status, body) = call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(sql_conn("db.internal", Some("pw"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "sql");
    assert_eq!(body["host"], "db.internal");
    assert_eq!(body["database"], "analytics");
    assert_eq!(body["password_secret"], "pw");
    // The password value is never echoed, only its secret name.
    assert!(!body.to_string().contains("s3cret"));

    let (status, got) = call(&app, "GET", "/api/v1/connections/feed", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["host"], "db.internal");
    assert_eq!(got["query"], "SELECT region, amount::text FROM sales");
    assert_eq!(got["password_secret"], "pw");
    assert!(!got.to_string().contains("s3cret"));
}

#[tokio::test]
async fn sql_mysql_connection_defines_and_round_trips() {
    let app = harness("mysql", sql_enabled(), &[("pw", "s3cret")]);
    let admin = login(&app, "admin").await;
    let body = json!({
        "kind": "sql",
        "engine": "mysql",
        "host": "db.internal",
        "port": 3306,
        "database": "app",
        "user": "reporting",
        "query": "SELECT region, amount FROM sales",
        "ssl_mode": "require",
        "password_secret": "pw",
    });
    let (status, got) = call(
        &app,
        "PUT",
        "/api/v1/connections/mariadb",
        &admin,
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["kind"], "sql");
    assert_eq!(got["engine"], "mysql");
    assert_eq!(got["host"], "db.internal");
    assert_eq!(got["password_secret"], "pw");
    assert!(!got.to_string().contains("s3cret"));
}

// In the default build (no `postgres`/`mysql` feature) a preview of a fully-valid, gated
// sql connection reports the connector is not compiled in, proving the feature
// gate. With `--features postgres` the query would run instead (needs a live DB).
#[cfg(not(feature = "postgres"))]
#[tokio::test]
async fn sql_preview_reports_not_built_without_the_feature() {
    let app = harness("notbuilt", sql_enabled(), &[("pw", "s3cret")]);
    let admin = login(&app, "admin").await;
    call(
        &app,
        "PUT",
        "/api/v1/connections/feed",
        &admin,
        Some(sql_conn("db.internal", Some("pw"))),
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
    assert_eq!(body["error"]["code"], "SQL_NOT_BUILT");
}
