//! M4 acceptance suite: "it calculates" (end of Phase 4).
//!
//! Proves the Phase 4 definition of done end to end over the REAL router, under
//! deterministic mode (fixed admin, ManualClock, seeded IdGen, tempdir Stores,
//! injected CalcFactory so reads are rule-aware): a user defines rules on a
//! same-cube model (Margin = Sales - Cost) and a cross-cube model (a national
//! roll-up that reads the first cube's rule-derived consolidation), and then:
//!
//!   * rule-derived leaves and their consolidations read back exact values;
//!   * a cross-cube reference resolves a rule-and-consolidation-derived cell in
//!     another cube;
//!   * feeders are auto-inferred for the analyzable cube and validated with no
//!     under-feed and no over-feed, while the cross-cube-only rules are honestly
//!     reported as un-analyzable (opaque) rather than silently mis-fed;
//!   * "explain" returns a provenance trace whose value agrees with the read and
//!     whose structure distinguishes a firing rule from a consolidation;
//!   * the model's rule unit tests run green (including a what-if fixture);
//!   * every one of the above survives a server restart over the same data dir.
//!
//! The model is analyzable-by-construction: every populated input drives a
//! single rule-target leaf, every fed leaf has a non-zero value, and no
//! non-zero rule leaf is left unfed. This is the binding, non-flaky CI gate
//! for M4.

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
    let dir = std::env::temp_dir().join(format!("epiphany-m4-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// Region(North,South,Total) x Measure(Sales,Cost,Margin), with Sales and Cost
/// populated so Margin = Sales - Cost is a clean, analyzable leaf rule.
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
    // Margin is a rule-derived leaf (not a consolidation), so it carries inferred
    // feeders and rolls up to Total via the Region dimension.
    measure.add_leaf("Margin");

    let mut cube = Cube::new("Sales", vec![region, measure]).unwrap();
    let mut set = |r, m, v: i32| cube.set_leaf(&[r, m], Fixed::from(v)).unwrap();
    set(north, sales, 100);
    set(north, cost, 60);
    set(south, sales, 200);
    set(south, cost, 150);
    cube
}

/// Metric(NationalMargin, NorthMargin): a tiny cube whose values come entirely
/// from cross-cube references into the Sales cube's rule-derived Margin.
fn consol_cube() -> Cube {
    let mut metric = Dimension::new("Metric");
    metric.add_leaf("NationalMargin");
    metric.add_leaf("NorthMargin");
    Cube::new("Consol", vec![metric]).unwrap()
}

/// Build the router over Stores at `dir/cubes/{Sales,Consol}` (created if absent,
/// else reopened), with a fixed admin and the rule-aware CalcFactory injected so
/// reads reflect rules and cross-cube references.
fn router_for(dir: &Path) -> Router {
    let open_or_create = |name: &str, build: fn() -> Cube| {
        let cube_dir = dir.join("cubes").join(name);
        if cube_dir.join("snapshot.model").is_file() {
            Store::open(cube_dir).unwrap()
        } else {
            Store::create(cube_dir, build()).unwrap()
        }
    };
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), open_or_create("Sales", sales_cube));
    stores.insert("Consol".to_string(), open_or_create("Consol", consol_cube));

    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));
    let cells = Arc::new(epiphany_api::CalcFactory::new(engine.clone()));
    let state = AppState {
        engine,
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells,
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
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

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
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

/// Read one cell's numeric value (decimal string) via the rule-aware read path.
async fn read_value(app: &Router, token: &str, cube: &str, coord: Value) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/v1/cubes/{cube}/cells/read"),
        token,
        Some(json!({ "coords": [coord] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read {cube}: {body}");
    body["cells"][0]["value"].as_str().unwrap().to_string()
}

const MARGIN_RULE: &str =
    "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];";

// Two cross-cube rules, each pinning every dimension of the referenced cube
// (cross-cube references must be fully addressed).
const CONSOL_RULES: &str = concat!(
    "['Metric':'NationalMargin'] = 'Sales'!['Region':'Total', 'Measure':'Margin'];\n",
    "['Metric':'NorthMargin'] = 'Sales'!['Region':'North', 'Measure':'Margin'];"
);

fn margin_coord(region: &str) -> Value {
    json!({ "Region": region, "Measure": "Margin" })
}

#[tokio::test]
async fn m4_definition_of_done() {
    let dir = scratch("dod");

    // --- Session 1: define rules and tests, then verify every capability ---
    {
        let app = router_for(&dir);
        let token = login(&app).await;

        // 1. A bad rule is rejected at define time with a located parse error,
        //    and nothing is persisted.
        let (status, err) = call(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/rules",
            &token,
            Some(json!({ "source": "['Measure':'Margin'] = value['Measure':'Sales' -;" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
        assert_eq!(err["error"]["code"], "RULE_PARSE_ERROR");
        assert!(err["error"]["details"]["line"].is_number());

        // 2. Define the analyzable same-cube rule, and the cross-cube roll-up.
        let (status, _) = call(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/rules",
            &token,
            Some(json!({ "source": MARGIN_RULE })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &app,
            "PUT",
            "/api/v1/cubes/Consol/rules",
            &token,
            Some(json!({ "source": CONSOL_RULES })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // 3. Rule-derived leaves and their consolidation read back exactly.
        assert_eq!(
            read_value(&app, &token, "Sales", margin_coord("North")).await,
            "40"
        );
        assert_eq!(
            read_value(&app, &token, "Sales", margin_coord("South")).await,
            "50"
        );
        assert_eq!(
            read_value(&app, &token, "Sales", margin_coord("Total")).await,
            "90"
        );

        // 4. A cross-cube reference resolves the other cube's rule-and-
        //    consolidation-derived cell.
        assert_eq!(
            read_value(
                &app,
                &token,
                "Consol",
                json!({ "Metric": "NationalMargin" })
            )
            .await,
            "90"
        );
        assert_eq!(
            read_value(&app, &token, "Consol", json!({ "Metric": "NorthMargin" })).await,
            "40"
        );

        // 5. Feeders for the analyzable cube are auto-inferred and validated with
        //    no under-feed and no over-feed.
        let (status, diag) = call(
            &app,
            "GET",
            "/api/v1/cubes/Sales/feeders/diagnostics",
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{diag}");
        assert_eq!(diag["fed_cell_count"], 2, "North/Margin and South/Margin");
        assert_eq!(
            diag["under_fed"].as_array().unwrap().len(),
            0,
            "no under-feed"
        );
        assert_eq!(
            diag["over_fed"].as_array().unwrap().len(),
            0,
            "no over-feed"
        );
        assert_eq!(diag["opaque_rules"].as_array().unwrap().len(), 0);

        // 5b. The cross-cube-only rules are honestly reported as un-analyzable
        //     (opaque) rather than silently treated as fully fed.
        let (status, diag) = call(
            &app,
            "GET",
            "/api/v1/cubes/Consol/feeders/diagnostics",
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{diag}");
        assert_eq!(
            diag["opaque_rules"].as_array().unwrap().len(),
            2,
            "both cross-cube rules cannot be auto-fed"
        );
        assert_eq!(diag["fed_cell_count"], 0);

        // 6. Explain a rule-derived leaf: a firing rule over two stored inputs.
        let (status, trace) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/cells/explain",
            &token,
            Some(json!({ "coord": margin_coord("North"), "depth": "full" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{trace}");
        assert_eq!(trace["kind"], "rule");
        assert_eq!(trace["value"], "40");
        let inputs = trace["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 2, "Sales and Cost");
        assert!(inputs.iter().all(|i| i["kind"] == "stored"));

        // 6b. Explain the consolidation: a roll-up of two rule-derived leaves,
        //     value agreeing with the read.
        let (status, trace) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/cells/explain",
            &token,
            Some(json!({ "coord": margin_coord("Total"), "depth": "full" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{trace}");
        assert_eq!(trace["kind"], "consolidation");
        assert_eq!(trace["value"], "90");
        assert_eq!(trace["contributions"], 2);
        assert!(trace["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] == "rule"));

        // 7. Rule unit tests: a live check and a what-if fixture, both green.
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/rules/tests",
            &token,
            Some(json!({
                "name": "live_margin",
                "fixtures": [],
                "assertions": [
                    { "coord": { "Region": "North", "Measure": "Margin" }, "value": "40" },
                    { "coord": { "Region": "Total", "Measure": "Margin" }, "value": "90" }
                ]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/rules/tests",
            &token,
            Some(json!({
                "name": "whatif_margin",
                "fixtures": [
                    { "coord": { "Region": "North", "Measure": "Sales" }, "value": "300" },
                    { "coord": { "Region": "North", "Measure": "Cost" }, "value": "120" },
                    { "coord": { "Region": "South", "Measure": "Sales" }, "value": "400" },
                    { "coord": { "Region": "South", "Measure": "Cost" }, "value": "100" }
                ],
                "assertions": [
                    { "coord": { "Region": "North", "Measure": "Margin" }, "value": "180" },
                    { "coord": { "Region": "South", "Measure": "Margin" }, "value": "300" },
                    { "coord": { "Region": "Total", "Measure": "Margin" }, "value": "480" }
                ]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, report) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/rules/tests/run",
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["all_passed"], true, "rule tests green: {report}");
        assert_eq!(report["outcomes"].as_array().unwrap().len(), 2);
    }

    // --- Session 2: restart over the same data directory ---
    {
        let app = router_for(&dir);
        let token = login(&app).await;

        // 8. Rules, cross-cube references, and rule tests all survived the restart.
        assert_eq!(
            read_value(&app, &token, "Sales", margin_coord("Total")).await,
            "90"
        );
        assert_eq!(
            read_value(
                &app,
                &token,
                "Consol",
                json!({ "Metric": "NationalMargin" })
            )
            .await,
            "90"
        );

        let (status, diag) = call(
            &app,
            "GET",
            "/api/v1/cubes/Sales/feeders/diagnostics",
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            diag["fed_cell_count"], 2,
            "feeders re-inferred after restart"
        );
        assert_eq!(diag["under_fed"].as_array().unwrap().len(), 0);

        let (status, report) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/rules/tests/run",
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            report["all_passed"], true,
            "tests persisted and pass: {report}"
        );
        assert_eq!(report["outcomes"].as_array().unwrap().len(), 2);
    }

    std::fs::remove_dir_all(&dir).ok();
}
