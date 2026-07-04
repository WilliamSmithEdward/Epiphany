//! Cross-cube provenance confinement (ADR-0015/0023): `explain_cell` builds the
//! element mask for the TARGET cube only and checks `Cube:Read` only for the
//! target, so a rule that references another cube must not leak that cube's cell
//! value or coordinate member names in the trace to a caller with no read grant on
//! it. The API prunes cross-cube inputs the caller cannot read (fail-closed).
//!
//! Determinism (ADR-0009): pinned `ManualClock`, seeded `IdGen`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, CalcFactory, SessionStore};
use epiphany_core::{Cube, Dimension, Fixed};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::Engine;
use epiphany_mdx::MdxEvaluator;
use epiphany_persist::Store;
use epiphany_security::{AccessLevel, AuditLog, ObjectKind, Scope, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Sales(Region:North, Measure:{Units, Revenue}) with Revenue defined by a rule
/// that multiplies Units by a rate read from the FX cube (a cross-cube reference).
fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Units");
    measure.add_leaf("Revenue");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

/// FX(Pair:USD): a single rate cell the Sales rule reads.
fn fx_cube() -> Cube {
    let mut pair = Dimension::new("Pair");
    pair.add_leaf("USD");
    Cube::new("FX", vec![pair]).unwrap()
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-xcube-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// A two-cube router (Sales references FX) with the rule-aware CalcFactory. The
/// `reader` user gets cube-scoped `Cube:Read` on Sales ONLY (no grant on FX).
fn router(name: &str) -> Router {
    let sales_dir = scratch(&format!("{name}-sales"));
    let fx_dir = scratch(&format!("{name}-fx"));
    let sales_store = Store::create(sales_dir, sales_cube()).unwrap();
    let fx_store = Store::create(fx_dir, fx_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), sales_store);
    stores.insert("FX".to_string(), fx_store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));

    // Seed Units = 10 (Sales) and Rate = 3 (FX); define the cross-cube rule.
    let sales_snap = engine.snapshot("Sales").unwrap();
    let units = sales_snap.cube().dimension(1).resolve("Units").unwrap();
    let north = sales_snap.cube().dimension(0).resolve("North").unwrap();
    engine
        .apply_batch(
            "Sales",
            None,
            &[epiphany_engine::CellWrite::Leaf {
                coord: vec![north, units],
                value: Fixed::from(10),
            }],
        )
        .unwrap();
    let fx_snap = engine.snapshot("FX").unwrap();
    let usd = fx_snap.cube().dimension(0).resolve("USD").unwrap();
    engine
        .apply_batch(
            "FX",
            None,
            &[epiphany_engine::CellWrite::Leaf {
                coord: vec![usd],
                value: Fixed::from(3),
            }],
        )
        .unwrap();
    engine
        .define_rules(
            "Sales",
            None,
            "['Measure':'Revenue'] = value['Measure':'Units'] * 'FX'!['Pair':'USD'];".to_string(),
        )
        .unwrap();

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("reader", "pw", false).unwrap();
    // reader can read Sales but has NO grant on FX (cube-scoped, ADR-0023).
    sec.set_grant(
        &Subject::User("reader".into()),
        Scope::Cube("Sales".into()),
        ObjectKind::Cube,
        AccessLevel::Read,
    )
    .unwrap();

    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
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
            epiphany_persist::AutomationStore::open(scratch(&format!("{name}-auto"))).unwrap(),
        )),
        http: Default::default(),
        sql: Default::default(),
    };
    build_router(state)
}

async fn login(app: &Router, user: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "username": user, "password": "pw" }).to_string(),
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

async fn explain(app: &Router, token: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/cubes/Sales/cells/explain")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "coord": { "Region": "North", "Measure": "Revenue" } }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Collect every `cube` field appearing anywhere in a trace tree.
fn cubes_in(trace: &Value, out: &mut Vec<String>) {
    if let Some(c) = trace["cube"].as_str() {
        out.push(c.to_string());
    }
    if let Some(inputs) = trace["inputs"].as_array() {
        for i in inputs {
            cubes_in(i, out);
        }
    }
}

/// A caller with `Cube:Read` on the target but NOT on a referenced cube must not
/// see the referenced cube anywhere in the provenance trace; the target cell's own
/// value is still returned. An admin (all access) still sees the full trace.
#[tokio::test]
async fn explain_prunes_cross_cube_inputs_the_caller_cannot_read() {
    let app = router("prune");

    // The admin sees the full trace, including the FX input worth 3.
    let admin = login(&app, "admin").await;
    let (status, admin_trace) = explain(&app, &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(admin_trace["value"], "30"); // Units(10) * Rate(3)
    let mut admin_cubes = Vec::new();
    cubes_in(&admin_trace, &mut admin_cubes);
    assert!(
        admin_cubes.iter().any(|c| c == "FX"),
        "admin trace should include the FX cube: {admin_cubes:?}"
    );

    // The reader (Sales-only) gets the same top-level value but NO FX node: the
    // cross-cube value (3) and FX coordinate names never appear.
    let reader = login(&app, "reader").await;
    let (status, trace) = explain(&app, &reader).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trace["value"], "30");
    let mut reader_cubes = Vec::new();
    cubes_in(&trace, &mut reader_cubes);
    assert!(
        reader_cubes.iter().all(|c| c == "Sales"),
        "reader trace must not reference any cube it cannot read: {reader_cubes:?}"
    );
    // Belt and suspenders: the FX rate value must not be serialized anywhere.
    assert!(
        !serde_json::to_string(&trace).unwrap().contains("\"USD\""),
        "the FX coordinate member 'USD' leaked into the reader's trace"
    );
}

/// Issue an authenticated JSON request, returning (status, body).
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
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
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

/// A6/CA4: a rule test on a cube whose rule references ANOTHER cube now RUNS
/// against the pinned multi-cube registry, evaluating on real values, instead of
/// erroring with calc's single-cube `CrossCube` limitation. Sales/Revenue =
/// Units * FX!USD = 10 * 3 = 30, so the assertion holds; a wrong expectation is a
/// reported failure (not a run error), and a fixture that overrides Units flows
/// through the same cross-cube rule.
#[tokio::test]
async fn cross_cube_rule_test_runs_against_the_multi_cube_registry() {
    let app = router("xcube-ruletest");
    let admin = login(&app, "admin").await;

    // A live cross-cube assertion (no fixtures): Revenue = 10 * 3 = 30.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/rules/tests",
        &admin,
        json!({
            "name": "live_revenue",
            "fixtures": [],
            "assertions": [
                { "coord": { "Region": "North", "Measure": "Revenue" }, "value": "30" }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A what-if fixture overriding Units still resolves the cross-cube rate:
    // Revenue = 7 * 3 = 21.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/rules/tests",
        &admin,
        json!({
            "name": "whatif_revenue",
            "fixtures": [
                { "coord": { "Region": "North", "Measure": "Units" }, "value": "7" }
            ],
            "assertions": [
                { "coord": { "Region": "North", "Measure": "Revenue" }, "value": "21" }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The run now SUCCEEDS (pre-A6 this was a 422 CrossCube limitation error) and
    // both cross-cube assertions pass on real values.
    let (status, report) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/rules/tests/run",
        &admin,
        Value::Null,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "cross-cube rule tests must run, not error: {report}"
    );
    assert_eq!(
        report["all_passed"], true,
        "both cross-cube tests pass: {report}"
    );
    assert_eq!(report["outcomes"].as_array().unwrap().len(), 2);

    // A WRONG expectation is a reported assertion failure (values compared), not a
    // run-level error — proving the runner actually evaluates the cross-cube rule.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/rules/tests",
        &admin,
        json!({
            "name": "wrong_revenue",
            "fixtures": [],
            "assertions": [
                { "coord": { "Region": "North", "Measure": "Revenue" }, "value": "999" }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, report) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/rules/tests/run",
        &admin,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        report["all_passed"], false,
        "the wrong assertion fails on value: {report}"
    );
    let wrong = report["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == "wrong_revenue")
        .unwrap();
    assert_eq!(wrong["passed"], false);
    assert_eq!(
        wrong["failures"][0]["actual"], "30",
        "computed the real cross-cube value"
    );
    assert_eq!(wrong["failures"][0]["expected"], "999");
}
