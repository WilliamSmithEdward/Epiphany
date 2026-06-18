//! Data-spreading integration tests (ADR-0029) over the real router: the four
//! methods through the endpoint, refusal of weighted consolidations, fail-closed
//! element security, and what-if sandbox routing.
//!
//! Determinism (ADR-0009): pinned `ManualClock` and seeded `IdGen`.

use std::collections::BTreeMap;
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
use epiphany_security::{AccessLevel, AuditLog, ObjectKind, Scope, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Region(North,South,East,Total=N+S+E) x Measure(Sales,Cost,Margin=Sales-Cost).
fn cube() -> Cube {
    let mut region = Dimension::new("Region");
    let n = region.add_leaf("North");
    let s = region.add_leaf("South");
    let e = region.add_leaf("East");
    let t = region.add_consolidated("Total");
    region.add_child(t, n, 1).unwrap();
    region.add_child(t, s, 1).unwrap();
    region.add_child(t, e, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    let sales = measure.add_leaf("Sales");
    let cost = measure.add_leaf("Cost");
    let margin = measure.add_consolidated("Margin");
    measure.add_child(margin, sales, 1).unwrap();
    measure.add_child(margin, cost, -1).unwrap();
    Cube::new("Sales", vec![region, measure]).unwrap()
}

struct Harness {
    app: Router,
    engine: Engine,
    security: Arc<Mutex<SecurityStore>>,
}

fn harness(name: &str) -> Harness {
    let dir = std::env::temp_dir().join(format!("epiphany-spread-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    sec.create_user("bob", "pw", false).unwrap();
    for user in ["ann", "bob"] {
        sec.set_grant(
            &Subject::User(user.into()),
            Scope::Global,
            ObjectKind::Cube,
            AccessLevel::Write,
        )
        .unwrap();
    }
    let security = Arc::new(Mutex::new(sec));

    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: security.clone(),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(CalcFactory::new(engine.clone())),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Default::default(),
        automation: Arc::new(Mutex::new(
            epiphany_persist::AutomationStore::open(std::env::temp_dir().join(format!(
                "epiphany-test-auto-{}-spread-0",
                std::process::id()
            )))
            .unwrap(),
        )),
        http: Default::default(),
        sql: Default::default(),
    };
    Harness {
        app: build_router(state),
        engine,
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
    sandbox: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if let Some(sb) = sandbox {
        builder = builder.header("x-epiphany-sandbox", sb);
    }
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

fn spread_body(region: &str, measure: &str, value: &str, method: &str) -> Value {
    json!({
        "target": { "Region": region, "Measure": measure },
        "value": value,
        "method": method,
    })
}

/// Read leaf Sales values [North, South, East] from a fresh snapshot.
fn leaf_sales(engine: &Engine) -> Vec<String> {
    let snap = engine.snapshot("Sales").unwrap();
    let region = |m: &str| snap.cube().dimension(0).resolve(m).unwrap();
    let sales = snap.cube().dimension(1).resolve("Sales").unwrap();
    ["North", "South", "East"]
        .iter()
        .map(|r| {
            snap.cube()
                .get_leaf(&[region(r), sales])
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn equal_spread_splits_across_leaves_exactly() {
    let h = harness("equal");
    let t = login(&h.app, "admin").await;
    let (status, _) = send(
        &h.app,
        "POST",
        "/api/v1/cubes/Sales/cells/spread",
        &t,
        None,
        Some(spread_body("Total", "Sales", "100", "equal")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // 100 / 3 = 33.3334, 33.3333, 33.3333 (remainder to the first leaf); sums to 100.
    assert_eq!(leaf_sales(&h.engine), vec!["33.3334", "33.3333", "33.3333"]);
}

#[tokio::test]
async fn proportional_spread_weighs_by_current_values() {
    let h = harness("prop");
    let t = login(&h.app, "admin").await;
    // Seed North=10, South=30, East=0.
    let snap = h.engine.snapshot("Sales").unwrap();
    let region = |m: &str| snap.cube().dimension(0).resolve(m).unwrap();
    let sales = snap.cube().dimension(1).resolve("Sales").unwrap();
    h.engine
        .apply_batch(
            "Sales",
            None,
            &[
                CellWrite::Leaf {
                    coord: vec![region("North"), sales],
                    value: Fixed::from(10),
                },
                CellWrite::Leaf {
                    coord: vec![region("South"), sales],
                    value: Fixed::from(30),
                },
            ],
        )
        .unwrap();
    let (status, _) = send(
        &h.app,
        "POST",
        "/api/v1/cubes/Sales/cells/spread",
        &t,
        None,
        Some(spread_body("Total", "Sales", "100", "proportional")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(leaf_sales(&h.engine), vec!["25", "75", "0"]);
}

#[tokio::test]
async fn repeat_and_clear() {
    let h = harness("repeatclear");
    let t = login(&h.app, "admin").await;
    send(
        &h.app,
        "POST",
        "/api/v1/cubes/Sales/cells/spread",
        &t,
        None,
        Some(spread_body("Total", "Sales", "5", "repeat")),
    )
    .await;
    assert_eq!(leaf_sales(&h.engine), vec!["5", "5", "5"]);
    send(
        &h.app,
        "POST",
        "/api/v1/cubes/Sales/cells/spread",
        &t,
        None,
        Some(spread_body("Total", "Sales", "0", "clear")),
    )
    .await;
    assert_eq!(leaf_sales(&h.engine), vec!["0", "0", "0"]);
}

#[tokio::test]
async fn weighted_consolidation_is_refused() {
    let h = harness("weighted");
    let t = login(&h.app, "admin").await;
    // Margin rolls up Cost with weight -1, so it cannot be spread.
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/cubes/Sales/cells/spread",
        &t,
        None,
        Some(spread_body("North", "Margin", "100", "equal")),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "SPREAD_WEIGHTED");
    // Nothing was written.
    assert_eq!(leaf_sales(&h.engine), vec!["0", "0", "0"]);
}

#[tokio::test]
async fn spread_is_denied_when_a_contributing_leaf_is_restricted() {
    let h = harness("elemsec");
    // Restrict Region/East to bob only; ann may not write East.
    h.security
        .lock()
        .unwrap()
        .set_element_access(
            "Sales",
            "Region",
            "East",
            &Subject::User("bob".into()),
            AccessLevel::Write,
        )
        .unwrap();
    let ann = login(&h.app, "ann").await;
    let (status, _) = send(
        &h.app,
        "POST",
        "/api/v1/cubes/Sales/cells/spread",
        &ann,
        None,
        Some(spread_body("Total", "Sales", "100", "equal")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Fail-closed: nothing was written, including the leaves ann could write.
    assert_eq!(leaf_sales(&h.engine), vec!["0", "0", "0"]);
}

#[tokio::test]
async fn spread_into_a_sandbox_leaves_the_base_unchanged() {
    let h = harness("sandbox");
    let t = login(&h.app, "admin").await;
    // Create a sandbox, then spread within it.
    let (status, _) = send(
        &h.app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &t,
        None,
        Some(json!({ "name": "what" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = send(
        &h.app,
        "POST",
        "/api/v1/cubes/Sales/cells/spread",
        &t,
        Some("what"),
        Some(spread_body("Total", "Sales", "90", "equal")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The base is untouched; the what-if overlay holds the spread.
    assert_eq!(leaf_sales(&h.engine), vec!["0", "0", "0"]);
    let snap = h.engine.snapshot("Sales").unwrap();
    let sandbox = snap.model().sandbox("what").expect("sandbox exists");
    assert_eq!(sandbox.cells.len(), 3, "three leaves staged in the sandbox");
}
