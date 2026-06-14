//! Sandbox lifecycle and ownership acceptance (ADR-0014): create, list, get,
//! discard, and commit over the real router, plus the per-user ownership
//! control (a non-admin may use only their own sandboxes; an admin may use any).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
use epiphany_security::SecurityStore;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-sb-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let n = region.add_leaf("North");
    let t = region.add_consolidated("Total");
    region.add_child(t, n, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

/// A router with three users: admin, plus the non-admins ann and bob.
fn router(dir: &Path) -> Router {
    let store = Store::create(dir.to_path_buf(), sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    sec.create_user("bob", "pw", false).unwrap();
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
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

/// Like [`call`] but with one extra header (e.g. the sandbox selector).
async fn call_h(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
    header: (&str, &str),
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header(header.0, header.1);
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

/// Region(North,South,Total=N+S) x Measure(Sales,Cost,Margin) for the what-if
/// recompute test (Margin is a rule-derived leaf).
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

/// A router with the rule-aware CalcFactory, returning the engine so a test can
/// seed base data, rules, and sandbox overrides directly.
fn router_calc(dir: &Path) -> (Router, Engine) {
    let store = Store::create(dir.to_path_buf(), margin_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));
    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(CalcFactory::new(engine.clone())),
        command_connectors_enabled: false,
    };
    (build_router(state), engine)
}

#[tokio::test]
async fn sandbox_lifecycle_and_ownership() {
    let dir = scratch("lifecycle");
    let app = router(&dir);
    let ann = login(&app, "ann").await;
    let bob = login(&app, "bob").await;
    let admin = login(&app, "admin").await;

    // ann creates a sandbox.
    let (s, v) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &ann,
        Some(json!({ "name": "wi" })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["owner"], "ann");
    assert_eq!(v["cell_count"], 0);

    // A duplicate name is a conflict; a blank name is a bad request.
    let (s, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &ann,
        Some(json!({ "name": "wi" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &ann,
        Some(json!({ "name": "  " })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // ann sees and can read her own sandbox.
    let (s, list) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes", &ann, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["sandboxes"].as_array().unwrap().len(), 1);
    let (s, _) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes/wi", &ann, None).await;
    assert_eq!(s, StatusCode::OK);

    // bob neither sees nor can read ann's sandbox.
    let (s, blist) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes", &bob, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(blist["sandboxes"].as_array().unwrap().len(), 0);
    let (s, _) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes/wi", &bob, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &app,
        "DELETE",
        "/api/v1/cubes/Sales/sandboxes/wi",
        &bob,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // An admin may access any sandbox.
    let (s, _) = call(
        &app,
        "GET",
        "/api/v1/cubes/Sales/sandboxes/wi",
        &admin,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // An unknown sandbox is a 404.
    let (s, _) = call(
        &app,
        "GET",
        "/api/v1/cubes/Sales/sandboxes/ghost",
        &ann,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Committing an empty sandbox is a no-op that reports zero committed cells.
    let (s, c) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes/wi/commit",
        &ann,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["committed"], 0);

    // ann discards her sandbox; the list is then empty.
    let (s, _) = call(
        &app,
        "DELETE",
        "/api/v1/cubes/Sales/sandboxes/wi",
        &ann,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, list) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes", &ann, None).await;
    assert_eq!(list["sandboxes"].as_array().unwrap().len(), 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn sandbox_read_recomputes_what_if_and_marks_overlaid() {
    let dir = scratch("recompute");
    let (app, engine) = router_calc(&dir);
    let ann = login(&app, "ann").await;

    // Resolve element indices once.
    let snap = engine.snapshot("Sales").unwrap();
    let region = snap.cube().dimension(0);
    let measure = snap.cube().dimension(1);
    let (north, south) = (
        region.resolve("North").unwrap(),
        region.resolve("South").unwrap(),
    );
    let (sales, cost) = (
        measure.resolve("Sales").unwrap(),
        measure.resolve("Cost").unwrap(),
    );
    drop(snap);

    // Seed base leaves and the Margin = Sales - Cost rule directly via the engine.
    engine
        .apply_batch(
            "Sales",
            None,
            &[
                CellWrite::Leaf {
                    coord: vec![north, sales],
                    value: Fixed::from(100),
                },
                CellWrite::Leaf {
                    coord: vec![north, cost],
                    value: Fixed::from(60),
                },
                CellWrite::Leaf {
                    coord: vec![south, sales],
                    value: Fixed::from(200),
                },
                CellWrite::Leaf {
                    coord: vec![south, cost],
                    value: Fixed::from(150),
                },
            ],
        )
        .unwrap();
    engine
        .define_rules(
            "Sales",
            None,
            "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];"
                .to_string(),
        )
        .unwrap();

    // A sandbox overriding North/Sales -> 500.
    engine.create_sandbox("Sales", None, "wi", "ann").unwrap();
    engine
        .sandbox_set_cells(
            "Sales",
            None,
            "wi",
            &[CellWrite::Leaf {
                coord: vec![north, sales],
                value: Fixed::from(500),
            }],
        )
        .unwrap();

    let coords = json!({ "coords": [
        { "Region": "North", "Measure": "Sales" },
        { "Region": "North", "Measure": "Margin" },
        { "Region": "Total", "Measure": "Sales" },
        { "Region": "Total", "Measure": "Margin" }
    ]});

    // Without the header: base values, nothing flagged overlaid.
    let (s, base) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &ann,
        Some(coords.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{base}");
    let bc = base["cells"].as_array().unwrap();
    assert_eq!(bc[0]["value"], "100");
    assert_eq!(bc[0]["overlaid"], false);
    assert_eq!(bc[1]["value"], "40"); // North/Margin
    assert_eq!(bc[2]["value"], "300"); // Total/Sales
    assert_eq!(bc[3]["value"], "90"); // Total/Margin

    // With the header: rules and consolidations recompute over the override, and
    // the directly-overridden leaf is flagged.
    let (s, wi) = call_h(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &ann,
        Some(coords),
        ("x-epiphany-sandbox", "wi"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{wi}");
    let wc = wi["cells"].as_array().unwrap();
    assert_eq!(wc[0]["value"], "500");
    assert_eq!(wc[0]["overlaid"], true); // North/Sales override
    assert_eq!(wc[1]["value"], "440");
    assert_eq!(wc[1]["overlaid"], false); // North/Margin recomputed (derived, not flagged)
    assert_eq!(wc[2]["value"], "700"); // Total/Sales rolled up
    assert_eq!(wc[3]["value"], "490"); // Total/Margin recomputed and rolled up

    // The ad-hoc cellset path recomputes too and flags the overlaid cell.
    let body = json!({
        "rows": [{ "dimension": "Region", "type": "members", "members": ["North", "Total"] }],
        "columns": [{ "dimension": "Measure", "type": "members", "members": ["Sales", "Margin"] }]
    });
    let (s, cs) = call_h(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cellset",
        &ann,
        Some(body),
        ("x-epiphany-sandbox", "wi"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{cs}");
    // Row-major: North/Sales, North/Margin, Total/Sales, Total/Margin.
    let cc = cs["cells"].as_array().unwrap();
    assert_eq!(cc[0]["value"], "500");
    assert_eq!(cc[0]["overlaid"], true);
    assert_eq!(cc[1]["value"], "440");
    assert_eq!(cc[2]["value"], "700");
    assert_eq!(cc[3]["value"], "490");

    // Base is untouched: a no-header read still sees 90 at Total/Margin.
    let (_, again) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &ann,
        Some(json!({ "coords": [{ "Region": "Total", "Measure": "Margin" }] })),
    )
    .await;
    assert_eq!(again["cells"][0]["value"], "90");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn sandbox_write_isolates_base_then_commit_merges() {
    let dir = scratch("write-isolate");
    let (app, _engine) = router_calc(&dir);
    let ann = login(&app, "ann").await;

    let cell = |v: &str| json!({ "coord": { "Region": "North", "Measure": "Sales" }, "value": v });
    let read = json!({ "coords": [{ "Region": "North", "Measure": "Sales" }] });

    // Base write North/Sales = 100 (no sandbox).
    let (s, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &ann,
        Some(cell("100")),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Create a sandbox and write North/Sales = 999 into it (header).
    call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &ann,
        Some(json!({ "name": "wi" })),
    )
    .await;
    let (s, w) = call_h(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &ann,
        Some(cell("999")),
        ("x-epiphany-sandbox", "wi"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{w}");
    assert_eq!(w["value"], "999");
    assert_eq!(w["overlaid"], true);

    // Base read is unchanged; the sandbox read sees the what-if value.
    let (_, base) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &ann,
        Some(read.clone()),
    )
    .await;
    assert_eq!(base["cells"][0]["value"], "100");
    assert_eq!(base["cells"][0]["overlaid"], false);
    let (_, sb) = call_h(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &ann,
        Some(read.clone()),
        ("x-epiphany-sandbox", "wi"),
    )
    .await;
    assert_eq!(sb["cells"][0]["value"], "999");
    assert_eq!(sb["cells"][0]["overlaid"], true);

    // Commit merges into base; the sandbox stays but is now empty.
    let (s, c) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes/wi/commit",
        &ann,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["committed"], 1);
    let (_, after) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        &ann,
        Some(read),
    )
    .await;
    assert_eq!(after["cells"][0]["value"], "999");
    let (_, meta) = call(&app, "GET", "/api/v1/cubes/Sales/sandboxes/wi", &ann, None).await;
    assert_eq!(meta["cell_count"], 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn sandbox_rejects_string_what_if_but_allows_numeric() {
    // String what-if is out of scope this phase (the overlay is numeric, ADR-0014):
    // a string override into a sandbox is rejected loudly rather than staged and
    // silently committed; a numeric override in the same sandbox still works.
    let dir = scratch("string-reject");
    let cube = {
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        let mut measure = Dimension::new("Measure");
        measure.add_leaf("Sales");
        measure.add_string("Note");
        Cube::new("Sales", vec![region, measure]).unwrap()
    };
    let store = Store::create(dir.to_path_buf(), cube).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));
    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(SecurityStore::with_admin("ann", "pw", true))),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(CalcFactory::new(engine)),
        command_connectors_enabled: false,
    };
    let app = build_router(state);
    let ann = login(&app, "ann").await;

    // A base string write works.
    let note = |v: &str| json!({ "coord": { "Region": "North", "Measure": "Note" }, "value": v });
    let (s, _) = call(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &ann,
        Some(note("base")),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // A string what-if write into a sandbox is rejected (422); base is untouched.
    call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/sandboxes",
        &ann,
        Some(json!({ "name": "wi" })),
    )
    .await;
    let (s, _) = call_h(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &ann,
        Some(note("what-if")),
        ("x-epiphany-sandbox", "wi"),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // A numeric what-if in the same sandbox still works and is flagged overlaid.
    let (s, w) = call_h(
        &app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        &ann,
        Some(json!({ "coord": { "Region": "North", "Measure": "Sales" }, "value": "42" })),
        ("x-epiphany-sandbox", "wi"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{w}");
    assert_eq!(w["overlaid"], true);

    std::fs::remove_dir_all(&dir).ok();
}
