//! Acceptance suite for `POST /cubes/{cube}/mdx`: parse + lower + execute a raw
//! MDX `SELECT` query to a cellset over the REAL router.
//!
//! The endpoint shipped tested only against the app's own generated dialect
//! (explicit member-list sets on COLUMNS/ROWS, CrossJoin, a WHERE slicer). This
//! suite closes the gap for HAND-WRITTEN MDX: it asserts REAL cell values (not
//! just status codes) for the generated dialect, and then exercises every
//! hand-written set form the parser accepts (`.Members`, `.Children`,
//! `Descendants(...)`, `Filter(...)`, `Order(...)`, a 3-way CrossJoin) end to end
//! through `view_from_mdx` -> `execute_view`. Each form is pinned to its ACTUAL
//! behavior: a form that executes correctly asserts its exact tuples/values; a
//! form with a lowering/execution gap asserts the current (clean 4xx) behavior so
//! a future fix is caught by a failing test. The error legs prove cube-name
//! mismatch (422), invalid MDX (4xx), and a no-access caller (403) -- and that
//! attacker-controlled MDX never panics and never leaks internal error text.
//!
//! Determinism (ADR-0009): pinned `ManualClock`, seeded `IdGen`, injected
//! `MdxEvaluator`; no wall clock / RNG. Dependency-free: hand-authored asserts.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, SessionStore};
use epiphany_core::{AttributeKind, AttributeValue, Cube, Dimension, Fixed};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::Engine;
use epiphany_mdx::MdxEvaluator;
use epiphany_persist::Store;
use epiphany_security::{AccessLevel, AuditLog, ObjectKind, Scope, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Region(North,South,East leaves; Total=N+S+E) x Product(Widget,Gadget; All=W+G)
/// x Measure(Sales,Cost; Margin=Sales-Cost) x Period(Q1,Q2).
///
/// Region carries a numeric `Pop` attribute (North 300, South 100, East 200) so
/// `Filter`/`Order` over an attribute can be exercised. All values are seeded at
/// Period Q1 (Q2 is all zero), so every test pins Period in the WHERE slicer (or
/// on an axis) and the value matrix is fully determined. East has no Product data,
/// exercising a real zero cell. The 4th dimension lets a 3-way CrossJoin on one
/// axis still leave one dimension for the other axis (no forced-empty axis).
fn sample_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let north = region.add_leaf("North");
    let south = region.add_leaf("South");
    let east = region.add_leaf("East");
    let r_total = region.add_consolidated("Total");
    region.add_child(r_total, north, 1).unwrap();
    region.add_child(r_total, south, 1).unwrap();
    region.add_child(r_total, east, 1).unwrap();
    region.add_attribute("Pop", AttributeKind::Numeric);
    region
        .set_attribute(north, "Pop", AttributeValue::Numeric(Fixed::from(300)))
        .unwrap();
    region
        .set_attribute(south, "Pop", AttributeValue::Numeric(Fixed::from(100)))
        .unwrap();
    region
        .set_attribute(east, "Pop", AttributeValue::Numeric(Fixed::from(200)))
        .unwrap();

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

    let mut period = Dimension::new("Period");
    let q1 = period.add_leaf("Q1");
    period.add_leaf("Q2");

    let mut cube = Cube::new("Sales", vec![region, product, measure, period]).unwrap();
    // All data lives at Q1; Q2 is zero.
    let mut set = |r, p, m, v: i32| cube.set_leaf(&[r, p, m, q1], Fixed::from(v)).unwrap();
    // North: Widget(Sales 100, Cost 60), Gadget(Sales 10, Cost 5).
    set(north, widget, sales, 100);
    set(north, widget, cost, 60);
    set(north, gadget, sales, 10);
    set(north, gadget, cost, 5);
    // South: Widget(Sales 200, Cost 150), Gadget(Sales 50, Cost 50).
    set(south, widget, sales, 200);
    set(south, widget, cost, 150);
    set(south, gadget, sales, 50);
    set(south, gadget, cost, 50);
    // East: no data (all zero), so a query touching East proves real zero cells.
    cube
}

/// Build a router over a fresh in-memory-ish Store with a fixed admin plus a
/// non-admin `nobody` user who is granted NO cube access (for the 403 leg).
fn router() -> Router {
    // A per-process counter gives each router a distinct temp dir (deterministic:
    // a fixed sequence within a run, never the wall clock or RNG).
    let dir = std::env::temp_dir().join(format!(
        "epiphany-mdx-query-{}-{}",
        std::process::id(),
        next_dir_id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, sample_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    // `nobody` exists and can authenticate, but holds no grant on the Sales cube,
    // so `require_cube_access(Read)` must reject the query (fail-closed).
    sec.create_user("nobody", "pw", false).unwrap();
    // `reader` is a non-admin who DOES hold cube Read, proving a normal user can
    // run a hand-written query (the gate is access, not admin-only).
    sec.create_user("reader", "pw", false).unwrap();
    sec.set_grant(
        &Subject::User("reader".into()),
        Scope::Global,
        ObjectKind::Cube,
        AccessLevel::Read,
    )
    .unwrap();

    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(50, 900_000))),
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
            epiphany_persist::AutomationStore::open(std::env::temp_dir().join(format!(
                "epiphany-test-auto-{}-mdx_query-{}",
                std::process::id(),
                next_dir_id()
            )))
            .unwrap(),
        )),
        http: Default::default(),
        sql: Default::default(),
    };
    build_router(state)
}

/// A per-process counter so each router gets a unique temp dir (deterministic
/// within a run: the sequence is fixed, never the wall clock).
fn next_dir_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
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

/// POST an MDX query for `cube` as `token`, returning (status, json body).
async fn mdx(app: &Router, cube: &str, token: &str, query: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/cubes/{cube}/mdx"))
        .header("content-type", "application/json");
    if !token.is_empty() {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = json!({ "mdx": query }).to_string();
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// Reduce a cellset to (row tuples as names, column tuples as names, cell
/// values) for exact comparison.
fn summary(cs: &Value) -> (Vec<Vec<String>>, Vec<Vec<String>>, Vec<String>) {
    let tuples = |key: &str| -> Vec<Vec<String>> {
        cs[key]
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
            .collect()
    };
    let cells = cs["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["value"].as_str().unwrap().to_string())
        .collect();
    (tuples("row_tuples"), tuples("column_tuples"), cells)
}

fn row_names(cs: &Value) -> Vec<Vec<String>> {
    summary(cs).0
}

// ------------------------------------------------------------------
// (a) Generated dialect: explicit member-list sets on COLUMNS and ROWS.
// ------------------------------------------------------------------

#[tokio::test]
async fn generated_dialect_member_lists_on_columns_and_rows() {
    let app = router();
    let token = login(&app, "admin").await;
    // Region on COLUMNS, Measure on ROWS, Product pinned in the WHERE slicer.
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Region].[North], [Region].[South] } ON COLUMNS, \
         { [Measure].[Sales], [Measure].[Cost] } ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");

    let (rows, cols, cells) = summary(&cs);
    assert_eq!(cols, vec![vec!["North"], vec!["South"]]);
    assert_eq!(rows, vec![vec!["Sales"], vec!["Cost"]]);
    // Widget/Q1: North(Sales 100, Cost 60), South(Sales 200, Cost 150). Row-major,
    // columns fastest: [N/Sales, S/Sales, N/Cost, S/Cost].
    assert_eq!(cells, vec!["100", "200", "60", "150"]);
    assert_eq!(cs["context"][0]["dimension"], "Product");
    assert_eq!(cs["context"][0]["member"], "Widget");
}

// ------------------------------------------------------------------
// (b) Hand-written `.Members` axis.
// ------------------------------------------------------------------

#[tokio::test]
async fn handwritten_members_axis_enumerates_the_dimension() {
    let app = router();
    let token = login(&app, "admin").await;
    // Region.Members on ROWS enumerates ALL region members in definition order:
    // North, South, East, Total. Measure.[Sales] on COLUMNS; Product pinned.
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, [Region].Members ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");

    let rows = row_names(&cs);
    assert_eq!(
        rows,
        vec![vec!["North"], vec!["South"], vec!["East"], vec!["Total"],],
        ".Members is definition order, consolidations included"
    );
    let (_, _, cells) = summary(&cs);
    // Widget/Sales/Q1: North 100, South 200, East 0, Total 300.
    assert_eq!(cells, vec!["100", "200", "0", "300"]);
}

// ------------------------------------------------------------------
// (c) WHERE / slicer clause moves a dimension to the context.
// ------------------------------------------------------------------

#[tokio::test]
async fn where_slicer_sets_the_context_and_filters_values() {
    let app = router();
    let token = login(&app, "admin").await;
    // Same axes, but slice on Cost instead of Sales by moving Measure to WHERE.
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Region].[North], [Region].[South] } ON COLUMNS, \
         { [Product].[Widget], [Product].[Gadget] } ON ROWS \
         FROM [Sales] WHERE ( [Measure].[Cost], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");

    let (rows, cols, cells) = summary(&cs);
    assert_eq!(cols, vec![vec!["North"], vec!["South"]]);
    assert_eq!(rows, vec![vec!["Widget"], vec!["Gadget"]]);
    // Cost/Q1: N/Widget 60, S/Widget 150, N/Gadget 5, S/Gadget 50.
    assert_eq!(cells, vec!["60", "150", "5", "50"]);
    assert_eq!(cs["context"][0]["dimension"], "Measure");
    assert_eq!(cs["context"][0]["member"], "Cost");
}

// ------------------------------------------------------------------
// (d) Zero-suppression: verify whether the endpoint can express it.
//
// `view_from_mdx` hard-codes `suppress_zeros: false` and the MDX grammar has no
// NON EMPTY / suppression syntax, so this endpoint CANNOT request suppression.
// We assert the documented behavior: a zero row is RETURNED (value "0"), never
// dropped, and `suppressed.row_tuples` is 0.
// ------------------------------------------------------------------

#[tokio::test]
async fn zero_suppression_is_not_expressible_zero_rows_are_returned() {
    let app = router();
    let token = login(&app, "admin").await;
    // East has no data, so East/Sales is a real zero row. With no suppression it
    // must appear with value "0" and nothing must be reported as suppressed.
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, \
         { [Region].[North], [Region].[East] } ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");

    let (rows, _, cells) = summary(&cs);
    assert_eq!(
        rows,
        vec![vec!["North"], vec!["East"]],
        "the all-zero East row is NOT suppressed by this endpoint"
    );
    assert_eq!(cells, vec!["100", "0"]);
    assert_eq!(
        cs["suppressed"]["row_tuples"], 0,
        "the endpoint cannot express zero-suppression, so nothing is suppressed"
    );
    assert_eq!(cs["suppressed"]["column_tuples"], 0);
}

// ------------------------------------------------------------------
// (e) Cube-name mismatch between the URL and the MDX FROM -> 422.
// ------------------------------------------------------------------

#[tokio::test]
async fn cube_name_mismatch_is_422() {
    let app = router();
    let token = login(&app, "admin").await;
    let (status, body) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, { [Region].[North] } ON ROWS \
         FROM [OtherCube] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "MDX_CUBE_MISMATCH");
}

// ------------------------------------------------------------------
// (f) Syntactically invalid MDX -> 4xx (a clean parse error, no panic/leak).
// ------------------------------------------------------------------

#[tokio::test]
async fn invalid_mdx_is_a_clean_4xx() {
    let app = router();
    let token = login(&app, "admin").await;

    // Garbage that is not a SELECT at all.
    let (status, body) = mdx(&app, "Sales", &token, "this is not mdx").await;
    assert!(status.is_client_error(), "got {status}, body {body}");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "MDX_PARSE_ERROR");

    // A truncated SELECT (missing FROM).
    let (status, _) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Region].[North] } ON COLUMNS",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Unbalanced braces.
    let (status, _) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Region].[North] ON COLUMNS, {} ON ROWS FROM [Sales]",
    )
    .await;
    assert!(status.is_client_error());
}

/// Attacker-controlled MDX must never panic and never leak internal error text
/// (RG-12). A spread of malformed / hostile inputs must each yield a clean 4xx
/// with a stable error code and no Rust-internal phrasing in the message.
#[tokio::test]
async fn hostile_mdx_never_panics_or_leaks() {
    let app = router();
    let token = login(&app, "admin").await;

    let hostile = [
        "",                                                                                  // empty
        "SELECT",                                               // bare keyword
        "SELECT FROM [Sales]",                                  // no axes
        "SELECT {} ON COLUMNS FROM",                            // dangling FROM
        "SELECT {} ON COLUMNS, {} ON ROWS FROM [Sales] DROP",   // trailing junk
        "SELECT [Region].[North'; --] ON COLUMNS FROM [Sales]", // sql-ish noise
        &"{".repeat(5000),                                      // deep nesting
        "SELECT { [Region].[North] } ON COLUMNS, { [Region].[South] } ON ROWS FROM [Sales]", // Region twice -> dimension coverage
    ];
    for src in hostile {
        let (status, body) = mdx(&app, "Sales", &token, src).await;
        assert!(
            status.is_client_error(),
            "input {src:?} should be a clean 4xx, got {status} body {body}"
        );
        // The body is our structured ApiError, never a raw panic string.
        let msg = body["error"]["message"].as_str().unwrap_or("");
        for leak in ["panicked", "RUST_BACKTRACE", "unwrap", "src\\", "src/"] {
            assert!(
                !msg.contains(leak),
                "error message for {src:?} leaks internal detail: {msg}"
            );
        }
    }
}

// ------------------------------------------------------------------
// (g) A caller without cube access -> 403 (authenticated, not permitted);
//     no token at all -> 401.
// ------------------------------------------------------------------

#[tokio::test]
async fn no_cube_access_is_403_and_no_token_is_401() {
    let app = router();

    // Authenticated but ungranted user: fail-closed 403.
    let nobody = login(&app, "nobody").await;
    let (status, _) = mdx(
        &app,
        "Sales",
        &nobody,
        "SELECT { [Measure].[Sales] } ON COLUMNS, { [Region].[North] } ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "ungranted user must be 403");

    // No Authorization header at all: 401.
    let (status, _) = mdx(
        &app,
        "Sales",
        "",
        "SELECT { [Measure].[Sales] } ON COLUMNS, { [Region].[North] } ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "missing token must be 401"
    );
}

/// A normal (non-admin) user holding only cube Read can run a hand-written query
/// -- the gate is access, not admin-only.
#[tokio::test]
async fn non_admin_reader_can_run_a_query() {
    let app = router();
    let token = login(&app, "reader").await;
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, { [Region].[North] } ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");
    let (_, _, cells) = summary(&cs);
    assert_eq!(cells, vec!["100"]);
}

// ==================================================================
// Task 3: every hand-written set form the parser ACCEPTS, executed end to end.
// Each test pins the ACTUAL behavior (works -> exact values; gap -> clean 4xx).
// ==================================================================

/// `.Children` on a qualified member: `[Region].[Total].Children` -> the three
/// leaves, in edge order. WORKS end to end.
#[tokio::test]
async fn handwritten_children_axis_works() {
    let app = router();
    let token = login(&app, "admin").await;
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, [Region].[Total].Children ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");
    let rows = row_names(&cs);
    assert_eq!(rows, vec![vec!["North"], vec!["South"], vec!["East"]]);
    let (_, _, cells) = summary(&cs);
    // Widget/Sales: North 100, South 200, East 0.
    assert_eq!(cells, vec!["100", "200", "0"]);
}

/// `Descendants(...)` (function form) on a qualified member -> pre-order DFS
/// (self first). WORKS end to end.
#[tokio::test]
async fn handwritten_descendants_function_works() {
    let app = router();
    let token = login(&app, "admin").await;
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, Descendants([Region].[Total]) ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");
    let rows = row_names(&cs);
    assert_eq!(
        rows,
        vec![vec!["Total"], vec!["North"], vec!["South"], vec!["East"],],
        "Descendants is self-first pre-order DFS"
    );
    let (_, _, cells) = summary(&cs);
    // Widget/Sales: Total 300, North 100, South 200, East 0.
    assert_eq!(cells, vec!["300", "100", "200", "0"]);
}

/// `.Descendants` (postfix form) is equivalent to the function form. WORKS.
#[tokio::test]
async fn handwritten_descendants_postfix_works() {
    let app = router();
    let token = login(&app, "admin").await;
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, [Region].[Total].Descendants ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");
    let rows = row_names(&cs);
    assert_eq!(
        rows,
        vec![vec!["Total"], vec!["North"], vec!["South"], vec!["East"]]
    );
}

/// `Filter(set, predicate)` over a numeric attribute. WORKS end to end:
/// `Filter([Region].Members, Properties("Pop") >= 200)` keeps North(300) and
/// East(200), dropping South(100) and the attribute-less Total.
#[tokio::test]
async fn handwritten_filter_on_attribute_works() {
    let app = router();
    let token = login(&app, "admin").await;
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, \
         Filter([Region].Members, Properties(\"Pop\") >= 200) ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");
    let rows = row_names(&cs);
    assert_eq!(
        rows,
        vec![vec!["North"], vec!["East"]],
        "Filter keeps Pop>=200 (North 300, East 200), drops South and Total"
    );
    let (_, _, cells) = summary(&cs);
    // Widget/Sales: North 100, East 0.
    assert_eq!(cells, vec!["100", "0"]);
}

/// `Order(set, "Attr", DESC)` over a numeric attribute. WORKS end to end:
/// Order the three leaves by Pop descending -> North(300), East(200), South(100).
#[tokio::test]
async fn handwritten_order_by_attribute_works() {
    let app = router();
    let token = login(&app, "admin").await;
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, \
         Order([Region].[Total].Children, \"Pop\", DESC) ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");
    let rows = row_names(&cs);
    assert_eq!(
        rows,
        vec![vec!["North"], vec!["East"], vec!["South"]],
        "Order by Pop DESC: North 300, East 200, South 100"
    );
    let (_, _, cells) = summary(&cs);
    // Widget/Sales: North 100, East 0, South 200.
    assert_eq!(cells, vec!["100", "0", "200"]);
}

/// A 3-way `CrossJoin` (a flat 3-arg call, the dialect the pivot view emits for
/// 3+ nested dimensions) lowers to three per-dimension component sets in order,
/// preserving the crossjoin component order (first = outermost). WORKS end to
/// end. Region x Product x Measure on COLUMNS, Period on ROWS.
#[tokio::test]
async fn handwritten_three_way_crossjoin_works() {
    let app = router();
    let token = login(&app, "admin").await;
    let (status, cs) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT CrossJoin({ [Region].[North], [Region].[South] }, \
                          { [Product].[Widget] }, \
                          { [Measure].[Sales] }) ON COLUMNS, \
         { [Period].[Q1] } ON ROWS FROM [Sales]",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {cs}");
    let (rows, cols, cells) = summary(&cs);
    assert_eq!(rows, vec![vec!["Q1"]]);
    assert_eq!(
        cols,
        vec![
            vec!["North", "Widget", "Sales"],
            vec!["South", "Widget", "Sales"],
        ],
        "3-way crossjoin yields full tuples, Region-major (first component outermost)"
    );
    // Q1: North/Widget/Sales 100, South/Widget/Sales 200.
    assert_eq!(cells, vec!["100", "200"]);
}

/// GAP (documented, NOT fixed): the parser accepts an empty axis set `{} ON ROWS`
/// (see the parser's `select_accepts_nary_crossjoin_axis_and_empty_axis` test),
/// but `view_from_mdx` cannot lower it: an empty set has no member reference, so
/// `axis_dimension` returns `None` and the handler returns a clean 422
/// (`MDX_EVAL_ERROR`). This is an inherent limitation -- core's coverage check
/// requires every dimension to be placed on an axis or in the context, and an
/// empty axis binds no dimension -- so it is pinned here rather than fixed. If a
/// future change makes empty axes lowerable, this test will flag the change.
#[tokio::test]
async fn empty_axis_set_is_a_documented_422_gap() {
    let app = router();
    let token = login(&app, "admin").await;
    let (status, body) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Region].[North] } ON COLUMNS, { } ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Measure].[Sales], [Period].[Q1] )",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an empty axis cannot bind a dimension; body {body}"
    );
    assert_eq!(body["error"]["code"], "MDX_EVAL_ERROR");
}

/// An UNQUALIFIED `.Children` / member on an axis cannot have its dimension
/// determined (`axis_dimension` returns None when the path has < 2 segments), so
/// `view_from_mdx` returns a clean 422 (`MDX_EVAL_ERROR`). This pins the
/// documented limitation: hand-written axes must qualify members as
/// `[Dim].[Member]`.
#[tokio::test]
async fn unqualified_member_axis_is_a_clean_422() {
    let app = router();
    let token = login(&app, "admin").await;
    // `[Total].Children` -- no dimension qualifier, just a bare member name.
    let (status, body) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, [Total].Children ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body {body}");
    assert_eq!(body["error"]["code"], "MDX_EVAL_ERROR");
}

/// An unknown member inside an otherwise-valid axis set surfaces as a clean 422
/// from the evaluator (never a panic or a 500).
#[tokio::test]
async fn unknown_member_in_axis_is_a_clean_422() {
    let app = router();
    let token = login(&app, "admin").await;
    let (status, body) = mdx(
        &app,
        "Sales",
        &token,
        "SELECT { [Measure].[Sales] } ON COLUMNS, { [Region].[Atlantis] } ON ROWS \
         FROM [Sales] WHERE ( [Product].[Widget], [Period].[Q1] )",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body {body}");
    assert_eq!(body["error"]["code"], "MDX_EVAL_ERROR");
}
