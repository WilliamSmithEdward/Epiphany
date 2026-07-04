//! Integration tests for the Phase 3G subset/view/cellset endpoints, driven
//! through the real router via tower oneshot under deterministic mode. The MDX
//! evaluator is injected (a dev-dependency) so dynamic subsets resolve.

use std::collections::BTreeMap;
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
use epiphany_security::{AccessLevel, AuditLog, ObjectKind, Scope, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const TTL: u64 = 60_000;

/// Region(North,South,Total) x Product(Widget,Gadget,All) x
/// Measure(Sales,Cost,Margin=Sales-Cost), with values set so North/Gadget is
/// all-zero and South/Gadget is partially zero.
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

fn router(name: &str) -> Router {
    router_cube(name, sample_cube())
}

/// A cube with two 1500-member dimensions plus a single measure: their crossjoin
/// (2,250,000 cells) exceeds `MAX_CELLSET_CELLS` (2,000,000), for the cellset cap.
fn oversized_cube() -> Cube {
    let mut rows = Dimension::new("Rows");
    let mut cols = Dimension::new("Cols");
    for i in 0..1500 {
        rows.add_leaf(format!("r{i}"));
        cols.add_leaf(format!("c{i}"));
    }
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new("Big", vec![rows, cols, measure]).unwrap()
}

fn router_cube(name: &str, cube: Cube) -> Router {
    let dir =
        std::env::temp_dir().join(format!("epiphany-api-query-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let cube_name = cube.name().to_string();
    let store = Store::create(dir, cube).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert(cube_name, store);
    let mut security = SecurityStore::with_admin("admin", "pw", true);
    security.create_user("bob", "pw", false).unwrap();
    // Subset-visibility tests use a non-admin who can read the cube (ADR-0023).
    security
        .set_grant(
            &Subject::User("bob".into()),
            Scope::Global,
            ObjectKind::Cube,
            AccessLevel::Write,
        )
        .unwrap();
    // A non-admin with NO cube grant, for authorization-ordering tests (a caller
    // without access must not distinguish a missing object from a forbidden one).
    security.create_user("carol", "pw", false).unwrap();
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(security)),
        sessions: Arc::new(Mutex::new(SessionStore::new(TTL))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        secure_cookies: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Default::default(),
        automation: Arc::new(Mutex::new(
            epiphany_persist::AutomationStore::open(
                std::env::temp_dir()
                    .join(format!("epiphany-test-auto-{}-query-0", std::process::id())),
            )
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

#[tokio::test]
async fn new_routes_require_authentication() {
    let app = router("auth");
    let resp = app
        .oneshot(
            Request::post("/api/v1/cubes/Sales/dimensions/Region/subsets")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": "X", "kind": "static" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn static_subset_crud_and_members() {
    let app = router("subset-crud");
    let t = login(&app, "admin").await;
    let base = "/api/v1/cubes/Sales/dimensions/Region/subsets";

    let (status, created) = call(
        &app,
        "POST",
        base,
        &t,
        Some(json!({ "name": "Core", "kind": "static", "members": ["North", "South"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["owner"], "admin");
    assert_eq!(created["visibility"], "public");

    let (_, list) = call(&app, "GET", base, &t, None).await;
    assert_eq!(list["subsets"].as_array().unwrap().len(), 1);

    let (status, members) = call(&app, "GET", &format!("{base}/Core/members"), &t, None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = members["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["North", "South"]);

    // Duplicate name -> 409.
    let (status, dup) = call(
        &app,
        "POST",
        base,
        &t,
        Some(json!({ "name": "Core", "kind": "static", "members": ["North"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(dup["error"]["code"], "DUPLICATE_NAME");

    let (status, _) = call(&app, "DELETE", &format!("{base}/Core"), &t, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, "GET", &format!("{base}/Core"), &t, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dynamic_subset_and_mdx_preview() {
    let app = router("mdx");
    let t = login(&app, "admin").await;
    let base = "/api/v1/cubes/Sales/dimensions/Region";

    // MDX preview resolves a set without saving.
    let (status, preview) = call(
        &app,
        "POST",
        &format!("{base}/mdx/preview"),
        &t,
        Some(json!({ "mdx": "[Region].[Total].Children" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = preview["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["North", "South"]);

    // A saved dynamic subset resolves the same way.
    let (status, _) = call(
        &app,
        "POST",
        &format!("{base}/subsets"),
        &t,
        Some(json!({ "name": "Leaves", "kind": "dynamic", "mdx": "[Region].[Total].Children" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, members) = call(
        &app,
        "GET",
        &format!("{base}/subsets/Leaves/members"),
        &t,
        None,
    )
    .await;
    assert_eq!(members["members"].as_array().unwrap().len(), 2);

    // Bad MDX -> 422 MDX_ERROR.
    let (status, err) = call(
        &app,
        "POST",
        &format!("{base}/mdx/preview"),
        &t,
        Some(json!({ "mdx": "{[Region].[North]" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err["error"]["code"], "MDX_ERROR");
}

#[tokio::test]
async fn execute_nested_view_to_cellset() {
    let app = router("view");
    let t = login(&app, "admin").await;

    call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/dimensions/Region/subsets",
        &t,
        Some(json!({ "name": "Core", "kind": "static", "members": ["North", "South"] })),
    )
    .await;

    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/views",
        &t,
        Some(json!({
            "name": "Grid",
            "rows": [
                { "dimension": "Region", "type": "subset", "subset": "Core" },
                { "dimension": "Product", "type": "members", "members": ["Widget", "Gadget"] }
            ],
            "columns": [
                { "dimension": "Measure", "type": "members", "members": ["Sales", "Cost", "Margin"] }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, cs) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/views/Grid/execute",
        &t,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Row tuples nest Region (outer) over Product.
    assert_eq!(cs["row_dimensions"], json!(["Region", "Product"]));
    let first_row: Vec<&str> = cs["row_tuples"][0]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(first_row, vec!["North", "Widget"]);
    // (North,Widget): Sales=100 (editable leaf), Margin=40 (consolidated, read-only).
    assert_eq!(cs["cells"][0]["value"], "100");
    assert_eq!(cs["cells"][0]["editable"], true);
    assert_eq!(cs["cells"][2]["value"], "40");
    assert_eq!(cs["cells"][2]["editable"], false);
    assert!(cs["version"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn ad_hoc_cellset_with_zero_row_suppression() {
    let app = router("suppress");
    let t = login(&app, "admin").await;

    // suppress_zero_rows only: the all-zero North/Gadget row drops; columns are
    // untouched (no column is all-zero across the kept rows here anyway).
    let spec = json!({
        "suppress_zero_rows": true,
        "rows": [
            { "dimension": "Region", "type": "members", "members": ["North", "South"] },
            { "dimension": "Product", "type": "members", "members": ["Widget", "Gadget"] }
        ],
        "columns": [
            { "dimension": "Measure", "type": "members", "members": ["Sales", "Cost", "Margin"] }
        ]
    });
    let (status, cs) = call(&app, "POST", "/api/v1/cubes/Sales/cellset", &t, Some(spec)).await;
    assert_eq!(status, StatusCode::OK);

    // North/Gadget is all zero -> suppressed; the other three rows remain.
    let rows: Vec<Vec<&str>> = cs["row_tuples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            t.as_array()
                .unwrap()
                .iter()
                .map(|m| m["name"].as_str().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            vec!["North", "Widget"],
            vec!["South", "Widget"],
            vec!["South", "Gadget"],
        ]
    );
    assert_eq!(cs["suppressed"]["row_tuples"], 1);
    assert_eq!(cs["suppressed"]["column_tuples"], 0);
}

#[tokio::test]
async fn ad_hoc_cellset_with_zero_column_suppression() {
    let app = router("suppress-cols");
    let t = login(&app, "admin").await;

    // North on rows x {Widget, Gadget} on columns, sliced to Sales: North/Widget
    // = 100, North/Gadget = 0. The Gadget COLUMN is all-zero. suppress_zero_columns
    // drops it; the row stays (it is partially zero, not all-zero) and would not
    // be touched by the column flag in any case.
    let spec = json!({
        "suppress_zero_columns": true,
        "rows": [
            { "dimension": "Region", "type": "members", "members": ["North"] }
        ],
        "columns": [
            { "dimension": "Product", "type": "members", "members": ["Widget", "Gadget"] }
        ],
        "context": [ { "dimension": "Measure", "member": "Sales" } ]
    });
    let (status, cs) = call(&app, "POST", "/api/v1/cubes/Sales/cellset", &t, Some(spec)).await;
    assert_eq!(status, StatusCode::OK);

    let cols: Vec<Vec<&str>> = cs["column_tuples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            t.as_array()
                .unwrap()
                .iter()
                .map(|m| m["name"].as_str().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(cols, vec![vec!["Widget"]]);
    assert_eq!(cs["suppressed"]["column_tuples"], 1);
    assert_eq!(cs["suppressed"]["row_tuples"], 0);
}

#[tokio::test]
async fn ad_hoc_cellset_legacy_suppress_zeros_still_accepted() {
    let app = router("suppress-legacy");
    let t = login(&app, "admin").await;

    // Input back-compat: the legacy single `suppress_zeros: true` flag must drive
    // BOTH axes, reproducing the pre-split row-suppression behavior.
    let spec = json!({
        "suppress_zeros": true,
        "rows": [
            { "dimension": "Region", "type": "members", "members": ["North", "South"] },
            { "dimension": "Product", "type": "members", "members": ["Widget", "Gadget"] }
        ],
        "columns": [
            { "dimension": "Measure", "type": "members", "members": ["Sales", "Cost", "Margin"] }
        ]
    });
    let (status, cs) = call(&app, "POST", "/api/v1/cubes/Sales/cellset", &t, Some(spec)).await;
    assert_eq!(status, StatusCode::OK);
    // North/Gadget is the only all-zero row; it drops as before.
    assert_eq!(cs["suppressed"]["row_tuples"], 1);
}

#[tokio::test]
async fn error_mapping_table() {
    let app = router("errors");
    let t = login(&app, "admin").await;

    // Unknown dimension.
    let (status, e) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/dimensions/Nope/subsets",
        &t,
        Some(json!({ "name": "X", "kind": "static", "members": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e["error"]["code"], "UNKNOWN_DIMENSION");

    // A view with a dimension on two axes -> DIMENSION_COVERAGE.
    let (status, e) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cellset",
        &t,
        Some(json!({
            "rows": [{ "dimension": "Region", "type": "members", "members": ["North"] }],
            "columns": [{ "dimension": "Region", "type": "members", "members": ["South"] }],
            "context": [
                { "dimension": "Product", "member": "Widget" },
                { "dimension": "Measure", "member": "Sales" }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e["error"]["code"], "DIMENSION_COVERAGE");

    // Executing a missing view -> 404 UNKNOWN_VIEW.
    let (status, e) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/views/Ghost/execute",
        &t,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(e["error"]["code"], "UNKNOWN_VIEW");
}

#[tokio::test]
async fn private_subset_is_hidden_from_non_owner() {
    let app = router("visibility");
    let admin = login(&app, "admin").await;
    let base = "/api/v1/cubes/Sales/dimensions/Region/subsets";

    let (status, _) = call(
        &app,
        "POST",
        base,
        &admin,
        Some(json!({ "name": "Secret", "kind": "static", "members": ["North"], "visibility": "private" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Bob (non-admin, non-owner) cannot see it.
    let bob = login(&app, "bob").await;
    let (status, _) = call(&app, "GET", &format!("{base}/Secret"), &bob, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = call(&app, "GET", base, &bob, None).await;
    assert!(list["subsets"].as_array().unwrap().is_empty());

    // The owner still sees it.
    let (status, _) = call(&app, "GET", &format!("{base}/Secret"), &admin, None).await;
    assert_eq!(status, StatusCode::OK);
}

/// A caller with NO cube access must get 403 from the mutating subset/view routes
/// whether or not the object exists, so a 404-vs-403 distinction cannot enumerate
/// cube/object names. (Regression: these handlers used to probe existence first.)
#[tokio::test]
async fn mutating_routes_authorize_before_existence_probe() {
    let app = router("authz-order");
    let carol = login(&app, "carol").await; // non-admin, no cube grant

    // PUT/DELETE a nonexistent subset -> 403 (not 404), same as an existing one.
    let sub = "/api/v1/cubes/Sales/dimensions/Region/subsets/Nope";
    let (status, _) = call(
        &app,
        "PUT",
        sub,
        &carol,
        Some(json!({ "kind": "static", "members": ["North"] })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, "DELETE", sub, &carol, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // PUT/DELETE a nonexistent view -> 403 (not 404).
    let view = "/api/v1/cubes/Sales/views/Nope";
    let (status, _) = call(
        &app,
        "PUT",
        view,
        &carol,
        Some(json!({ "rows": [], "columns": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, "DELETE", view, &carol, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// A non-owner with cube Write must get a uniform 404 (not a 403 "you do not own
/// this object") when replacing/deleting another user's PRIVATE subset, so the
/// private object's existence is not revealed. (Regression: the ownership check
/// used to run against an unfiltered lookup, leaking existence via 403.)
#[tokio::test]
async fn mutating_a_foreign_private_subset_404s_not_403() {
    let app = router("authz-private");
    let admin = login(&app, "admin").await;
    let base = "/api/v1/cubes/Sales/dimensions/Region/subsets";
    let (status, _) = call(
        &app,
        "POST",
        base,
        &admin,
        Some(json!({ "name": "Secret", "kind": "static", "members": ["North"], "visibility": "private" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Bob has global Cube:Write but is not the owner and cannot see the private
    // subset: PUT and DELETE both 404 (existence stays hidden), never 403.
    let bob = login(&app, "bob").await;
    let target = format!("{base}/Secret");
    let (status, _) = call(
        &app,
        "PUT",
        &target,
        &bob,
        Some(json!({ "kind": "static", "members": ["South"] })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, "DELETE", &target, &bob, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// An ad-hoc view axis referencing another user's PRIVATE subset must not resolve
/// (the private membership must not leak through the cellset's tuples): the
/// execute path now applies the same visibility filter as the direct subset
/// endpoints, so the reference reads as UnknownSubset (404). (Regression: execute
/// passed an unfiltered subset lookup.)
#[tokio::test]
async fn adhoc_view_cannot_resolve_a_foreign_private_subset() {
    let app = router("authz-subset-exec");
    let admin = login(&app, "admin").await;
    let base = "/api/v1/cubes/Sales/dimensions/Region/subsets";
    let (status, _) = call(
        &app,
        "POST",
        base,
        &admin,
        Some(json!({ "name": "Secret", "kind": "static", "members": ["North"], "visibility": "private" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Bob (cube Write, non-owner) references the private subset on an ad-hoc axis.
    let bob = login(&app, "bob").await;
    let spec = json!({
        "rows": [ { "dimension": "Region", "type": "subset", "subset": "Secret" } ],
        "columns": [ { "dimension": "Measure", "type": "members", "members": ["Sales"] } ],
        "context": [ { "dimension": "Product", "member": "Widget" } ]
    });
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cellset",
        &bob,
        Some(spec),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "UNKNOWN_SUBSET");

    // The owner can still resolve it.
    let spec = json!({
        "rows": [ { "dimension": "Region", "type": "subset", "subset": "Secret" } ],
        "columns": [ { "dimension": "Measure", "type": "members", "members": ["Sales"] } ],
        "context": [ { "dimension": "Product", "member": "Widget" } ]
    });
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/cubes/Sales/cellset",
        &admin,
        Some(spec),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// An ad-hoc cellset whose axis crossjoin exceeds `MAX_CELLSET_CELLS` is refused
/// with a clean 422 `CELLSET_TOO_LARGE` (before any dense grid is allocated), so a
/// reader cannot OOM the server with a large-crossjoin view. (Regression: this
/// mapped to the generic MODEL_ERROR.)
#[tokio::test]
async fn oversized_cellset_is_refused_with_a_clean_422() {
    let app = router_cube("oversized", oversized_cube());
    let admin = login(&app, "admin").await;
    // Rows (1500) x Cols (1500) = 2,250,000 tuples > the 2,000,000 cell cap.
    let spec = json!({
        "rows": [ { "dimension": "Rows", "type": "members",
                    "members": (0..1500).map(|i| format!("r{i}")).collect::<Vec<_>>() } ],
        "columns": [ { "dimension": "Cols", "type": "members",
                       "members": (0..1500).map(|i| format!("c{i}")).collect::<Vec<_>>() } ],
        "context": [ { "dimension": "Measure", "member": "Sales" } ]
    });
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/cubes/Big/cellset",
        &admin,
        Some(spec),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CELLSET_TOO_LARGE");
}
