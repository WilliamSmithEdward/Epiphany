//! Element-security acceptance (Phase 7G, ADR-0015 decision 4) over the real
//! router with the rule-aware resolver. An admin restricts one leaf member
//! (Region/South) for everyone but `bob`; the suite then proves the three call-
//! site policies: a directly-addressed denied cell is 403, a denied member is
//! suppressed from an axis, and a consolidation (or rule) that rolls up the
//! denied leaf is itself denied -- closing the subtraction-inference leak.
//!
//! Determinism (ADR-0009): pinned `ManualClock` and seeded `IdGen`.

use std::collections::BTreeMap;
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
use epiphany_security::{AccessLevel, AuditLog, ObjectKind, Scope, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Region(North,South,Total=N+S) x Measure(Sales,Cost,Margin); Margin is a rule
/// so the cube exercises rollup and rule re-entry over a denied leaf.
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

struct Harness {
    app: Router,
    security: Arc<Mutex<SecurityStore>>,
}

fn harness(name: &str) -> Harness {
    let dir = std::env::temp_dir().join(format!("epiphany-elemsec-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, margin_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));

    // Seed leaves and a Margin rule (so reads are rule-aware).
    let snap = engine.snapshot("Sales").unwrap();
    let region = |m: &str| snap.cube().dimension(0).resolve(m).unwrap();
    let measure = |m: &str| snap.cube().dimension(1).resolve(m).unwrap();
    let leaf = |r: &str, m: &str, v: i32| CellWrite::Leaf {
        coord: vec![region(r), measure(m)],
        value: Fixed::from(v),
    };
    engine
        .apply_batch(
            "Sales",
            None,
            &[
                leaf("North", "Sales", 100),
                leaf("North", "Cost", 60),
                leaf("South", "Sales", 200),
                leaf("South", "Cost", 150),
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

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    sec.create_user("bob", "pw", false).unwrap();
    // The cube is reachable (Cube:Write for the actors, ADR-0023) so the element
    // ACLs are what gate access within it.
    for user in ["ann", "bob"] {
        sec.set_grant(
            &Subject::User(user.into()),
            Scope::Global,
            ObjectKind::Cube,
            AccessLevel::Write,
        )
        .unwrap();
    }
    let security = Arc::new(Mutex::new(sec));

    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: security.clone(),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(CalcFactory::new(engine)),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Default::default(),
        http: Default::default(),
        sql: Default::default(),
    };
    Harness {
        app: build_router(state),
        security,
    }
}

/// Restrict Region/South to `bob` only (granting bob Read makes the member
/// managed, denying everyone else who is not an admin).
fn restrict_south_to_bob(security: &Arc<Mutex<SecurityStore>>) {
    security
        .lock()
        .unwrap()
        .set_element_access(
            "Sales",
            "Region",
            "South",
            &Subject::User("bob".into()),
            AccessLevel::Read,
        )
        .unwrap();
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
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Value>(&bytes).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn send(
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

/// Issue an authenticated request with no body (GET, or a body-less POST).
async fn send_empty(app: &Router, method: &str, uri: &str, token: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
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

async fn read(app: &Router, token: &str, region: &str, measure: &str) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        token,
        json!({ "coords": [{ "Region": region, "Measure": measure }] }),
    )
    .await
}

/// The numeric value of a single-coord read, asserting 200.
async fn read_value(app: &Router, token: &str, region: &str, measure: &str) -> String {
    let (status, body) = read(app, token, region, measure).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "expected OK reading {region}/{measure}"
    );
    body["cells"][0]["value"].as_str().unwrap().to_string()
}

async fn write(app: &Router, token: &str, region: &str, measure: &str, value: &str) -> StatusCode {
    send(
        app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        token,
        json!({ "coord": { "Region": region, "Measure": measure }, "value": value }),
    )
    .await
    .0
}

async fn cellset(
    app: &Router,
    token: &str,
    regions: &[&str],
    measure: &str,
) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        "/api/v1/cubes/Sales/cellset",
        token,
        json!({
            "rows": [{ "dimension": "Region", "type": "members", "members": regions }],
            "columns": [{ "dimension": "Measure", "type": "members", "members": [measure] }],
        }),
    )
    .await
}

async fn explain(app: &Router, token: &str, region: &str, measure: &str) -> StatusCode {
    send(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/explain",
        token,
        json!({ "coord": { "Region": region, "Measure": measure }, "depth": "immediate" }),
    )
    .await
    .0
}

#[tokio::test]
async fn unrestricted_cube_reads_normally_for_everyone() {
    // With no element ACLs, a non-admin reads every member (no regression).
    let h = harness("open");
    let ann = login(&h.app, "ann").await;
    assert_eq!(read_value(&h.app, &ann, "North", "Sales").await, "100");
    assert_eq!(read_value(&h.app, &ann, "South", "Sales").await, "200");
    assert_eq!(read_value(&h.app, &ann, "Total", "Sales").await, "300");
}

#[tokio::test]
async fn direct_read_of_denied_leaf_and_its_rollup_is_403() {
    let h = harness("direct");
    restrict_south_to_bob(&h.security);
    let ann = login(&h.app, "ann").await;
    let bob = login(&h.app, "bob").await;
    let admin = login(&h.app, "admin").await;

    // ann: the unrestricted leaf reads; the denied leaf and any rollup over it 403.
    assert_eq!(read_value(&h.app, &ann, "North", "Sales").await, "100");
    assert_eq!(
        read(&h.app, &ann, "South", "Sales").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read(&h.app, &ann, "Total", "Sales").await.0,
        StatusCode::FORBIDDEN,
        "the rollup includes a denied leaf"
    );
    // A rule-derived cell over the denied leaf is denied too (rule re-entry).
    assert_eq!(
        read(&h.app, &ann, "South", "Margin").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read(&h.app, &ann, "Total", "Margin").await.0,
        StatusCode::FORBIDDEN
    );
    // North/Margin does not touch South, so it computes (100 - 60 = 40).
    assert_eq!(read_value(&h.app, &ann, "North", "Margin").await, "40");

    // bob (granted Read on South) sees everything; admin bypasses entirely.
    assert_eq!(read_value(&h.app, &bob, "South", "Sales").await, "200");
    assert_eq!(read_value(&h.app, &bob, "Total", "Sales").await, "300");
    assert_eq!(read_value(&h.app, &admin, "South", "Sales").await, "200");
    assert_eq!(read_value(&h.app, &admin, "Total", "Sales").await, "300");
}

#[tokio::test]
async fn denied_members_are_suppressed_from_an_axis() {
    let h = harness("axis");
    restrict_south_to_bob(&h.security);
    let ann = login(&h.app, "ann").await;
    let bob = login(&h.app, "bob").await;

    // ann's cellset over [North, South, Total] keeps only North: South (denied
    // leaf) and Total (rolls up the denied leaf) vanish, and are NOT reported in
    // the suppressed counts (their existence must not leak).
    let (status, cs) = cellset(&h.app, &ann, &["North", "South", "Total"], "Sales").await;
    assert_eq!(status, StatusCode::OK);
    let rows = cs["row_tuples"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only North survives");
    assert_eq!(rows[0][0]["name"], "North");
    assert_eq!(cs["cells"].as_array().unwrap().len(), 1);
    assert_eq!(cs["cells"][0]["value"], "100");
    assert_eq!(
        cs["suppressed"]["row_tuples"], 0,
        "no leak via suppressed list"
    );

    // bob sees all three rows.
    let (status, cs) = cellset(&h.app, &bob, &["North", "South", "Total"], "Sales").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cs["row_tuples"].as_array().unwrap().len(), 3);
    let values: Vec<&str> = cs["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["value"].as_str().unwrap())
        .collect();
    assert_eq!(values, vec!["100", "200", "300"]);
}

#[tokio::test]
async fn writes_enforce_element_security() {
    let h = harness("write");
    restrict_south_to_bob(&h.security);
    let ann = login(&h.app, "ann").await;
    let bob = login(&h.app, "bob").await;
    let admin = login(&h.app, "admin").await;

    // ann may write the unrestricted leaf but not the denied one.
    assert_eq!(
        write(&h.app, &ann, "North", "Sales", "111").await,
        StatusCode::OK
    );
    assert_eq!(
        write(&h.app, &ann, "South", "Sales", "5").await,
        StatusCode::FORBIDDEN
    );
    // bob has only Read on South, so a write is still denied at the element level.
    assert_eq!(
        write(&h.app, &bob, "South", "Sales", "5").await,
        StatusCode::FORBIDDEN
    );
    // The admin may write the restricted leaf.
    assert_eq!(
        write(&h.app, &admin, "South", "Sales", "250").await,
        StatusCode::OK
    );

    // A batch that touches the denied leaf is rejected wholesale: the allowed
    // write in the same batch must not land.
    let (status, _) = send(
        &h.app,
        "POST",
        "/api/v1/cubes/Sales/cells/batch",
        &ann,
        json!({ "writes": [
            { "coord": { "Region": "North", "Measure": "Sales" }, "value": "7" },
            { "coord": { "Region": "South", "Measure": "Sales" }, "value": "7" }
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // North/Sales is unchanged from ann's earlier successful single write (111).
    assert_eq!(read_value(&h.app, &admin, "North", "Sales").await, "111");
}

#[tokio::test]
async fn member_enumeration_suppresses_denied_members() {
    let h = harness("members");
    restrict_south_to_bob(&h.security);
    let ann = login(&h.app, "ann").await;
    let bob = login(&h.app, "bob").await;

    // Previewing a static subset of all Region members: ann sees only North
    // (South denied, Total rolls up South); bob sees all three.
    async fn preview_names(app: &Router, token: &str) -> Vec<String> {
        let (status, body) = send(
            app,
            "POST",
            "/api/v1/cubes/Sales/dimensions/Region/subsets/preview",
            token,
            json!({ "kind": "static", "members": ["North", "South", "Total"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        body["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect()
    }
    assert_eq!(preview_names(&h.app, &ann).await, vec!["North"]);
    assert_eq!(
        preview_names(&h.app, &bob).await,
        vec!["North", "South", "Total"]
    );
}

#[tokio::test]
async fn cube_structure_suppresses_denied_members() {
    let h = harness("structure");
    restrict_south_to_bob(&h.security);
    let ann = login(&h.app, "ann").await;

    // GET /cubes/Sales: ann must not learn South's name. South (denied) and Total
    // (rolls up South) are suppressed from the Region dimension; North remains.
    let (status, detail) = send_empty(&h.app, "GET", "/api/v1/cubes/Sales", &ann).await;
    assert_eq!(status, StatusCode::OK);
    let region = detail["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "Region")
        .unwrap();
    let names: Vec<&str> = region["elements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["North"]);
}

#[tokio::test]
async fn model_tests_are_denied_to_element_restricted_users() {
    let h = harness("modeltests");
    restrict_south_to_bob(&h.security);
    let ann = login(&h.app, "ann").await;
    let admin = login(&h.app, "admin").await;

    // Rule/flow tests evaluate over a clone of the live cube, exposing derived
    // values; an element-restricted caller is denied, an unrestricted admin is not.
    assert_eq!(
        send_empty(&h.app, "POST", "/api/v1/cubes/Sales/rules/tests/run", &ann)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send_empty(
            &h.app,
            "POST",
            "/api/v1/cubes/Sales/rules/tests/run",
            &admin
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn explain_of_a_denied_cell_is_403() {
    let h = harness("explain");
    restrict_south_to_bob(&h.security);
    let ann = login(&h.app, "ann").await;

    // Explaining the denied leaf, or a total that rolls it up, is a direct read.
    assert_eq!(
        explain(&h.app, &ann, "South", "Sales").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        explain(&h.app, &ann, "Total", "Sales").await,
        StatusCode::FORBIDDEN
    );
    // An unrelated cell still explains.
    assert_eq!(
        explain(&h.app, &ann, "North", "Sales").await,
        StatusCode::OK
    );
}
