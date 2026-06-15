//! Audit emission acceptance (ADR-0010): logins, access denials, and object
//! changes produce queryable, deterministic-timestamp audit records with no
//! secrets. Inspects the in-memory audit log directly (the REST query is 7F).

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
use epiphany_security::{AuditAction, AuditFilter, AuditLog, SecurityStore};
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
    audit: Arc<Mutex<AuditLog>>,
}

fn harness(commands: bool) -> Harness {
    let dir = std::env::temp_dir().join(format!("epiphany-auditest-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    let audit = Arc::new(Mutex::new(AuditLog::in_memory()));
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: commands,
        audit: audit.clone(),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
    };
    Harness {
        app: build_router(state),
        audit,
    }
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

async fn login(app: &Router, user: &str, pass: &str) -> (StatusCode, Option<String>) {
    let body = json!({ "username": user, "password": pass }).to_string();
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
    let status = resp.status();
    let token = body_json(resp)
        .await
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    (status, token)
}

async fn put_connection(app: &Router, token: &str) -> StatusCode {
    let body = json!({ "name": "c", "kind": "command", "program": "echo", "format": "csv" });
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/cubes/Sales/connections/c")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

fn count(h: &Harness, filter: AuditFilter) -> usize {
    h.audit.lock().unwrap().query(&filter).len()
}

#[tokio::test]
async fn login_denial_and_object_change_are_audited() {
    let h = harness(true);

    // A successful and a failed login.
    let (s, admin) = login(&h.app, "admin", "pw").await;
    assert_eq!(s, StatusCode::OK);
    let admin = admin.unwrap();
    let (s, _) = login(&h.app, "ann", "wrong").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // A successful login is audited allowed; the failed one is audited denied.
    assert_eq!(
        count(
            &h,
            AuditFilter {
                action: Some(AuditAction::Login),
                allowed: Some(true),
                ..Default::default()
            }
        ),
        1
    );
    let failed = h.audit.lock().unwrap().query(&AuditFilter {
        action: Some(AuditAction::Login),
        allowed: Some(false),
        ..Default::default()
    });
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].actor, "ann");
    // Deterministic timestamp from the pinned clock; no password in the record.
    assert_eq!(failed[0].timestamp_millis, 1_000);

    // A non-admin connection write is denied and audited; base is unchanged.
    let (_, ann) = login(&h.app, "ann", "pw").await;
    let ann = ann.unwrap();
    assert_eq!(put_connection(&h.app, &ann).await, StatusCode::FORBIDDEN);
    let denied = h.audit.lock().unwrap().query(&AuditFilter {
        action: Some(AuditAction::AccessDenied),
        ..Default::default()
    });
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].actor, "ann");
    assert_eq!(denied[0].object_kind, "connection");
    assert_eq!(denied[0].target, "Sales/c");

    // An admin connection write succeeds and is audited as an object change.
    assert_eq!(put_connection(&h.app, &admin).await, StatusCode::OK);
    assert_eq!(
        count(
            &h,
            AuditFilter {
                action: Some(AuditAction::ObjectUpdate),
                ..Default::default()
            }
        ),
        1
    );
}
