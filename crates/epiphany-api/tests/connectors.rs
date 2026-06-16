//! Connector acceptance: a flow ingests rows from a command (process-execution)
//! connection, end to end over the real router, plus the security gates that
//! fence host execution (ADR-0012 decision 6): admin-only definition and the
//! server enable flag.
//!
//! The connection runs the platform shell to emit fixed CSV (the test's own
//! choosing; a production connection names python/pwsh/an exe). Determinism is
//! preserved: the command output is fixed, and a flow unit test would pin inline
//! rows rather than touch a live connection.

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
use epiphany_security::{AccessLevel, AuditLog, ObjectKind, Scope, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-conn-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_consolidated("Total");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

/// Build a router. `is_admin` controls the (single) user's role; `commands`
/// toggles the command-connector enable gate.
fn router_for(dir: &Path, is_admin: bool, commands: bool) -> Router {
    let cube_dir = dir.join("cubes").join("Sales");
    let store = if cube_dir.join("snapshot.model").is_file() {
        Store::open(cube_dir).unwrap()
    } else {
        Store::create(cube_dir, sales_cube()).unwrap()
    };
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let mut sec = SecurityStore::with_admin("admin", "pw", is_admin);
    // The redaction test reads connections as a non-admin, so grant the actor the
    // Connection:Read it now needs (ADR-0023). Read, not Write: defining a
    // connection still requires Connection:Write, which a non-admin lacks here.
    sec.set_grant(
        &Subject::User("admin".into()),
        Scope::Global,
        ObjectKind::Connection,
        AccessLevel::Read,
    )
    .unwrap();
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: commands,
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

const LOAD_FLOW: &str = "\
function rows(ctx) {
  const data = ctx.input();
  const regions = data.map(function (r) { return r.Region; });
  ctx.ensureElements('Region', regions);
  regions.forEach(function (name) { ctx.addChild('Region', 'Total', name, 1); });
  ctx.writeCells(data.map(function (r) {
    return { coord: { Region: r.Region, Measure: 'Sales' }, value: r.Value };
  }));
}";

/// A command connection that emits fixed CSV via the platform shell.
fn emit_csv_connection() -> Value {
    #[cfg(windows)]
    let (program, args) = (
        "cmd",
        json!(["/C", "echo Region,Value&&echo North,100&&echo South,200"]),
    );
    #[cfg(not(windows))]
    let (program, args) = (
        "sh",
        json!(["-c", "printf 'Region,Value\\nNorth,100\\nSouth,200\\n'"]),
    );
    json!({
        "name": "emit",
        "kind": "command",
        "program": program,
        "args": args,
        "format": "csv",
        "timeout_ms": 10000
    })
}

async fn read_total(app: &Router, token: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        token,
        Some(json!({ "coords": [{ "Region": "Total", "Measure": "Sales" }] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["cells"][0]["value"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn flow_ingests_from_a_command_connection() {
    let dir = scratch("happy");
    let app = router_for(&dir, true, true);
    let token = login(&app).await;

    // Store the flow and define the command connection (admin + enabled).
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/flows/load",
        &token,
        Some(json!({ "name": "load", "source": LOAD_FLOW })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/connections/emit",
        &token,
        Some(emit_csv_connection()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Run the flow with the connection as its source.
    let (status, report) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/flows/load/run",
        &token,
        Some(json!({ "connection": "emit" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["rows_read"], 2);
    assert_eq!(report["cells_written"], 2);
    assert_eq!(report["elements_added"], 2);

    // The program's rows were loaded and consolidate.
    assert_eq!(read_total(&app, &token).await, "300");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn defining_a_command_connection_requires_the_enable_gate() {
    let dir = scratch("disabled");
    let app = router_for(&dir, true, false); // admin, but commands disabled
    let token = login(&app).await;

    let (status, err) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/connections/emit",
        &token,
        Some(emit_csv_connection()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["code"], "FORBIDDEN");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn connection_preview_returns_sample_rows() {
    let dir = scratch("preview");
    let app = router_for(&dir, true, true);
    let token = login(&app).await;

    // Define the connection, then test it via the preview endpoint (ADR-0027).
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/connections/emit",
        &token,
        Some(emit_csv_connection()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/connections/emit/preview",
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["row_count"], 2);
    let columns: Vec<&str> = body["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert_eq!(columns, vec!["Region", "Value"]);
    assert_eq!(body["rows"].as_array().unwrap().len(), 2);
    assert_eq!(body["rows"][0][0], "North");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn connection_working_dir_must_be_absolute_without_traversal() {
    let dir = scratch("workdir");
    let app = router_for(&dir, true, true);
    let token = login(&app).await;
    let put = |conn: Value| {
        let app = app.clone();
        let token = token.clone();
        async move {
            call(
                &app,
                "PUT",
                "/api/v1/cubes/Sales/connections/emit",
                &token,
                Some(conn),
            )
            .await
            .0
        }
    };

    // A relative working_dir is rejected.
    let mut conn = emit_csv_connection();
    conn["working_dir"] = json!("scripts");
    assert_eq!(put(conn.clone()).await, StatusCode::UNPROCESSABLE_ENTITY);

    // A '..' traversal is rejected (per platform's absolute form).
    #[cfg(windows)]
    let bad = "C:\\data\\..\\secret";
    #[cfg(not(windows))]
    let bad = "/data/../secret";
    conn["working_dir"] = json!(bad);
    assert_eq!(put(conn.clone()).await, StatusCode::UNPROCESSABLE_ENTITY);

    // A clean absolute path is accepted.
    #[cfg(windows)]
    let good = "C:\\epiphany\\scripts";
    #[cfg(not(windows))]
    let good = "/epiphany/scripts";
    conn["working_dir"] = json!(good);
    assert_eq!(put(conn).await, StatusCode::OK);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn defining_a_connection_requires_admin() {
    let dir = scratch("nonadmin");
    let app = router_for(&dir, false, true); // commands enabled, but not admin
    let token = login(&app).await;

    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/connections/emit",
        &token,
        Some(emit_csv_connection()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn non_admin_sees_redacted_command_line() {
    let dir = scratch("redact");
    // An admin defines the connection (durable).
    let admin = router_for(&dir, true, true);
    let atok = login(&admin).await;
    let (status, _) = call(
        &admin,
        "PUT",
        "/api/v1/cubes/Sales/connections/emit",
        &atok,
        Some(emit_csv_connection()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The admin sees the full command line.
    let (_, alist) = call(
        &admin,
        "GET",
        "/api/v1/cubes/Sales/connections",
        &atok,
        None,
    )
    .await;
    assert!(!alist["connections"][0]["program"]
        .as_str()
        .unwrap()
        .is_empty());

    // A non-admin (reopened over the same data dir) sees the name but not the
    // program or args.
    let user = router_for(&dir, false, true);
    let utok = login(&user).await;
    let (status, ulist) = call(&user, "GET", "/api/v1/cubes/Sales/connections", &utok, None).await;
    assert_eq!(status, StatusCode::OK);
    let conn = &ulist["connections"][0];
    assert_eq!(conn["name"], "emit");
    assert_eq!(conn["program"], "", "program redacted for non-admins");
    assert_eq!(conn["args"].as_array().unwrap().len(), 0, "args redacted");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn an_unknown_output_format_is_rejected() {
    let dir = scratch("format");
    let app = router_for(&dir, true, true);
    let token = login(&app).await;
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/connections/bad",
        &token,
        Some(json!({ "name": "bad", "kind": "command", "program": "echo", "format": "xml" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_failing_connector_reports_an_error() {
    let dir = scratch("error");
    let app = router_for(&dir, true, true);
    let token = login(&app).await;

    call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/flows/load",
        &token,
        Some(json!({ "name": "load", "source": LOAD_FLOW })),
    )
    .await;
    // A connection whose program does not exist.
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/connections/bad",
        &token,
        Some(json!({
            "name": "bad",
            "kind": "command",
            "program": "epiphany-no-such-program-xyz",
            "args": [],
            "format": "csv",
            "timeout_ms": 5000
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, err) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/flows/load/run",
        &token,
        Some(json!({ "connection": "bad" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["error"]["code"], "CONNECTOR_ERROR");

    std::fs::remove_dir_all(&dir).ok();
}
