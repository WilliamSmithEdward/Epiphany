//! M8 acceptance: the Phase 8 definition of done (ROADMAP section 6) for the
//! scheduler (ADR-0013), proven deterministically by driving the reconcile loop
//! under a `ManualClock` with no real timers.
//!
//! "A job runs on schedule" and "a server recovers cleanly from a kill": a job
//! fires when its interval is due and commits its flow's cells; it does not fire
//! before the interval; a run interrupted by a crash is recovered and the firing
//! re-derives as due; and a scheduled write survives a store reopen.
//!
//! Determinism (ADR-0009): pinned `ManualClock`, seeded `IdGen`; the loop reads
//! the clock once per tick and freezes it into the run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{
    build_router, AppState, CalcFactory, RunLedger, RunRecord, RunState, Scheduler, SessionStore,
};
use epiphany_core::{Cube, Dimension, Flow, Job, Trigger};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::Engine;
use epiphany_mdx::MdxEvaluator;
use epiphany_persist::{AutomationStore, Store};
use epiphany_security::{AuditLog, SecurityStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

// A flow with no input that writes North/Sales = 42 (proves a scheduled run
// commits cells through the engine). It targets the cube explicitly via its
// default cube (set on the global flow below).
const LOAD_FLOW: &str =
    "function rows(ctx) { ctx.writeCells([{ coord: { Region: 'North', Measure: 'Sales' }, value: '42' }]); }";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epiphany-m8-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    region.add_leaf("North");
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

/// Build an engine over a fresh store holding only the Sales cube (ADR-0035:
/// flows and jobs are no longer in the cube model).
fn engine_with_cube(dir: &Path) -> Engine {
    let store = Store::create(dir.to_path_buf(), sales_cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    Engine::from_stores(stores, Arc::new(IdGen::default()))
}

/// A global automation store (in a fresh dir) with the `load` flow (owned by the
/// admin, default cube Sales) and a `nightly` job (one step, every 1000 ms).
fn automation_with_job(name: &str) -> AutomationStore {
    let dir = scratch(&format!("auto-{name}"));
    let mut store = AutomationStore::open(dir).unwrap();
    store
        .define_flow(Flow {
            name: "load".to_string(),
            source: LOAD_FLOW.to_string(),
            owner: Some("admin".to_string()),
            default_cube: Some("Sales".to_string()),
            inputs: Vec::new(),
        })
        .unwrap();
    store
        .define_job(Job {
            name: "nightly".to_string(),
            steps: vec!["load".to_string()],
            trigger: Trigger::Interval { every_millis: 1000 },
            enabled: true,
        })
        .unwrap();
    store
}

fn state(
    engine: Engine,
    clock: Arc<ManualClock>,
    runs: RunLedger,
    automation: AutomationStore,
) -> AppState {
    AppState {
        engine: engine.clone(),
        clock,
        security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(CalcFactory::new(engine)),
        command_connectors_enabled: false,
        secure_cookies: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(runs)),
        view_cache: Default::default(),
        secrets: Default::default(),
        automation: Arc::new(Mutex::new(automation)),
        http: Default::default(),
        sql: Default::default(),
    }
}

fn north_sales(engine: &Engine) -> i64 {
    let snap = engine.snapshot("Sales").unwrap();
    let r = snap.cube().dimension(0).resolve("North").unwrap();
    let m = snap.cube().dimension(1).resolve("Sales").unwrap();
    snap.cube().get_leaf(&[r, m]).unwrap().to_scaled()
}

#[test]
fn job_fires_on_schedule_commits_and_respects_the_interval() {
    let dir = scratch("fires");
    let engine = engine_with_cube(&dir);
    let clock = Arc::new(ManualClock::new(1000));
    let st = state(
        engine.clone(),
        clock.clone(),
        RunLedger::in_memory(),
        automation_with_job("fires"),
    );
    let scheduler = Scheduler::new(st.clone());

    // Before the first tick the cell is empty.
    assert_eq!(north_sales(&engine), 0);

    // First tick at now=1000: the never-fired job is due, fires, and commits.
    assert_eq!(scheduler.tick(), 1, "the due job fires once");
    assert_eq!(
        north_sales(&engine),
        42 * 10_000,
        "the scheduled run committed"
    );
    {
        // A global job's firings carry an empty cube key (ADR-0035).
        let ledger = st.runs.lock().unwrap();
        assert_eq!(ledger.last_succeeded_fire("", "nightly"), Some(1000));
        assert_eq!(ledger.runs_for_job("", "nightly").len(), 1);
        assert_eq!(
            ledger.runs_for_job("", "nightly")[0].state,
            RunState::Succeeded
        );
    }

    // Before the interval elapses: no fire.
    clock.set(1999);
    assert_eq!(scheduler.tick(), 0, "not due before the interval");

    // At the next interval boundary: fires again.
    clock.set(2000);
    assert_eq!(scheduler.tick(), 1, "due at the next interval");
    assert_eq!(st.runs.lock().unwrap().runs_for_job("", "nightly").len(), 2);
}

#[test]
fn an_interrupted_run_recovers_and_the_firing_re_derives_as_due() {
    let dir = scratch("recover");
    let ledger_path = scratch("recover-ledger").join("runs.log");

    // Simulate a crash mid-run: a run is left Running, never succeeding. A global
    // job's firings carry an empty cube key (ADR-0035), so the run id is
    // `sched::nightly:1000`.
    {
        let mut ledger = RunLedger::open(ledger_path.clone()).unwrap();
        ledger
            .append(RunRecord {
                id: "sched::nightly:1000".to_string(),
                cube: String::new(),
                target: "nightly".to_string(),
                is_job: true,
                fire_millis: 1000,
                state: RunState::Running,
                rows_read: 0,
                cells_written: 0,
                elements_added: 0,
                error: String::new(),
                principal: "scheduler".to_string(),
            })
            .unwrap();
    }

    // Reopen: the in-flight run is recovered as Interrupted, so the job has no
    // successful fire on record.
    let recovered = RunLedger::open(ledger_path).unwrap();
    assert_eq!(
        recovered.get("sched::nightly:1000").unwrap().state,
        RunState::Interrupted
    );
    assert_eq!(recovered.last_succeeded_fire("", "nightly"), None);

    // The convergent loop re-derives the firing as due and now succeeds.
    let engine = engine_with_cube(&dir);
    let clock = Arc::new(ManualClock::new(5000));
    let st = state(
        engine.clone(),
        clock,
        recovered,
        automation_with_job("recover"),
    );
    let scheduler = Scheduler::new(st.clone());
    assert_eq!(
        scheduler.tick(),
        1,
        "the interrupted firing re-derives as due"
    );
    assert_eq!(north_sales(&engine), 42 * 10_000);
    assert_eq!(
        st.runs.lock().unwrap().last_succeeded_fire("", "nightly"),
        Some(5000)
    );
}

async fn login(app: &Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "username": "admin", "password": "pw" }).to_string(),
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
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
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

#[tokio::test]
async fn job_rest_validates_steps_runs_manually_and_lists_runs() {
    // A cube plus a global automation store holding the `load` flow but no job
    // yet, so the global schedule REST surface defines it (ADR-0035).
    let dir = scratch("rest");
    let engine = engine_with_cube(&dir);
    let auto_dir = scratch("rest-auto");
    let mut automation = AutomationStore::open(auto_dir).unwrap();
    automation
        .define_flow(Flow {
            name: "load".to_string(),
            source: LOAD_FLOW.to_string(),
            owner: Some("admin".to_string()),
            default_cube: Some("Sales".to_string()),
            inputs: Vec::new(),
        })
        .unwrap();
    let st = state(
        engine,
        Arc::new(ManualClock::new(1000)),
        RunLedger::in_memory(),
        automation,
    );
    let app = build_router(st);
    let admin = login(&app).await;

    // A job whose step names an unknown flow is rejected.
    let (status, _) = send(
        &app,
        "PUT",
        "/api/v1/schedules/bad",
        &admin,
        Some(json!({ "steps": ["ghost"], "every_millis": 1000, "enabled": true })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A valid job is stored and listed.
    let (status, _) = send(
        &app,
        "PUT",
        "/api/v1/schedules/nightly",
        &admin,
        Some(json!({ "steps": ["load"], "every_millis": 1000, "enabled": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, jobs) = send(&app, "GET", "/api/v1/schedules", &admin, None).await;
    assert_eq!(jobs["jobs"].as_array().unwrap().len(), 1);

    // A manual kick runs the job now and returns a succeeded run.
    let (status, run) = send(&app, "POST", "/api/v1/schedules/nightly/run", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["state"], "succeeded");
    let run_id = run["id"].as_str().unwrap().to_string();

    // The run is queryable by id (global) and appears in the global run list.
    let (status, fetched) =
        send(&app, "GET", &format!("/api/v1/runs/{run_id}"), &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["id"], run_id);
    let (_, runs) = send(&app, "GET", "/api/v1/runs", &admin, None).await;
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
}

#[test]
fn a_scheduled_write_survives_a_store_reopen() {
    let dir = scratch("durable");
    {
        let engine = engine_with_cube(&dir);
        let clock = Arc::new(ManualClock::new(1000));
        let st = state(
            engine.clone(),
            clock,
            RunLedger::in_memory(),
            automation_with_job("durable"),
        );
        Scheduler::new(st).tick();
        assert_eq!(north_sales(&engine), 42 * 10_000);
    }
    // Reopen the store from disk (a clean "restart"): the scheduled write is
    // durable (committed through the WAL), so it recovers.
    let store = Store::open(&dir).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let reopened = Engine::from_stores(stores, Arc::new(IdGen::default()));
    assert_eq!(north_sales(&reopened), 42 * 10_000);
}
