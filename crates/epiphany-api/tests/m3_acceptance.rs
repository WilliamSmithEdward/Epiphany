//! M3 acceptance suite: "subsets, views, and MDX" (end of Phase 3).
//!
//! Proves the Phase 3 definition of done end to end over the REAL router, under
//! deterministic mode (fixed admin, ManualClock, seeded IdGen, tempdir Store,
//! injected MdxEvaluator): a user defines an MDX-backed dynamic subset, builds a
//! nested view referencing it with zero-suppression on, executes it to a
//! cellset, and the exact tuples/values survive a server restart. A preview leg
//! proves the same shape is reachable ad hoc without saving.
//!
//! This is the binding, non-flaky CI gate for M3.

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
    let dir = std::env::temp_dir().join(format!("epiphany-m3-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

/// Region(North,South,Total) x Product(Widget,Gadget,All) x
/// Measure(Sales,Cost,Margin), pre-populated so North/Gadget is all-zero and
/// South/Gadget is partially zero.
fn sample_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let north = region.add_leaf("North");
    let south = region.add_leaf("South");
    let r_total = region.add_consolidated("Total");
    region.add_child(r_total, north, 1).unwrap();
    region.add_child(r_total, south, 1).unwrap();

    let mut product = Dimension::new("Product");
    let widget = product.add_leaf("Widget");
    let gadget = product.add_leaf("Gadget");
    let p_all = product.add_consolidated("All");
    product.add_child(p_all, widget, 1).unwrap();
    product.add_child(p_all, gadget, 1).unwrap();

    let mut measure = Dimension::new("Measure");
    let sales = measure.add_leaf("Sales");
    let cost = measure.add_leaf("Cost");
    let margin = measure.add_consolidated("Margin");
    measure.add_child(margin, sales, 1).unwrap();
    measure.add_child(margin, cost, -1).unwrap();

    let mut cube = Cube::new("Sales", vec![region, product, measure]).unwrap();
    let mut set = |r, p, m, v: i32| cube.set_leaf(&[r, p, m], Fixed::from(v)).unwrap();
    set(north, widget, sales, 100);
    set(north, widget, cost, 60);
    set(south, widget, sales, 200);
    set(south, widget, cost, 150);
    set(south, gadget, sales, 50);
    set(south, gadget, cost, 50);
    cube
}

/// Build the router over a Store at `dir/cubes/Sales` (created if absent, else
/// reopened), with a fixed admin and the real MDX evaluator injected.
fn router_for(dir: &Path) -> Router {
    let sales_dir = dir.join("cubes").join("Sales");
    let store = if sales_dir.join("snapshot.model").is_file() {
        Store::open(sales_dir.clone()).unwrap()
    } else {
        Store::create(sales_dir.clone(), sample_cube()).unwrap()
    };
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
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

/// The view spec used throughout: two dimensions on rows (Region outer, Product
/// via the dynamic subset), Measure on columns, zero-suppression on.
fn plan_spec() -> Value {
    json!({
        "name": "Plan",
        "suppress_zeros": true,
        "rows": [
            { "dimension": "Region", "type": "members", "members": ["North", "South"] },
            { "dimension": "Product", "type": "subset", "subset": "Items" }
        ],
        "columns": [
            { "dimension": "Measure", "type": "members", "members": ["Sales", "Cost", "Margin"] }
        ]
    })
}

/// Reduce a cellset to (row tuples as names, cell values) for comparison.
fn summary(cs: &Value) -> (Vec<Vec<String>>, Vec<String>) {
    let rows = cs["row_tuples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            t.as_array()
                .unwrap()
                .iter()
                .map(|m| m["name"].as_str().unwrap().to_string())
                .collect()
        })
        .collect();
    let cells = cs["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["value"].as_str().unwrap().to_string())
        .collect();
    (rows, cells)
}

#[tokio::test]
async fn m3_definition_of_done() {
    let dir = scratch("dod");

    let expected_rows = vec![
        vec!["North".to_string(), "Widget".to_string()],
        vec!["South".to_string(), "Widget".to_string()],
        vec!["South".to_string(), "Gadget".to_string()],
    ];
    // North/Widget 100,60,40 ; South/Widget 200,150,50 ; South/Gadget 50,50,0.
    let expected_cells: Vec<String> = ["100", "60", "40", "200", "150", "50", "50", "50", "0"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // --- Session 1: define, build, execute, preview ---
    {
        let app = router_for(&dir);
        let token = login(&app).await;

        // 1. Define an MDX-backed dynamic subset over Product.
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/dimensions/Product/subsets",
            &token,
            Some(json!({ "name": "Items", "kind": "dynamic", "mdx": "[Product].[All].Children" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // 2. Build a nested view referencing it, with zero-suppression on.
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/views",
            &token,
            Some(plan_spec()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // 3. Execute it to a cellset.
        let (status, cs) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/views/Plan/execute",
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // 4. Exact axis tuples (nesting cross-product, MDX order), the exact value
        // matrix, and an all-empty tuple absent (reported) while a partial stays.
        let (rows, cells) = summary(&cs);
        assert_eq!(
            rows, expected_rows,
            "nested tuples in Region-major MDX order"
        );
        assert_eq!(cells, expected_cells, "decimal-string value matrix");
        assert_eq!(
            cs["suppressed"]["row_tuples"], 1,
            "North/Gadget was suppressed"
        );
        assert_eq!(cs["row_dimensions"], json!(["Region", "Product"]));

        // 6. Preview leg: the same shape ad hoc, without saving.
        let (status, adhoc) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/cellset",
            &token,
            Some(plan_spec()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            summary(&adhoc),
            (expected_rows.clone(), expected_cells.clone())
        );
    }

    // --- Session 2: restart over the same data directory ---
    {
        let app = router_for(&dir);
        let token = login(&app).await;

        // 5. The persisted subset and view re-execute to an identical cellset.
        let (status, cs) = call(
            &app,
            "POST",
            "/api/v1/cubes/Sales/views/Plan/execute",
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the view persisted across restart");
        assert_eq!(
            summary(&cs),
            (expected_rows, expected_cells),
            "the dynamic subset and view produce an identical cellset after restart"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
