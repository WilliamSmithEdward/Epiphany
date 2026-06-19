//! M6 acceptance: the Phase 6 definition of done (ROADMAP section 6), proven end
//! to end over the real router with the rule-aware resolver.
//!
//! "A user enters what-if numbers in a sandbox, sees rules and consolidations
//! recompute over them without affecting base data, then commits or discards."
//!
//! Determinism (ADR-0009): the clock is a pinned `ManualClock` and ids come from
//! a seeded `IdGen`, so every run is reproducible.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, CalcFactory, SessionStore};
use epiphany_core::{Cube, Dimension};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::Engine;
use epiphany_mdx::MdxEvaluator;
use epiphany_persist::Store;
use epiphany_security::{AuditLog, SecurityStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-m6-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// Region(North,South,Total=N+S) x Measure(Sales,Cost,Margin); Margin is derived
/// by a rule so the cube exercises both rule recompute and consolidation rollup.
fn margin_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let n = region.add_leaf("North");
    let s = region.add_leaf("South");
    let t = region.add_consolidated("Total");
    region.add_child(t, n, 1).unwrap();
    region.add_child(t, s, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    measure.add_leaf("Cost");
    measure.add_leaf("Margin");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

/// The full server stack: rule-aware CalcFactory, real MDX evaluator, one admin.
fn router(dir: &Path) -> Router {
    let store = Store::create(dir.to_path_buf(), margin_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));
    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(SecurityStore::with_admin("ann", "pw", true))),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(CalcFactory::new(engine)),
        command_connectors_enabled: false,
        secure_cookies: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Default::default(),
        automation: Arc::new(Mutex::new(
            epiphany_persist::AutomationStore::open(std::env::temp_dir().join(format!(
                "epiphany-test-auto-{}-m6_acceptance-0",
                std::process::id()
            )))
            .unwrap(),
        )),
        http: Default::default(),
        sql: Default::default(),
    };
    build_router(state)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

async fn login(app: &Router) -> String {
    let body = json!({ "username": "ann", "password": "pw" }).to_string();
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
    body_json(resp).await["token"].as_str().unwrap().to_string()
}

/// One API call, optionally carrying the sandbox-selector header.
async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
    sandbox: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    if let Some(name) = sandbox {
        builder = builder.header("x-epiphany-sandbox", name);
    }
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// Read one cell's value, optionally through a sandbox.
async fn read(
    app: &Router,
    token: &str,
    region: &str,
    measure: &str,
    sandbox: Option<&str>,
) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        token,
        Some(json!({ "coords": [{ "Region": region, "Measure": measure }] })),
        sandbox,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["cells"][0]["value"].as_str().unwrap_or("").to_string()
}

/// Seed the base model: the Margin rule and the leaf data, all on base.
async fn seed(app: &Router, token: &str) {
    let (s, _) = call(
        app,
        "PUT",
        "/api/v1/cubes/Sales/rules",
        token,
        Some(json!({
            "source": "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];"
        })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/batch",
        token,
        Some(json!({ "writes": [
            { "coord": { "Region": "North", "Measure": "Sales" }, "value": "100" },
            { "coord": { "Region": "North", "Measure": "Cost" }, "value": "60" },
            { "coord": { "Region": "South", "Measure": "Sales" }, "value": "200" },
            { "coord": { "Region": "South", "Measure": "Cost" }, "value": "150" }
        ]})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn dod_what_if_recompute_then_commit() {
    let dir = scratch("commit");
    let app = router(&dir);
    let token = login(&app).await;
    seed(&app, &token).await;

    // Base: Margin = Sales - Cost; Total/Margin = 40 + 50 = 90.
    assert_eq!(read(&app, &token, "Total", "Margin", None).await, "90");

    // The user opens a sandbox and enters a what-if number: North/Sales = 150.
    let (s, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &token,
        Some(json!({ "name": "plan" })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, written) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &token,
        Some(json!({ "coord": { "Region": "North", "Measure": "Sales" }, "value": "150" })),
        Some("plan"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(written["value"], "150");
    assert_eq!(written["overlaid"], true);

    // In the sandbox: the Margin rule recomputes (150 - 60 = 90 at North) and the
    // Total consolidation rolls it up (90 + 50 = 140); Total/Sales = 350.
    assert_eq!(
        read(&app, &token, "North", "Margin", Some("plan")).await,
        "90"
    );
    assert_eq!(
        read(&app, &token, "Total", "Margin", Some("plan")).await,
        "140"
    );
    assert_eq!(
        read(&app, &token, "Total", "Sales", Some("plan")).await,
        "350"
    );

    // Base data is untouched while the what-if is uncommitted.
    assert_eq!(read(&app, &token, "Total", "Margin", None).await, "90");
    assert_eq!(read(&app, &token, "North", "Sales", None).await, "100");

    // Commit merges the what-if into base.
    let (s, c) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes/plan/commit",
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["committed"], 1);

    // Base now reflects the committed what-if, and a base read confirms the
    // recomputed rollups.
    assert_eq!(read(&app, &token, "North", "Sales", None).await, "150");
    assert_eq!(read(&app, &token, "Total", "Margin", None).await, "140");
    assert_eq!(read(&app, &token, "Total", "Sales", None).await, "350");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn dod_what_if_then_discard_leaves_base_unchanged() {
    let dir = scratch("discard");
    let app = router(&dir);
    let token = login(&app).await;
    seed(&app, &token).await;

    // Enter a what-if in a sandbox, see it recompute, then discard it.
    call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &token,
        Some(json!({ "name": "scratch" })),
        None,
    )
    .await;
    let (s, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &token,
        Some(json!({ "coord": { "Region": "North", "Measure": "Sales" }, "value": "999" })),
        Some("scratch"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        read(&app, &token, "Total", "Margin", Some("scratch")).await,
        "989"
    ); // (999-60)+50

    // Discard the sandbox.
    let (s, _) = call(
        &app,
        "DELETE",
        "/api/v1/cubes/Sales/sandboxes/scratch",
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Base is exactly as seeded: the discarded what-if left no trace.
    assert_eq!(read(&app, &token, "North", "Sales", None).await, "100");
    assert_eq!(read(&app, &token, "Total", "Margin", None).await, "90");
    // The sandbox is gone.
    let (s, _) = call(
        &app,
        "GET",
        "/api/v1/cubes/Sales/sandboxes/scratch",
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(&dir).ok();
}
