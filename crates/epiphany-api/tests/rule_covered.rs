//! Integration coverage for ADR-0040: writes to rule-covered (rule-calculated)
//! leaf cells are rejected, spreading excludes them, and the `editable` DTO flag
//! matches the enforcement. Over the REAL router with the rule-aware CalcFactory
//! injected, deterministic (fixed admin, ManualClock, seeded IdGen, tempdir Store).
//!
//! Model: Region(North,South,Total) x Measure(Sales,Cost,Margin-leaf). Rules make
//! every `(*, Margin)` a rule-derived leaf, and pin `(North, Sales)` to a constant
//! so one leaf of the `(Total, Sales)` rollup is covered and another is not.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, SessionStore};
use epiphany_core::{Cube, Dimension, Fixed};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::Engine;
use epiphany_mdx::MdxEvaluator;
use epiphany_persist::Store;
use epiphany_security::{AuditLog, SecurityStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-rulecov-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// Region(North,South,Total) x Measure(Sales,Cost,Margin). Sales/Cost seeded so the
/// margin rule is analyzable; Margin is a plain leaf the rule computes.
fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let north = region.add_leaf("North");
    let south = region.add_leaf("South");
    let total = region.add_consolidated("Total");
    region.add_child(total, north, 1).unwrap();
    region.add_child(total, south, 1).unwrap();

    let mut measure = Dimension::new("Measure");
    let sales = measure.add_leaf("Sales");
    let cost = measure.add_leaf("Cost");
    measure.add_leaf("Margin");

    let mut cube = Cube::new("Sales", vec![region, measure]).unwrap();
    let mut set = |r, m, v: i32| cube.set_leaf(&[r, m], Fixed::from(v)).unwrap();
    set(north, sales, 100);
    set(north, cost, 60);
    set(south, sales, 200);
    set(south, cost, 150);
    cube
}

fn router_for(dir: &Path) -> Router {
    let store = Store::create(dir.join("cubes").join("Sales"), sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));
    let cells = Arc::new(epiphany_api::CalcFactory::new(engine.clone()));
    let state = AppState {
        engine,
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells,
        command_connectors_enabled: false,
        secure_cookies: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Default::default(),
        automation: Arc::new(Mutex::new(
            epiphany_persist::AutomationStore::open(std::env::temp_dir().join(format!(
                "epiphany-test-auto-{}-rule_covered",
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
    let body = json!({ "username": "admin", "password": "pw" }).to_string();
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

/// Call a route, optionally in a what-if sandbox (X-Epiphany-Sandbox).
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
    if let Some(sb) = sandbox {
        builder = builder.header("x-epiphany-sandbox", sb);
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

/// Both rules: `(*, Margin)` is rule-derived, and `(North, Sales)` is pinned to a
/// constant (so one leaf of the Total/Sales rollup is covered, the other is not).
const RULES: &str = concat!(
    "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];\n",
    "['Region':'North','Measure':'Sales'] = 500;"
);

async fn seed_rules(app: &Router, token: &str) {
    let (s, v) = call(
        app,
        "PUT",
        "/api/v1/cubes/Sales/rules",
        token,
        Some(json!({ "source": RULES })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "define rules: {v}");
}

async fn read_value(app: &Router, token: &str, coord: Value) -> String {
    let (s, body) = call(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        token,
        Some(json!({ "coords": [coord] })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "read: {body}");
    body["cells"][0]["value"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn write_to_a_rule_covered_leaf_is_rejected_but_an_uncovered_leaf_is_writable() {
    let app = router_for(&scratch("write"));
    let token = login(&app).await;
    seed_rules(&app, &token).await;

    // (North, Margin) is computed by a rule -> 422 RULE_COVERED_CELL, nothing stored.
    let (s, err) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &token,
        Some(json!({ "coord": { "Region": "North", "Measure": "Margin" }, "value": "7" })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["error"]["code"], "RULE_COVERED_CELL");

    // (South, Sales) is an ordinary leaf -> still writable.
    let (s, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &token,
        Some(json!({ "coord": { "Region": "South", "Measure": "Sales" }, "value": "222" })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        read_value(
            &app,
            &token,
            json!({ "Region": "South", "Measure": "Sales" })
        )
        .await,
        "222"
    );
}

#[tokio::test]
async fn batch_write_is_rejected_atomically_if_any_target_is_rule_covered() {
    let app = router_for(&scratch("batch"));
    let token = login(&app).await;
    seed_rules(&app, &token).await;

    // One good write and one rule-covered write: the whole batch is rejected and
    // NOTHING is applied (South/Sales stays at its seeded 200).
    let (s, err) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/batch",
        &token,
        Some(json!({ "writes": [
            { "coord": { "Region": "South", "Measure": "Sales" }, "value": "222" },
            { "coord": { "Region": "North", "Measure": "Margin" }, "value": "7" }
        ] })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["error"]["code"], "RULE_COVERED_CELL");
    assert_eq!(
        read_value(
            &app,
            &token,
            json!({ "Region": "South", "Measure": "Sales" })
        )
        .await,
        "200",
        "the good write must not have landed (all-or-nothing)"
    );
}

#[tokio::test]
async fn a_sandbox_write_to_a_rule_covered_leaf_is_rejected_too() {
    let app = router_for(&scratch("sandbox"));
    let token = login(&app).await;
    seed_rules(&app, &token).await;

    let (s, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &token,
        Some(json!({ "name": "wi" })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    // A what-if overlay is consulted beneath the rules (ADR-0014), so a sandbox
    // write to a rule-covered leaf is just as futile as a base write -> rejected.
    let (s, err) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &token,
        Some(json!({ "coord": { "Region": "North", "Measure": "Margin" }, "value": "7" })),
        Some("wi"),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["error"]["code"], "RULE_COVERED_CELL");
}

#[tokio::test]
async fn spread_into_an_all_rule_covered_consolidation_is_rejected() {
    let app = router_for(&scratch("spread-all"));
    let token = login(&app).await;
    seed_rules(&app, &token).await;

    // (Total, Margin) rolls up North/Margin and South/Margin, both rule-covered:
    // there is nowhere to place the value, so the spread is rejected.
    let (s, err) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/spread",
        &token,
        Some(json!({
            "target": { "Region": "Total", "Measure": "Margin" },
            "value": "100",
            "method": "equal"
        })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["error"]["code"], "RULE_COVERED_CELL");
}

#[tokio::test]
async fn spread_excludes_rule_covered_leaves_and_reproduces_the_total() {
    let app = router_for(&scratch("spread-mixed"));
    let token = login(&app).await;
    seed_rules(&app, &token).await;

    // (Total, Sales) rolls up North/Sales (rule-covered = 500) and South/Sales
    // (ordinary). The covered leaf is excluded, so all of 90 lands on South/Sales,
    // reproducing the entered total on read-back; North/Sales still reads its rule.
    let (s, body) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/spread",
        &token,
        Some(json!({
            "target": { "Region": "Total", "Measure": "Sales" },
            "value": "90",
            "method": "equal"
        })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["applied"], 1, "only the one uncovered leaf is written");
    assert_eq!(
        read_value(
            &app,
            &token,
            json!({ "Region": "South", "Measure": "Sales" })
        )
        .await,
        "90"
    );
    assert_eq!(
        read_value(
            &app,
            &token,
            json!({ "Region": "North", "Measure": "Sales" })
        )
        .await,
        "500",
        "the rule-covered leaf keeps its rule value"
    );
}

#[tokio::test]
async fn read_cells_reports_editable_false_for_a_rule_covered_leaf() {
    let app = router_for(&scratch("editable-read"));
    let token = login(&app).await;
    seed_rules(&app, &token).await;

    let (s, body) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &token,
        Some(json!({ "coords": [
            { "Region": "North", "Measure": "Margin" },
            { "Region": "South", "Measure": "Sales" }
        ] })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(
        body["cells"][0]["editable"], false,
        "a rule-covered leaf is not editable"
    );
    assert_eq!(
        body["cells"][1]["editable"], true,
        "an ordinary leaf is editable"
    );
}

#[tokio::test]
async fn cellset_reports_editable_false_for_a_rule_covered_leaf() {
    let app = router_for(&scratch("editable-cellset"));
    let token = login(&app).await;
    seed_rules(&app, &token).await;

    // Row South x Columns {Sales, Margin}: South/Sales is editable, South/Margin
    // (rule-derived) is not — the display flag matches the write-path enforcement.
    let view = json!({
        "rows": [ { "dimension": "Region", "type": "members", "members": ["South"] } ],
        "columns": [ { "dimension": "Measure", "type": "members", "members": ["Sales", "Margin"] } ]
    });
    let (s, cs) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cellset",
        &token,
        Some(view),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{cs}");
    let cells = cs["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2, "one row x two columns: {cs}");
    assert_eq!(cells[0]["editable"], true, "South/Sales is editable");
    assert_eq!(cells[1]["editable"], false, "South/Margin is rule-covered");
}
