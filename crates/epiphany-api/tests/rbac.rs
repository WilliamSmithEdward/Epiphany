//! Modular per-object-kind permissions (ADR-0023), end to end over REST. Proves
//! the separation the scheme exists for: a data-entry user (cube Write) cannot
//! edit the model, and a "flow author" (Flow:Write) can create and run flows but
//! cannot write cells or edit dimensions. Cube data access still uses the prior
//! cube-level grant during the migration (RBAC-2); model-object editing uses the
//! new per-kind gate.

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
use epiphany_security::{AccessLevel, AuditLog, ObjectKind, Scope, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Amount");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

fn app(tag: &str) -> Router {
    let dir = std::env::temp_dir().join(format!("epiphany-rbac-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert(
        "Sales".to_string(),
        Store::create(dir.join("Sales"), sales_cube()).unwrap(),
    );

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("fa", "pw", false).unwrap(); // flow author
    sec.create_user("de", "pw", false).unwrap(); // data entry
    sec.create_user("nobody", "pw", false).unwrap(); // no grants
    sec.create_user("mgr", "pw", false).unwrap(); // cube manager (below admin)
                                                  // Cube manager: a global Cube:Admin grant -> may create/delete cubes and admin
                                                  // all cubes, without server-admin powers.
    sec.set_grant(
        &Subject::User("mgr".into()),
        Scope::Global,
        ObjectKind::Cube,
        AccessLevel::Admin,
    )
    .unwrap();
    // Flow author: a per-kind Flow:Write grant (the new scheme), no cube grant.
    sec.set_grant(
        &Subject::User("fa".into()),
        Scope::Global,
        ObjectKind::Flow,
        AccessLevel::Write,
    )
    .unwrap();
    // Data-entry user: Cube:Write on Sales, no per-kind model grants.
    sec.set_grant(
        &Subject::User("de".into()),
        Scope::Cube("Sales".into()),
        ObjectKind::Cube,
        AccessLevel::Write,
    )
    .unwrap();
    // Power user: a flow author who ALSO has cube Write, so running a
    // cell-writing flow is within their own access.
    sec.create_user("power", "pw", false).unwrap();
    sec.set_grant(
        &Subject::User("power".into()),
        Scope::Global,
        ObjectKind::Flow,
        AccessLevel::Write,
    )
    .unwrap();
    sec.set_grant(
        &Subject::User("power".into()),
        Scope::Cube("Sales".into()),
        ObjectKind::Cube,
        AccessLevel::Write,
    )
    .unwrap();
    // Job author: Job:Write but no cube/dimension write, so they may define jobs
    // but cannot manually kick one into writing the cube.
    sec.create_user("jobber", "pw", false).unwrap();
    sec.set_grant(
        &Subject::User("jobber".into()),
        Scope::Global,
        ObjectKind::Job,
        AccessLevel::Write,
    )
    .unwrap();

    let state = AppState {
        engine: Engine::from_stores(stores, Arc::new(IdGen::default())).with_cubes_dir(dir),
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

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> StatusCode {
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
    app.clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap()
        .status()
}

const FLOW_SRC: &str = "function rows(ctx) {}\n";
// A flow that writes a cell regardless of input (used to prove a flow cannot
// exceed the runner's own data access).
const WRITING_FLOW: &str =
    "function rows(ctx) { ctx.writeCells([{ coord: { Region: \"North\", Measure: \"Amount\" }, value: \"7\" }]); }\n";

#[tokio::test]
async fn per_kind_gates_separate_modeler_from_data_entry() {
    let app = app("matrix");
    let admin = login(&app, "admin").await;
    let fa = login(&app, "fa").await;
    let de = login(&app, "de").await;
    let nobody = login(&app, "nobody").await;

    // The put_flow handler takes the flow name from the path; the body just
    // carries the source, so one body is reused for every PUT.
    let flow_body = json!({ "name": "f", "source": FLOW_SRC });
    let write_cell = json!({ "coord": { "Region": "North", "Measure": "Amount" }, "value": "5" });
    let add_element =
        json!({ "elements": [{ "dimension": "Region", "name": "East", "kind": "numeric" }] });

    // Admin bypasses everything.
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/flows/load",
            &admin,
            Some(flow_body.clone())
        )
        .await,
        StatusCode::OK
    );

    // Flow author: can create AND run flows (Flow:Write) ...
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/flows/fa_flow",
            &fa,
            Some(flow_body.clone())
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Sales/flows/fa_flow/run",
            &fa,
            Some(json!({ "input": "" })),
        )
        .await,
        StatusCode::OK
    );
    // ... but cannot write cells (no cube Write) or edit dimensions (no Dimension grant).
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/cell",
            &fa,
            Some(write_cell.clone())
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Sales/elements",
            &fa,
            Some(add_element.clone())
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // Data-entry user: can write cells (cube Write) ...
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/cell",
            &de,
            Some(write_cell.clone())
        )
        .await,
        StatusCode::OK
    );
    // ... but CANNOT edit the model (no per-kind grant) - the core separation.
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/flows/de_flow",
            &de,
            Some(flow_body.clone())
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Sales/elements",
            &de,
            Some(add_element.clone())
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // No grants: denied everywhere (fail-closed). Cube creation needs global
    // Cube:Admin, which nobody has.
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/flows/x",
            &nobody,
            Some(flow_body.clone())
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes",
            &de,
            Some(json!({ "name": "New", "dimensions": [{ "name": "D", "elements": [{ "name": "a", "kind": "numeric" }] }] })),
        )
        .await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn flow_run_cannot_exceed_runners_data_access() {
    let app = app("nuance");
    let admin = login(&app, "admin").await;
    let fa = login(&app, "fa").await; // Flow:Write, NO cube write
    let power = login(&app, "power").await; // Flow:Write AND cube write

    // Store a flow that writes a cell (admin can).
    let writing = json!({ "name": "w", "source": WRITING_FLOW });
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/flows/wflow",
            &admin,
            Some(writing)
        )
        .await,
        StatusCode::OK
    );

    // The flow author can run flows in general, but running THIS one is denied:
    // it would write a cell they cannot write directly (no cube Write).
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Sales/flows/wflow/run",
            &fa,
            Some(json!({ "input": "" }))
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // The power user (flow author WITH cube Write) can run it - the effect is
    // within their own access.
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Sales/flows/wflow/run",
            &power,
            Some(json!({ "input": "" }))
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn manual_job_kick_cannot_exceed_runners_access() {
    let app = app("jobkick");
    let admin = login(&app, "admin").await;
    let jobber = login(&app, "jobber").await; // Job:Write only, no cube write

    // Admin defines a flow and a job that runs it.
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/flows/load",
            &admin,
            Some(json!({ "name": "load", "source": FLOW_SRC }))
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/v1/cubes/Sales/jobs/nightly",
            &admin,
            Some(json!({ "name": "nightly", "steps": ["load"], "every_millis": 3_600_000, "enabled": false })),
        )
        .await,
        StatusCode::OK
    );

    // The job author can edit jobs (Job:Write) but a manual kick is denied: it
    // would run flows as them, and they lack cube/dimension write.
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Sales/jobs/nightly/run",
            &jobber,
            None
        )
        .await,
        StatusCode::FORBIDDEN
    );
    // Admin can kick it.
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes/Sales/jobs/nightly/run",
            &admin,
            None
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn global_cube_admin_creates_cubes_below_server_admin() {
    let app = app("mgr");
    let mgr = login(&app, "mgr").await; // non-admin, holds a global Cube:Admin grant
    let de = login(&app, "de").await; // cube Write only
    let new_cube = json!({
        "name": "Budget",
        "dimensions": [{ "name": "D", "elements": [{ "name": "a", "kind": "numeric" }] }]
    });
    // The cube manager (not a server admin) can create a cube ...
    assert_eq!(
        send(&app, "POST", "/api/v1/cubes", &mgr, Some(new_cube)).await,
        StatusCode::OK
    );
    // ... while a data-entry user (cube Write only) cannot.
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/cubes",
            &de,
            Some(json!({ "name": "Nope", "dimensions": [{ "name": "D", "elements": [{ "name": "a", "kind": "numeric" }] }] })),
        )
        .await,
        StatusCode::FORBIDDEN
    );
}
