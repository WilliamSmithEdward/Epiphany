//! M5 acceptance suite: "flows" (end of Phase 5).
//!
//! Proves the Phase 5 definition of done end to end over the REAL router, under
//! deterministic mode (fixed admin, ManualClock, seeded IdGen, tempdir Store): a
//! user writes a TypeScript flow that reads a CSV, builds dimension elements,
//! and loads cell values; runs it from the REST API and gets row/cell/element
//! counts back; the loaded values (and their consolidation) read back exactly; a
//! malformed flow is rejected with a located error and never stored; a flow unit
//! test runs green; and all of it survives a server restart over the same data
//! directory. A guided CSV import leg loads more data without a hand-written
//! flow.
//!
//! This is the binding, non-flaky CI gate for M5.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
use epiphany_security::{AuditLog, SecurityStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-m5-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// Region(Total, no leaves yet) x Measure(Sales): the flow builds the Region
/// leaves under Total and loads Sales values, so the cube starts almost empty.
fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_consolidated("Total");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

fn router_for(dir: &Path) -> Router {
    let cube_dir = dir.join("cubes").join("Sales");
    let store = if cube_dir.join("snapshot.model").is_file() {
        Store::open(cube_dir).unwrap()
    } else {
        Store::create(cube_dir, sales_cube()).unwrap()
    };
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
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

async fn read_value(app: &Router, token: &str, coord: Value) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        token,
        Some(json!({ "coords": [coord] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read: {body}");
    body["cells"][0]["value"].as_str().unwrap().to_string()
}

fn sales_coord(region: &str) -> Value {
    json!({ "Region": region, "Measure": "Sales" })
}

// A TypeScript flow: read the CSV, build the Region leaves under Total, load the
// Sales values. Exercises the type stripper (annotations) and the host API.
const LOAD_FLOW: &str = "\
function rows(ctx: FlowContext): void {
  const data = ctx.input();
  const regions: string[] = data.map(function (r) { return r.Region; });
  ctx.ensureElements('Region', regions);
  regions.forEach(function (name: string) { ctx.addChild('Region', 'Total', name, 1); });
  const cells = data.map(function (r) {
    return { coord: { Region: r.Region, Measure: 'Sales' }, value: r.Value };
  });
  ctx.writeCells(cells);
}";

const CSV: &str = "Region,Value\nNorth,100\nSouth,200\n";

#[tokio::test]
async fn m5_definition_of_done() {
    let dir = scratch("dod");

    // --- Session 1: author, run, test, import ---
    {
        let app = router_for(&dir);
        let token = login(&app).await;

        // 1. A malformed flow is rejected at define time and not stored.
        let (status, err) = call(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/flows/load",
            &token,
            Some(json!({ "name": "load", "source": "enum Bad { A }\nfunction rows(ctx) {}" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
        assert_eq!(err["error"]["code"], "FLOW_STRIP_ERROR");
        assert!(err["error"]["details"]["line"].is_number());
        let (status, body) = call(&app, "GET", "/api/v1/cubes/Sales/flows", &token, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["flows"].as_array().unwrap().len(),
            0,
            "bad flow not stored"
        );

        // 2. Store the real flow (validated), then run it over the CSV.
        let (status, _) = call(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/flows/load",
            &token,
            Some(json!({ "name": "load", "source": LOAD_FLOW })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, report) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/flows/load/run",
            &token,
            Some(json!({ "input": CSV })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["rows_read"], 2);
        assert_eq!(report["cells_written"], 2);
        assert_eq!(report["elements_added"], 2, "North and South were built");

        // 3. The loaded leaves and their consolidation read back exactly.
        assert_eq!(read_value(&app, &token, sales_coord("North")).await, "100");
        assert_eq!(read_value(&app, &token, sales_coord("South")).await, "200");
        assert_eq!(read_value(&app, &token, sales_coord("Total")).await, "300");

        // 4. A flow unit test runs green.
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/flows/tests",
            &token,
            Some(json!({
                "name": "loads_total",
                "flow": "load",
                "input": CSV,
                "assertions": [
                    { "coord": { "Region": "North", "Measure": "Sales" }, "value": "100" },
                    { "coord": { "Region": "Total", "Measure": "Sales" }, "value": "300" }
                ]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, report) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/flows/tests/run",
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["all_passed"], true, "flow tests green: {report}");
        assert_eq!(report["outcomes"].as_array().unwrap().len(), 1);

        // 5. A flow that throws reports a runtime error (and changes nothing new).
        let (status, _) = call(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/flows/boom",
            &token,
            Some(json!({ "name": "boom", "source": "function rows(ctx) { throw new Error('nope'); }" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, err) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/flows/boom/run",
            &token,
            Some(json!({ "input": "" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err["error"]["code"], "FLOW_RUNTIME_ERROR");
        assert!(err["error"]["message"].as_str().unwrap().contains("nope"));

        // 6. Guided CSV import builds members and loads values without a script.
        let (status, report) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/flows/import",
            &token,
            Some(json!({
                "csv": "Region,Value\nEast,40\nWest,60\n",
                "columns": { "Region": "Region" },
                "value_column": "Value",
                "fixed": { "Measure": "Sales" }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["elements_added"], 2);
        assert_eq!(read_value(&app, &token, sales_coord("East")).await, "40");
        // East and West are NOT under Total (import does not infer hierarchy), so
        // Total still reflects the flow-loaded North + South.
        assert_eq!(read_value(&app, &token, sales_coord("Total")).await, "300");
    }

    // --- Session 2: restart over the same data directory ---
    {
        let app = router_for(&dir);
        let token = login(&app).await;

        // 7. Built members, loaded cells, the flow, and the flow test all
        //    survived the restart.
        assert_eq!(read_value(&app, &token, sales_coord("North")).await, "100");
        assert_eq!(read_value(&app, &token, sales_coord("Total")).await, "300");
        assert_eq!(read_value(&app, &token, sales_coord("East")).await, "40");

        let (status, body) = call(&app, "GET", "/api/v1/cubes/Sales/flows", &token, None).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = body["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"load"), "flow persisted: {names:?}");

        let (status, report) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/flows/tests/run",
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            report["all_passed"], true,
            "flow test persisted and passes: {report}"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
