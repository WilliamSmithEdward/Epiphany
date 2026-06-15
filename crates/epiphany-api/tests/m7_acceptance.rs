//! M7 acceptance: the Phase 7 definition of done (ROADMAP section 6), proven end
//! to end over the real router with the rule-aware resolver.
//!
//! "A non-admin user sees and edits only permitted cubes and elements; an admin
//! manages it from the UI [REST]; audited actions produce correct, append-only,
//! deterministic-timestamp audit records that survive a restart, contain no
//! secrets or PII, and are queryable by an admin from the UI and REST."
//!
//! Determinism (ADR-0009): pinned `ManualClock` and seeded `IdGen`, so timestamps
//! and ids are reproducible.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, CalcFactory, SessionStore};
use epiphany_core::{Cube, Dimension, Fixed};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::{CellWrite, Engine};
use epiphany_mdx::MdxEvaluator;
use epiphany_persist::Store;
use epiphany_security::{AuditLog, SecurityStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-m7-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let n = region.add_leaf("North");
    let s = region.add_leaf("South");
    let t = region.add_consolidated("Total");
    region.add_child(t, n, 1).unwrap();
    region.add_child(t, s, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

struct Harness {
    app: Router,
}

fn harness(dir: &Path, audit_path: std::path::PathBuf) -> Harness {
    let store = Store::create(dir.to_path_buf(), sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));

    let snap = engine.snapshot("Sales").unwrap();
    let region = |m: &str| snap.cube().dimension(0).resolve(m).unwrap();
    let measure = |m: &str| snap.cube().dimension(1).resolve(m).unwrap();
    let leaf = |r: &str, v: i32| CellWrite::Leaf {
        coord: vec![region(r), measure("Sales")],
        value: Fixed::from(v),
    };
    engine
        .apply_batch("Sales", None, &[leaf("North", 100), leaf("South", 200)])
        .unwrap();

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    sec.create_user("bob", "pw", false).unwrap();
    // The DoD narrative starts from an open cube and then restricts it, so this
    // acceptance runs the opt-in open posture (the closed default is the secure
    // out-of-box behavior, covered by tests/cube_default_access.rs).
    sec.set_default_cube_open(true);

    let audit = AuditLog::open(audit_path).unwrap();
    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(CalcFactory::new(engine)),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(audit)),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
    };
    Harness {
        app: build_router(state),
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

async fn send(
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
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

async fn read_status(app: &Router, token: &str, region: &str) -> StatusCode {
    send(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        token,
        Some(json!({ "coords": [{ "Region": region, "Measure": "Sales" }] })),
    )
    .await
    .0
}

async fn write_status(app: &Router, token: &str, region: &str) -> StatusCode {
    send(
        app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        token,
        Some(json!({ "coord": { "Region": region, "Measure": "Sales" }, "value": "1" })),
    )
    .await
    .0
}

#[tokio::test]
async fn non_admin_is_confined_by_object_and_element_security_managed_via_rest() {
    let h = harness(
        &scratch("confine"),
        scratch("confine-audit").join("audit.log"),
    );
    let admin = login(&h.app, "admin").await;
    let ann = login(&h.app, "ann").await;
    let bob = login(&h.app, "bob").await;

    // Before any grant the cube is open: every authenticated user reads.
    assert_eq!(read_status(&h.app, &ann, "Total").await, StatusCode::OK);

    // The admin restricts the cube to ann (Read only) via the REST admin surface.
    let (status, _) = send(
        &h.app,
        "PUT",
        "/api/v1/acl/objects",
        &admin,
        Some(json!({
            "kind": "cube", "name": "Sales",
            "subject_kind": "user", "subject": "ann", "level": "read"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Object security now holds: bob (ungranted) is denied the cube entirely --
    // cells and the cube's connection list alike;
    // ann reads but, granted only Read, cannot write; the admin bypasses.
    assert_eq!(
        read_status(&h.app, &bob, "North").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&h.app, "GET", "/api/v1/cubes/Sales/connections", &bob, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(read_status(&h.app, &ann, "North").await, StatusCode::OK);
    assert_eq!(
        write_status(&h.app, &ann, "North").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(write_status(&h.app, &admin, "North").await, StatusCode::OK);

    // The admin layers element security: restricting Region/South (granting it to
    // bob, who has no cube access anyway) denies ann the member and any rollup.
    let (status, _) = send(
        &h.app,
        "PUT",
        "/api/v1/acl/elements",
        &admin,
        Some(json!({
            "cube": "Sales", "dimension": "Region", "element": "South",
            "subject_kind": "user", "subject": "bob", "level": "read"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // ann: North readable, South denied, Total denied (rolls up South).
    assert_eq!(read_status(&h.app, &ann, "North").await, StatusCode::OK);
    assert_eq!(
        read_status(&h.app, &ann, "South").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read_status(&h.app, &ann, "Total").await,
        StatusCode::FORBIDDEN
    );
    // The admin still sees everything.
    assert_eq!(read_status(&h.app, &admin, "Total").await, StatusCode::OK);

    // The denials were audited and are queryable by the admin (not by a non-admin).
    let (status, audit) = send(&h.app, "GET", "/api/v1/audit?outcome=denied", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    let denials = audit["records"].as_array().unwrap();
    assert!(denials.iter().any(|r| r["action"] == "access_denied"));
    assert_eq!(
        send(&h.app, "GET", "/api/v1/audit", &ann, None).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn audit_is_appendonly_deterministic_survives_restart_and_carries_no_secrets() {
    let audit_path = scratch("audit-restart").join("audit.log");

    // First "boot": some audited actions, then drop the whole stack (a restart).
    {
        let h = harness(&scratch("restart-a"), audit_path.clone());
        let admin = login(&h.app, "admin").await; // audited: login
        let ann = login(&h.app, "ann").await; // audited: login
                                              // A denied admin attempt by a non-admin -> access_denied.
        send(&h.app, "GET", "/api/v1/users", &ann, None).await;
        // A user change by the admin -> user_change.
        send(
            &h.app,
            "POST",
            "/api/v1/users",
            &admin,
            Some(json!({ "username": "carl", "password": "s3cret-pw" })),
        )
        .await;
    }

    // Second "boot": reopen the SAME audit file (recovery) and a fresh stack.
    let h = harness(&scratch("restart-b"), audit_path);
    let admin = login(&h.app, "admin").await;
    let (status, audit) = send(&h.app, "GET", "/api/v1/audit", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    let records = audit["records"].as_array().unwrap();

    // The pre-restart records survived (append-only, recovered from disk).
    assert!(records.iter().any(|r| r["action"] == "access_denied"));
    assert!(records.iter().any(|r| r["action"] == "user_change"));
    assert!(records.iter().any(|r| r["action"] == "login"));

    // Sequence numbers are strictly increasing (append-only ordering).
    let seqs: Vec<u64> = records.iter().map(|r| r["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));

    // Timestamps come from the injected clock (deterministic, never the wall clock).
    assert!(records.iter().all(|r| r["timestamp_millis"] == 1_000));

    // No secrets or PII (RG-13): the created user's PASSWORD never appears in any
    // record's fields; the user-change target names the user, not the credential.
    let blob = audit.to_string();
    assert!(
        !blob.contains("s3cret-pw"),
        "a password leaked into the audit log"
    );
}
