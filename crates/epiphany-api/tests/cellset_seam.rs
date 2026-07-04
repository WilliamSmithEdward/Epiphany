//! Integration coverage for the pass-3b cellset/view-cache seam fixes:
//!
//! - a write to a CROSS-CUBE dependency invalidates a dependent cached cellset
//!   (the view cache keys on referenced cubes' versions, not the target's alone);
//! - a String-kind cell renders as `kind:"string"` with its text, never a
//!   fabricated numeric zero, and a populated string is non-zero for suppression;
//! - a per-cell DivByZero degrades to a `kind:"error"` marker on that one cell
//!   instead of aborting the whole cellset;
//! - feeder diagnostics on a cube whose rules stopped compiling returns a clear
//!   422 (`RULE_COMPILE_ERROR`), not a 500.
//!
//! Determinism (ADR-0009): pinned `ManualClock`, seeded `IdGen`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, CalcFactory, SessionStore, ViewCache};
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
    let dir = std::env::temp_dir().join(format!("epiphany-cellset-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// Build a router over the given stores with the rule-aware `CalcFactory`, an
/// admin user, and an explicit (shared) view cache so the test can read counters.
fn router_with(stores: BTreeMap<String, Store>, cache: Arc<ViewCache>, name: &str) -> Router {
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));
    let sec = SecurityStore::with_admin("admin", "pw", true);
    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(CalcFactory::new(engine.clone())),
        command_connectors_enabled: false,
        secure_cookies: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: cache,
        secrets: Default::default(),
        automation: Arc::new(Mutex::new(
            epiphany_persist::AutomationStore::open(scratch(&format!("{name}-auto"))).unwrap(),
        )),
        http: Default::default(),
        sql: Default::default(),
    };
    build_router(state)
}

async fn login(app: &Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "username": "admin", "password": "pw" }).to_string(),
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

async fn post(app: &Router, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
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

async fn put(app: &Router, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
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

async fn get(app: &Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
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

// ---------------------------------------------------------------------------
// Item 1: a write to a CROSS-CUBE dependency invalidates a dependent cached
// cellset. Sales.Revenue = Units * FX!Rate; a write to FX (bumping FX's version,
// not Sales') must make the next Sales cellset recompute.
// ---------------------------------------------------------------------------

fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Units");
    measure.add_leaf("Revenue");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

fn fx_cube() -> Cube {
    let mut pair = Dimension::new("Pair");
    pair.add_leaf("USD");
    Cube::new("FX", vec![pair]).unwrap()
}

fn revenue_view() -> Value {
    json!({
        "rows": [
            { "dimension": "Measure", "type": "members", "members": ["Revenue"] }
        ],
        "columns": [
            { "dimension": "Region", "type": "members", "members": ["North"] }
        ]
    })
}

#[tokio::test]
async fn write_to_referenced_cube_invalidates_dependent_cellset() {
    let sales_store = Store::create(scratch("xdep-sales"), sales_cube()).unwrap();
    let fx_store = Store::create(scratch("xdep-fx"), fx_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), sales_store);
    stores.insert("FX".to_string(), fx_store);
    let cache: Arc<ViewCache> = Arc::new(ViewCache::new(64));
    let app = router_with(stores, cache.clone(), "xdep");
    let token = login(&app).await;

    // Seed Units = 10 (Sales), Rate = 3 (FX), and the cross-cube rule.
    let (s, _) = put(
        &app,
        "/api/v1/cubes/Sales/cell",
        &token,
        json!({ "coord": { "Region": "North", "Measure": "Units" }, "value": "10" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = put(
        &app,
        "/api/v1/cubes/FX/cell",
        &token,
        json!({ "coord": { "Pair": "USD" }, "value": "3" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = put(
        &app,
        "/api/v1/cubes/Sales/rules",
        &token,
        json!({ "source": "['Measure':'Revenue'] = value['Measure':'Units'] * 'FX'!['Pair':'USD'];" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // First cellset: Revenue = 10 * 3 = 30 (a miss).
    let (s, cs1) = post(&app, "/api/v1/cubes/Sales/cellset", &token, revenue_view()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cs1["cells"][0]["value"], "30", "Units(10) * Rate(3)");
    let misses_after_first = cache.misses();

    // A repeat read HITS (nothing changed): same numbers, no recompute.
    let (_, cs_again) = post(&app, "/api/v1/cubes/Sales/cellset", &token, revenue_view()).await;
    assert_eq!(cs_again["cells"][0]["value"], "30");
    assert_eq!(
        cache.misses(),
        misses_after_first,
        "an unchanged repeat read is a cache hit"
    );
    assert!(cache.hits() >= 1);

    // Write FX Rate 3 -> 5. This bumps FX's version, NOT Sales'. Pre-fix the Sales
    // cellset was keyed only on Sales' version, so the stale 30 was served forever.
    let (s, _) = put(
        &app,
        "/api/v1/cubes/FX/cell",
        &token,
        json!({ "coord": { "Pair": "USD" }, "value": "5" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // The next Sales cellset must recompute against the new FX rate: 10 * 5 = 50.
    let (s, cs2) = post(&app, "/api/v1/cubes/Sales/cellset", &token, revenue_view()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        cs2["cells"][0]["value"], "50",
        "a write to the referenced FX cube invalidated the dependent cellset"
    );
    assert!(
        cache.misses() > misses_after_first,
        "the cross-cube write forced a recompute (miss)"
    );
}

// ---------------------------------------------------------------------------
// Item 4: a String-kind cell renders as kind:"string" with its text, not a
// numeric zero, and a populated string counts as non-zero for zero-suppression.
// ---------------------------------------------------------------------------

/// Region(North) x Measure(Sales:numeric, Note:string), with Note text stored.
fn string_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    measure.add_string("Note");
    let mut cube = Cube::new("Comments", vec![region, measure]).unwrap();
    let north = cube.dimension(0).resolve("North").unwrap();
    let note = cube.dimension(1).resolve("Note").unwrap();
    // Sales stays 0 (numeric); Note carries text.
    cube.set_string(&[north, note], "hello").unwrap();
    cube
}

#[tokio::test]
async fn string_cell_renders_as_string_not_numeric_zero() {
    let store = Store::create(scratch("strcell"), string_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Comments".to_string(), store);
    let app = router_with(stores, Arc::new(ViewCache::new(64)), "strcell");
    let token = login(&app).await;

    // Rows: Measure(Sales, Note); Columns: Region(North). Two cells:
    //   (Sales, North)  -> numeric "0"
    //   (Note,  North)  -> string "hello"
    let view = json!({
        "rows": [
            { "dimension": "Measure", "type": "members", "members": ["Sales", "Note"] }
        ],
        "columns": [
            { "dimension": "Region", "type": "members", "members": ["North"] }
        ]
    });
    let (s, cs) = post(&app, "/api/v1/cubes/Comments/cellset", &token, view).await;
    assert_eq!(s, StatusCode::OK);

    let cells = cs["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2);
    // Sales row: a genuine numeric zero.
    assert_eq!(cells[0]["kind"], "numeric");
    assert_eq!(cells[0]["value"], "0");
    // Note row: kind "string" with the text, NOT numeric "0", and not editable.
    assert_eq!(
        cells[1]["kind"], "string",
        "a string cell is kind:string, not a fabricated numeric zero"
    );
    assert_eq!(cells[1]["value"], "hello");
    assert_eq!(cells[1]["editable"], false);
}

#[tokio::test]
async fn populated_string_is_not_zero_suppressed() {
    let store = Store::create(scratch("strsupp"), string_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Comments".to_string(), store);
    let app = router_with(stores, Arc::new(ViewCache::new(64)), "strsupp");
    let token = login(&app).await;

    // Suppress zero rows. The Sales row is all-zero (suppressed); the Note row has
    // a populated string and MUST survive (a string is non-zero content).
    let view = json!({
        "rows": [
            { "dimension": "Measure", "type": "members", "members": ["Sales", "Note"] }
        ],
        "columns": [
            { "dimension": "Region", "type": "members", "members": ["North"] }
        ],
        "suppress_zero_rows": true
    });
    let (s, cs) = post(&app, "/api/v1/cubes/Comments/cellset", &token, view).await;
    assert_eq!(s, StatusCode::OK);

    let rows = cs["row_tuples"].as_array().unwrap();
    let names: Vec<&str> = rows
        .iter()
        .map(|t| t[0]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["Note"],
        "the all-zero Sales row is suppressed; the populated string Note row survives"
    );
    assert_eq!(cs["suppressed"]["row_tuples"], 1);
}

// ---------------------------------------------------------------------------
// Item 5: a per-cell DivByZero degrades to a kind:"error" marker on that one cell
// instead of aborting the whole cellset. One healthy cell still returns its value.
// ---------------------------------------------------------------------------

/// Region(North, South) x Measure(Num, Den, Ratio). Ratio = Num / Den. North has
/// Den = 2 (Ratio ok); South has Den = 0 (Ratio divides by zero).
fn ratio_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    region.add_leaf("South");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Num");
    measure.add_leaf("Den");
    measure.add_leaf("Ratio");
    Cube::new("Ratios", vec![region, measure]).unwrap()
}

#[tokio::test]
async fn per_cell_divbyzero_degrades_not_blanks_the_cellset() {
    let store = Store::create(scratch("ratio"), ratio_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Ratios".to_string(), store);
    let app = router_with(stores, Arc::new(ViewCache::new(64)), "ratio");
    let token = login(&app).await;

    // North: Num=10, Den=2 (Ratio=5). South: Num=10, Den=0 (Ratio = DivByZero).
    for (region, measure, value) in [
        ("North", "Num", "10"),
        ("North", "Den", "2"),
        ("South", "Num", "10"),
        ("South", "Den", "0"),
    ] {
        let (s, _) = put(
            &app,
            "/api/v1/cubes/Ratios/cell",
            &token,
            json!({ "coord": { "Region": region, "Measure": measure }, "value": value }),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, _) = put(
        &app,
        "/api/v1/cubes/Ratios/rules",
        &token,
        json!({ "source": "['Measure':'Ratio'] = value['Measure':'Num'] / value['Measure':'Den'];" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Rows: Region(North, South); Columns: Measure(Ratio). North=5, South errors.
    let view = json!({
        "rows": [
            { "dimension": "Region", "type": "members", "members": ["North", "South"] }
        ],
        "columns": [
            { "dimension": "Measure", "type": "members", "members": ["Ratio"] }
        ]
    });
    let (s, cs) = post(&app, "/api/v1/cubes/Ratios/cellset", &token, view).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "one DivByZero cell must NOT abort the whole cellset: {cs}"
    );
    let cells = cs["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2);
    // North/Ratio: a healthy numeric value survives.
    assert_eq!(cells[0]["kind"], "numeric");
    assert_eq!(cells[0]["value"], "5");
    // South/Ratio: a per-cell error marker, not a blanked cellset.
    assert_eq!(cells[1]["kind"], "error");
    assert!(cells[1]["value"].is_null(), "an errored cell has no value");
    assert!(
        cells[1]["error"]
            .as_str()
            .unwrap()
            .contains("division by zero"),
        "the per-cell error message is surfaced: {}",
        cells[1]["error"]
    );
}

// ---------------------------------------------------------------------------
// Item 3: feeder diagnostics on a cube whose rules stopped compiling returns a
// clear 422 (RULE_COMPILE_ERROR), not a 500.
// ---------------------------------------------------------------------------

/// Region(North) x Measure(Sales, Cost, Margin), Margin = Sales - Cost.
fn margin_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    measure.add_leaf("Cost");
    measure.add_leaf("Margin");
    Cube::new("Books", vec![region, measure]).unwrap()
}

#[tokio::test]
async fn feeder_diagnostics_on_broken_rules_is_422_not_500() {
    let store = Store::create(scratch("feeddiag"), margin_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Books".to_string(), store);
    let app = router_with(stores, Arc::new(ViewCache::new(64)), "feeddiag");
    let token = login(&app).await;

    // A valid rule referencing Cost.
    let (s, _) = put(
        &app,
        "/api/v1/cubes/Books/rules",
        &token,
        json!({ "source": "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Diagnostics work while the rules compile.
    let (s, _diag) = get(&app, "/api/v1/cubes/Books/feeders/diagnostics", &token).await;
    assert_eq!(s, StatusCode::OK, "healthy rules -> diagnostics OK");

    // Delete Cost (the rule now fails to compile; no revalidation runs).
    let (s, body) = post(
        &app,
        "/api/v1/cubes/Books/dimensions/Measure/edit",
        &token,
        json!({ "op": "delete", "element": "Cost" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "delete Cost: {body}");

    // Feeder diagnostics on the now-broken cube: a clear 422, not a 500.
    let (s, body) = get(&app, "/api/v1/cubes/Books/feeders/diagnostics", &token).await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "broken rules -> 422, not 500: {body}"
    );
    assert_eq!(body["error"]["code"], "RULE_COMPILE_ERROR");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("feeder"),
        "the message explains feeders cannot be inferred: {body}"
    );
}
