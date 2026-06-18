//! Security-administration acceptance: the admin REST surface for users, groups,
//! the modular per-object-kind grants (ADR-0023), element ACLs (ADR-0015), and
//! the audit query (ADR-0010). Every route is admin-only and every mutation
//! lands an audit record.

use std::collections::BTreeMap;
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

fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

fn harness(name: &str) -> Router {
    let dir = std::env::temp_dir().join(format!("epiphany-secadmin-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
        clock: Arc::new(ManualClock::new(1_000)),
        security: Arc::new(Mutex::new(sec)),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(epiphany_engine::StoredCellsFactory),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: Default::default(),
        secrets: Default::default(),
        http: Default::default(),
        sql: Default::default(),
    };
    build_router(state)
}

async fn login(app: &Router, user: &str, pass: &str) -> Option<String> {
    let body = json!({ "username": user, "password": pass }).to_string();
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
    if resp.status() != StatusCode::OK {
        return None;
    }
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    Some(v["token"].as_str().unwrap().to_string())
}

/// Log in and return the full response body (token plus the user summary,
/// including `must_change_password`). Asserts the login itself succeeds.
async fn login_body(app: &Router, user: &str, pass: &str) -> Value {
    let body = json!({ "username": user, "password": pass }).to_string();
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
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Issue an authenticated request with an optional JSON body; return status and
/// parsed body (Null for an empty body).
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
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

async fn read_status(app: &Router, token: &str) -> StatusCode {
    call(
        app,
        "POST",
        "/api/v1/cubes/Sales/cells/read",
        token,
        Some(json!({ "coords": [{ "Region": "North", "Measure": "Sales" }] })),
    )
    .await
    .0
}

async fn write_status(app: &Router, token: &str) -> StatusCode {
    call(
        app,
        "PUT",
        "/api/v1/cubes/Sales/cell",
        token,
        Some(json!({ "coord": { "Region": "North", "Measure": "Sales" }, "value": "5" })),
    )
    .await
    .0
}

#[tokio::test]
async fn non_admin_is_forbidden_from_every_admin_route() {
    let app = harness("nonadmin");
    let ann = login(&app, "ann", "pw").await.unwrap();

    for (method, uri, body) in [
        ("GET", "/api/v1/users", None),
        (
            "POST",
            "/api/v1/users",
            Some(json!({ "username": "x", "password": "pw" })),
        ),
        ("GET", "/api/v1/groups", None),
        ("GET", "/api/v1/runs", None),
        ("GET", "/api/v1/acl/elements", None),
        ("GET", "/api/v1/acl/grants", None),
        (
            "PUT",
            "/api/v1/acl/grants",
            Some(
                json!({ "subject_kind": "group", "subject": "fa", "scope": "global", "kind": "flow", "level": "write" }),
            ),
        ),
        ("GET", "/api/v1/audit", None),
    ] {
        let (status, _) = call(&app, method, uri, &ann, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
}

#[tokio::test]
async fn admin_sets_and_lists_per_kind_grants() {
    let app = harness("grants");
    let admin = login(&app, "admin", "pw").await.unwrap();

    // Grant a "flow authors" group Flow:Write globally (ADR-0023).
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/acl/grants",
        &admin,
        Some(json!({
            "subject_kind": "group", "subject": "flow_authors",
            "scope": "global", "kind": "flow", "level": "write"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(&app, "GET", "/api/v1/acl/grants", &admin, None).await;
    let grants = body["grants"].as_array().unwrap();
    assert!(grants.iter().any(|g| {
        g["subject"] == "flow_authors"
            && g["kind"] == "flow"
            && g["scope"] == "global"
            && g["level"] == "write"
    }));

    // Revoking with level=none removes it.
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/acl/grants",
        &admin,
        Some(json!({
            "subject_kind": "group", "subject": "flow_authors",
            "scope": "global", "kind": "flow", "level": "none"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = call(&app, "GET", "/api/v1/acl/grants", &admin, None).await;
    assert!(body["grants"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn admin_manages_users_groups_and_membership() {
    let app = harness("users");
    let admin = login(&app, "admin", "pw").await.unwrap();

    // Create a user.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/users",
        &admin,
        Some(json!({ "username": "carl", "password": "pw", "is_admin": false })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // The new user can authenticate.
    assert!(login(&app, "carl", "pw").await.is_some());

    // A duplicate is a conflict.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/users",
        &admin,
        Some(json!({ "username": "carl", "password": "pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Create a group and put carl in it.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/groups",
        &admin,
        Some(json!({ "name": "editors" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        &app,
        "PATCH",
        "/api/v1/users/carl",
        &admin,
        Some(json!({ "groups": ["editors"] })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The listing reflects the membership.
    let (status, users) = call(&app, "GET", "/api/v1/users", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    let carl = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "carl")
        .unwrap();
    assert_eq!(carl["groups"], json!(["editors"]));
    assert_eq!(carl["is_admin"], false);

    // Reset carl's password; the old one stops working, the new one works.
    let (status, _) = call(
        &app,
        "PATCH",
        "/api/v1/users/carl",
        &admin,
        Some(json!({ "password": "pw2" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(login(&app, "carl", "pw").await.is_none());
    assert!(login(&app, "carl", "pw2").await.is_some());

    // Promote carl to admin; he can now reach the admin surface.
    let carl_before = login(&app, "carl", "pw2").await.unwrap();
    assert_eq!(
        call(&app, "GET", "/api/v1/users", &carl_before, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, _) = call(
        &app,
        "PATCH",
        "/api/v1/users/carl",
        &admin,
        Some(json!({ "is_admin": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // A freshly minted session resolves the new admin status.
    let carl_admin = login(&app, "carl", "pw2").await.unwrap();
    assert_eq!(
        call(&app, "GET", "/api/v1/users", &carl_admin, None)
            .await
            .0,
        StatusCode::OK
    );

    // Delete the group, then the user.
    assert_eq!(
        call(&app, "DELETE", "/api/v1/groups/editors", &admin, None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (_, groups) = call(&app, "GET", "/api/v1/groups", &admin, None).await;
    assert!(!groups["groups"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g == "editors"));
    assert_eq!(
        call(&app, "DELETE", "/api/v1/users/carl", &admin, None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert!(login(&app, "carl", "pw2").await.is_none());
    // Deleting an absent user is a 404.
    assert_eq!(
        call(&app, "DELETE", "/api/v1/users/carl", &admin, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn admin_resets_user_to_temp_password_and_forces_change() {
    let app = harness("temppw");
    let admin = login(&app, "admin", "pw").await.unwrap();

    // Admin resets ann to a freshly generated temporary password; it is returned
    // once in the response body.
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/users/ann/reset-password",
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["username"], "ann");
    let temp = body["temp_password"].as_str().unwrap().to_string();
    assert!(!temp.is_empty(), "a temporary password is returned");

    // The old password stops working; the temporary one logs in and reports that
    // a change is required.
    assert!(login(&app, "ann", "pw").await.is_none());
    let first = login_body(&app, "ann", &temp).await;
    assert_eq!(first["user"]["must_change_password"], json!(true));
    let ann = first["token"].as_str().unwrap().to_string();

    // Changing it with the temp clears the forced-change requirement.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/auth/password",
        &ann,
        Some(json!({ "current_password": temp, "new_password": "ann-brand-new-1" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let after = login_body(&app, "ann", "ann-brand-new-1").await;
    assert_eq!(after["user"]["must_change_password"], json!(false));

    // Resetting an unknown user is a 404.
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/v1/users/ghost/reset-password",
            &admin,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // A non-admin cannot reset anyone (admin-only surface).
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/v1/users/admin/reset-password",
            &ann,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn cube_grant_via_rest_governs_cube_access() {
    let app = harness("cubegrant");
    let admin = login(&app, "admin", "pw").await.unwrap();
    let ann = login(&app, "ann", "pw").await.unwrap();

    // Add a second non-admin, in a group.
    call(
        &app,
        "POST",
        "/api/v1/users",
        &admin,
        Some(json!({ "username": "dan", "password": "pw", "groups": ["viewers"] })),
    )
    .await;
    let dan = login(&app, "dan", "pw").await.unwrap();

    // Fail-closed (ADR-0023): an ungranted cube denies both non-admins.
    assert_eq!(read_status(&app, &ann).await, StatusCode::FORBIDDEN);
    assert_eq!(read_status(&app, &dan).await, StatusCode::FORBIDDEN);

    // Grant the group Cube:Read on Sales via the modular grants surface.
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/acl/grants",
        &admin,
        Some(json!({
            "subject_kind": "group", "subject": "viewers",
            "scope": "cube", "cube": "Sales", "kind": "cube", "level": "read"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // dan (in viewers) reads but cannot write; ann (ungranted) is still denied.
    assert_eq!(read_status(&app, &dan).await, StatusCode::OK);
    assert_eq!(write_status(&app, &dan).await, StatusCode::FORBIDDEN);
    assert_eq!(read_status(&app, &ann).await, StatusCode::FORBIDDEN);

    // The grant is listed.
    let (_, grants) = call(&app, "GET", "/api/v1/acl/grants", &admin, None).await;
    assert!(grants["grants"].as_array().unwrap().iter().any(|g| {
        g["kind"] == "cube"
            && g["cube"] == "Sales"
            && g["subject_kind"] == "group"
            && g["subject"] == "viewers"
            && g["level"] == "read"
    }));

    // Revoke (level none): dan loses read again.
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/acl/grants",
        &admin,
        Some(json!({
            "subject_kind": "group", "subject": "viewers",
            "scope": "cube", "cube": "Sales", "kind": "cube", "level": "none"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(read_status(&app, &dan).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn element_grant_roundtrips_via_rest() {
    let app = harness("elemacl");
    let admin = login(&app, "admin", "pw").await.unwrap();

    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/acl/elements",
        &admin,
        Some(json!({
            "cube": "Sales", "dimension": "Region", "element": "North",
            "subject_kind": "user", "subject": "ann", "level": "read"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, grants) = call(&app, "GET", "/api/v1/acl/elements", &admin, None).await;
    let g = &grants["grants"].as_array().unwrap()[0];
    assert_eq!(g["cube"], "Sales");
    assert_eq!(g["dimension"], "Region");
    assert_eq!(g["element"], "North");
    assert_eq!(g["subject"], "ann");
    assert_eq!(g["level"], "read");

    // A bad level is a 400.
    let (status, _) = call(
        &app,
        "PUT",
        "/api/v1/acl/elements",
        &admin,
        Some(json!({
            "cube": "Sales", "dimension": "Region", "element": "North",
            "subject_kind": "user", "subject": "ann", "level": "super"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Revoke.
    call(
        &app,
        "PUT",
        "/api/v1/acl/elements",
        &admin,
        Some(json!({
            "cube": "Sales", "dimension": "Region", "element": "North",
            "subject_kind": "user", "subject": "ann", "level": "none"
        })),
    )
    .await;
    let (_, grants) = call(&app, "GET", "/api/v1/acl/elements", &admin, None).await;
    assert!(grants["grants"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn audit_query_filters_logins_changes_and_denials() {
    let app = harness("audit");
    let admin = login(&app, "admin", "pw").await.unwrap();

    // A denied admin attempt by a non-admin lands an access_denied record.
    let ann = login(&app, "ann", "pw").await.unwrap();
    call(&app, "GET", "/api/v1/users", &ann, None).await;

    // A user-change (create) lands a user_change record.
    call(
        &app,
        "POST",
        "/api/v1/users",
        &admin,
        Some(json!({ "username": "eve", "password": "pw" })),
    )
    .await;

    // Logins are audited too.
    let (status, all) = call(&app, "GET", "/api/v1/audit", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!all["records"].as_array().unwrap().is_empty());

    // Filter to denials only.
    let (_, denied) = call(&app, "GET", "/api/v1/audit?outcome=denied", &admin, None).await;
    let denials = denied["records"].as_array().unwrap();
    assert!(!denials.is_empty());
    assert!(denials.iter().all(|r| r["allowed"] == false));
    assert!(denials.iter().any(|r| r["action"] == "access_denied"));

    // Filter by action token.
    let (_, changes) = call(
        &app,
        "GET",
        "/api/v1/audit?action=user_change",
        &admin,
        None,
    )
    .await;
    let changes = changes["records"].as_array().unwrap();
    assert!(!changes.is_empty());
    assert!(changes.iter().all(|r| r["action"] == "user_change"));

    // Filter by actor.
    let (_, by_actor) = call(&app, "GET", "/api/v1/audit?actor=ann", &admin, None).await;
    let recs = by_actor["records"].as_array().unwrap();
    assert!(!recs.is_empty());
    assert!(recs.iter().all(|r| r["actor"] == "ann"));
}
