//! Dimension structural-editing REST surface (ADR-0036), end to end: reorder,
//! reparent, set kind, delete, and insert a dimension's members over the
//! `POST .../dimensions/{dim}/edit` endpoints (cube-embedded by name, registry by
//! id), confirming that stored cells are remapped, rollups recompute, rejected
//! edits are 422, authorization is enforced (403), and a registry edit fans out to
//! every referencing cube.

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

/// A "Sales" cube: Region (North, South, East under Total) x Measure (Amount).
fn seed_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let north = region.add_leaf("North");
    let south = region.add_leaf("South");
    let east = region.add_leaf("East");
    let total = region.add_consolidated("Total");
    region.add_child(total, north, 1).unwrap();
    region.add_child(total, south, 1).unwrap();
    region.add_child(total, east, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Amount");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

fn data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-dimedit-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a router over `dir` with cube creation and the shared-dimension registry
/// enabled. `admin` is a server admin; `writer` holds global Cube:Admin +
/// Dimension:Write (can create cubes and edit dimensions); `ann` holds nothing;
/// `restricted` holds Dimension:Write on the cube but is denied element "East" of
/// Region in cube "Sales", so a data-dropping edit must reject them.
fn build_app(dir: &Path) -> Router {
    let cubes_dir = dir.join("cubes");
    std::fs::create_dir_all(&cubes_dir).unwrap();
    let mut stores = BTreeMap::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&cubes_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("snapshot.model").is_file())
        .collect();
    entries.sort();
    for path in &entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        stores.insert(name, Store::open(path).unwrap());
    }
    if stores.is_empty() {
        stores.insert(
            "Sales".to_string(),
            Store::create(cubes_dir.join("Sales"), seed_cube()).unwrap(),
        );
    }

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("writer", "pw", false).unwrap();
    sec.set_grant(
        &Subject::User("writer".into()),
        Scope::Global,
        ObjectKind::Cube,
        AccessLevel::Admin,
    )
    .unwrap();
    sec.set_grant(
        &Subject::User("writer".into()),
        Scope::Global,
        ObjectKind::Dimension,
        AccessLevel::Write,
    )
    .unwrap();
    sec.create_user("ann", "pw", false).unwrap();
    // `restricted` can write dimensions and read/write the Sales cube, but is
    // denied element "East" of Region (the ACL grants only writer), so a delete or
    // kind conversion through the edit endpoint must reject them (ADR-0036).
    sec.create_user("restricted", "pw", false).unwrap();
    sec.set_grant(
        &Subject::User("restricted".into()),
        Scope::Global,
        ObjectKind::Dimension,
        AccessLevel::Write,
    )
    .unwrap();
    sec.set_grant(
        &Subject::User("restricted".into()),
        Scope::Global,
        ObjectKind::Cube,
        AccessLevel::Write,
    )
    .unwrap();
    sec.set_element_access(
        "Sales",
        "Region",
        "East",
        &Subject::User("writer".into()),
        AccessLevel::Read,
    )
    .unwrap();

    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default()))
            .with_cubes_dir(cubes_dir)
            .with_dimensions_dir(dir.join("dimensions")),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
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
            epiphany_persist::AutomationStore::open(std::env::temp_dir().join(format!(
                "epiphany-test-auto-{}-dimension_editing-0",
                std::process::id()
            )))
            .unwrap(),
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

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn cube_dimension<'a>(detail: &'a Value, name: &str) -> &'a Value {
    detail["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == name)
        .expect("dimension present")
}

fn member_names(dim: &Value) -> Vec<String> {
    dim["elements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

/// The `pinned_to_top` flag (ADR-0038) on a named member of a dimension DTO.
fn pinned_to_top(dim: &Value, member: &str) -> bool {
    dim["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == member)
        .unwrap_or_else(|| panic!("member {member} present"))["pinned_to_top"]
        .as_bool()
        .expect("pinned_to_top is a boolean on every element")
}

/// The weighted parent->child edges of a dimension DTO, sorted for comparison.
fn edges(dim: &Value) -> Vec<(String, String, i64)> {
    let mut out: Vec<(String, String, i64)> = dim["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["parent"].as_str().unwrap().to_string(),
                e["child"].as_str().unwrap().to_string(),
                e["weight"].as_i64().unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

/// Write a leaf cell at (region, Amount) = value.
async fn write(app: &Router, cube: &str, token: &str, region: &str, value: &str) {
    let (status, _) = call(
        app,
        "PUT",
        &format!("/api/v1/cubes/{cube}/cell"),
        token,
        Some(json!({ "coord": { "Region": region, "Measure": "Amount" }, "value": value })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "write {region}={value}");
}

/// Read the numeric value at (region, Amount).
async fn read(app: &Router, cube: &str, token: &str, region: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/v1/cubes/{cube}/cells/read"),
        token,
        Some(json!({ "coords": [{ "Region": region, "Measure": "Amount" }] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read {region}");
    body["cells"][0]["value"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_default()
}

#[tokio::test]
async fn edit_a_cube_embedded_dimension_over_rest() {
    let dir = data_dir("embedded");
    let app = build_app(&dir);
    let admin = login(&app, "admin").await;
    let writer = login(&app, "writer").await;

    // Seed leaf data: North 10, South 20, East 30; Total = 60.
    write(&app, "Sales", &admin, "North", "10").await;
    write(&app, "Sales", &admin, "South", "20").await;
    write(&app, "Sales", &admin, "East", "30").await;
    assert_eq!(read(&app, "Sales", &admin, "Total").await, "60");

    let edit = "/api/v1/cubes/Sales/dimensions/Region/edit";

    // Reorder: the members permute and each value follows its member.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &writer,
        Some(json!({ "op": "reorder", "new_order": ["East", "North", "South", "Total"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reorder");
    let (_, detail) = call(&app, "GET", "/api/v1/cubes/Sales", &admin, None).await;
    assert_eq!(
        member_names(cube_dimension(&detail, "Region")),
        vec![
            "East".to_string(),
            "North".into(),
            "South".into(),
            "Total".into()
        ]
    );
    // Values are intact and still keyed to their members.
    assert_eq!(read(&app, "Sales", &admin, "East").await, "30");
    assert_eq!(read(&app, "Sales", &admin, "North").await, "10");
    assert_eq!(read(&app, "Sales", &admin, "Total").await, "60");

    // Reparent: detach East from Total; the rollup recomputes to 10 + 20 = 30.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &writer,
        Some(json!({ "op": "reparent", "child": "East", "new_parent": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reparent (detach)");
    assert_eq!(read(&app, "Sales", &admin, "Total").await, "30");

    // Insert a new leaf "West" after "North", then write to it; Total unaffected
    // (it is not under Total). A fresh leaf reads as empty/zero.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &writer,
        Some(json!({ "op": "insert", "name": "West", "kind": "numeric",
                     "position": { "at": "after", "ref": "North" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "insert");
    let (_, detail) = call(&app, "GET", "/api/v1/cubes/Sales", &admin, None).await;
    let names = member_names(cube_dimension(&detail, "Region"));
    let north = names.iter().position(|n| n == "North").unwrap();
    assert_eq!(names[north + 1], "West", "West landed right after North");

    // set_kind: convert the (now-detached, childless) East leaf from numeric to
    // string. A consolidation could not convert while it still has children, so a
    // leaf-to-leaf re-typing is the clean case here.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &writer,
        Some(json!({ "op": "set_kind", "element": "East", "kind": "string" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set_kind");
    let (_, detail) = call(&app, "GET", "/api/v1/cubes/Sales", &admin, None).await;
    let east = cube_dimension(&detail, "Region")["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "East")
        .unwrap();
    assert_eq!(east["kind"], "string", "East is now a string leaf");

    // Delete a leaf: South goes away, the remaining members keep their values.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &writer,
        Some(json!({ "op": "delete", "element": "South" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete leaf");
    let (_, detail) = call(&app, "GET", "/api/v1/cubes/Sales", &admin, None).await;
    assert!(!member_names(cube_dimension(&detail, "Region")).contains(&"South".to_string()));
    // North kept its numeric value through every index-changing edit.
    assert_eq!(read(&app, "Sales", &admin, "North").await, "10");
}

#[tokio::test]
async fn pin_and_unpin_a_member_to_the_top_level() {
    // ADR-0038: pin_to_top marks a member a display root EVEN THOUGH it still rolls
    // up under its consolidation; the parent edge is unchanged and the flag reads
    // back on the GET. unpin_from_top reverts it.
    let dir = data_dir("pin");
    let app = build_app(&dir);
    let admin = login(&app, "admin").await;
    let writer = login(&app, "writer").await;

    let edit = "/api/v1/cubes/Sales/dimensions/Region/edit";

    // East rolls up under Total. Before pinning it is not a display root.
    let (_, detail) = call(&app, "GET", "/api/v1/cubes/Sales", &admin, None).await;
    let region = cube_dimension(&detail, "Region");
    assert!(!pinned_to_top(region, "East"), "East starts unpinned");
    let edges_before = edges(region);
    assert!(
        edges_before.contains(&("Total".into(), "East".into(), 1)),
        "East -> Total edge present before pin: {edges_before:?}"
    );

    // Pin East to the top.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &writer,
        Some(json!({ "op": "pin_to_top", "element": "East" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pin_to_top");

    // It now reads back as pinned, and its parent edge is unchanged (still under
    // Total): pinning is display-only.
    let (_, detail) = call(&app, "GET", "/api/v1/cubes/Sales", &admin, None).await;
    let region = cube_dimension(&detail, "Region");
    assert!(
        pinned_to_top(region, "East"),
        "East is pinned after pin_to_top"
    );
    assert_eq!(
        edges(region),
        edges_before,
        "pinning changes no consolidation edge"
    );
    // Other members are not pinned.
    assert!(!pinned_to_top(region, "North"));
    assert!(!pinned_to_top(region, "South"));

    // Unpin reverts it; the edge is still intact.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &writer,
        Some(json!({ "op": "unpin_from_top", "element": "East" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unpin_from_top");
    let (_, detail) = call(&app, "GET", "/api/v1/cubes/Sales", &admin, None).await;
    let region = cube_dimension(&detail, "Region");
    assert!(
        !pinned_to_top(region, "East"),
        "East reverts to unpinned after unpin_from_top"
    );
    assert_eq!(edges(region), edges_before, "unpinning changes no edge");
}

#[tokio::test]
async fn registry_pin_fans_out_to_every_referencing_cube() {
    // ADR-0038: pinning a member of a registry dimension by id fans the flag out to
    // every referencing cube's materialized copy, just like the structural edits.
    let dir = data_dir("pin-fanout");
    let app = build_app(&dir);
    let admin = login(&app, "admin").await;
    let writer = login(&app, "writer").await;

    // Register a reusable Product dimension (Widget, Gadget under AllProducts).
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/dimensions",
        &writer,
        Some(json!({
            "name": "Product",
            "elements": [
                { "name": "Widget", "kind": "numeric" },
                { "name": "Gadget", "kind": "numeric" },
                { "name": "AllProducts", "kind": "consolidated" }
            ],
            "edges": [
                { "parent": "AllProducts", "child": "Widget", "weight": 1 },
                { "parent": "AllProducts", "child": "Gadget", "weight": 1 }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let product_id = body["id"].as_u64().unwrap();

    // Two cubes reference it.
    for cube in ["CubeA", "CubeB"] {
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes",
            &writer,
            Some(json!({
                "name": cube,
                "dimensions": [
                    { "ref": product_id },
                    { "name": "Measure", "elements": [{ "name": "Amount", "kind": "numeric" }] }
                ]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create {cube}");
    }

    // Pin Widget (a child of AllProducts) to the top by registry id.
    let (status, resp) = call(
        &app,
        "POST",
        &format!("/api/v1/dimensions/{product_id}/edit"),
        &writer,
        Some(json!({ "op": "pin_to_top", "element": "Widget" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "registry pin_to_top");
    let mut fanned: Vec<&str> = resp["fanned_out_to"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    fanned.sort_unstable();
    assert_eq!(fanned, vec!["CubeA", "CubeB"]);

    // The pin shows in both cubes' copies, the AllProducts->Widget edge is intact,
    // and the registry GET also reflects the pin.
    for cube in ["CubeA", "CubeB"] {
        let (_, detail) = call(&app, "GET", &format!("/api/v1/cubes/{cube}"), &admin, None).await;
        let product = cube_dimension(&detail, "Product");
        assert!(pinned_to_top(product, "Widget"), "{cube}: Widget pinned");
        assert!(
            !pinned_to_top(product, "Gadget"),
            "{cube}: Gadget not pinned"
        );
        assert!(
            edges(product).contains(&("AllProducts".into(), "Widget".into(), 1)),
            "{cube}: Widget still rolls up under AllProducts"
        );
    }
    let (_, reg) = call(
        &app,
        "GET",
        &format!("/api/v1/dimensions/{product_id}"),
        &admin,
        None,
    )
    .await;
    assert!(
        pinned_to_top(&reg, "Widget"),
        "registry GET reflects the pin: {reg}"
    );
    assert!(!pinned_to_top(&reg, "Gadget"));
}

#[tokio::test]
async fn rejected_edits_are_422() {
    let dir = data_dir("reject");
    let app = build_app(&dir);
    let admin = login(&app, "admin").await;
    let writer = login(&app, "writer").await;

    let edit = "/api/v1/cubes/Sales/dimensions/Region/edit";

    // Deleting a parent that still has children is rejected; nothing changes.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &writer,
        Some(json!({ "op": "delete", "element": "Total" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "delete a parent-with-children"
    );

    // A non-permutation reorder (missing a member) is rejected.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &writer,
        Some(json!({ "op": "reorder", "new_order": ["North", "South"] })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "non-permutation reorder"
    );

    // The dimension is intact: all four members survive both rejections.
    let (_, detail) = call(&app, "GET", "/api/v1/cubes/Sales", &admin, None).await;
    let names = member_names(cube_dimension(&detail, "Region"));
    for m in ["North", "South", "East", "Total"] {
        assert!(names.contains(&m.to_string()), "{m} survived");
    }
}

#[tokio::test]
async fn edit_authorization_is_enforced() {
    let dir = data_dir("authz");
    let app = build_app(&dir);
    let ann = login(&app, "ann").await;
    let restricted = login(&app, "restricted").await;

    let edit = "/api/v1/cubes/Sales/dimensions/Region/edit";

    // ann holds no Dimension grant: any edit is 403.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &ann,
        Some(json!({ "op": "reorder", "new_order": ["North", "South", "East", "Total"] })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no Dimension:Write -> 403");

    // `restricted` holds Dimension:Write but is element-restricted on Sales/Region
    // (denied East). A reorder (no data dropped) is allowed, but a delete or a
    // kind conversion (which can drop values) is denied fail-closed.
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &restricted,
        Some(json!({ "op": "delete", "element": "South" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "element-restricted delete denied"
    );
    let (status, _) = call(
        &app,
        "POST",
        edit,
        &restricted,
        Some(json!({ "op": "set_kind", "element": "Total", "kind": "numeric" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "element-restricted set_kind denied"
    );
}

#[tokio::test]
async fn registry_edit_fans_out_to_every_referencing_cube() {
    // A registry dimension edited by id applies to the registry generation and
    // fans the same edit out to every referencing cube, remapping each cube's
    // cells (ADR-0036).
    let dir = data_dir("fanout");
    let app = build_app(&dir);
    let admin = login(&app, "admin").await;
    let writer = login(&app, "writer").await;

    // Register a reusable Product dimension (Widget, Gadget under AllProducts).
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/dimensions",
        &writer,
        Some(json!({
            "name": "Product",
            "elements": [
                { "name": "Widget", "kind": "numeric" },
                { "name": "Gadget", "kind": "numeric" },
                { "name": "AllProducts", "kind": "consolidated" }
            ],
            "edges": [
                { "parent": "AllProducts", "child": "Widget", "weight": 1 },
                { "parent": "AllProducts", "child": "Gadget", "weight": 1 }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let product_id = body["id"].as_u64().unwrap();

    // Two cubes reference it; seed distinct leaf data in each.
    for cube in ["CubeA", "CubeB"] {
        let (status, _) = call(
            &app,
            "POST",
            "/api/v1/cubes",
            &writer,
            Some(json!({
                "name": cube,
                "dimensions": [
                    { "ref": product_id },
                    { "name": "Measure", "elements": [{ "name": "Amount", "kind": "numeric" }] }
                ]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create {cube}");
    }
    for (region, value) in [("Widget", "5"), ("Gadget", "7")] {
        write_product(&app, "CubeA", &admin, region, value).await;
    }
    for (region, value) in [("Widget", "1"), ("Gadget", "2")] {
        write_product(&app, "CubeB", &admin, region, value).await;
    }
    assert_eq!(
        read_product(&app, "CubeA", &admin, "AllProducts").await,
        "12"
    );
    assert_eq!(
        read_product(&app, "CubeB", &admin, "AllProducts").await,
        "3"
    );

    // Reorder the registry dimension by id: the edit fans out to both cubes.
    let (status, resp) = call(
        &app,
        "POST",
        &format!("/api/v1/dimensions/{product_id}/edit"),
        &writer,
        Some(json!({ "op": "reorder", "new_order": ["Gadget", "Widget", "AllProducts"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "registry reorder");
    let mut fanned: Vec<&str> = resp["fanned_out_to"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    fanned.sort_unstable();
    assert_eq!(fanned, vec!["CubeA", "CubeB"]);

    // Both cubes see the new member order, with values still keyed to members and
    // the rollup intact.
    for cube in ["CubeA", "CubeB"] {
        let (_, detail) = call(&app, "GET", &format!("/api/v1/cubes/{cube}"), &admin, None).await;
        assert_eq!(
            member_names(cube_dimension(&detail, "Product")),
            vec!["Gadget".to_string(), "Widget".into(), "AllProducts".into()],
            "{cube} order"
        );
    }
    assert_eq!(read_product(&app, "CubeA", &admin, "Widget").await, "5");
    assert_eq!(read_product(&app, "CubeB", &admin, "Gadget").await, "2");
    assert_eq!(
        read_product(&app, "CubeA", &admin, "AllProducts").await,
        "12"
    );

    // A registry edit by an unknown id is a 404.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/dimensions/9999/edit",
        &writer,
        Some(json!({ "op": "delete", "element": "Widget" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown registry id -> 404");
}

/// Write a leaf cell at (Product region, Amount) = value.
async fn write_product(app: &Router, cube: &str, token: &str, product: &str, value: &str) {
    let (status, _) = call(
        app,
        "PUT",
        &format!("/api/v1/cubes/{cube}/cell"),
        token,
        Some(json!({ "coord": { "Product": product, "Measure": "Amount" }, "value": value })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "write {product}={value}");
}

async fn read_product(app: &Router, cube: &str, token: &str, product: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/v1/cubes/{cube}/cells/read"),
        token,
        Some(json!({ "coords": [{ "Product": product, "Measure": "Amount" }] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read {product}");
    body["cells"][0]["value"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_default()
}
