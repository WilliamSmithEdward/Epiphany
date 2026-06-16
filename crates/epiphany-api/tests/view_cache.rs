//! View-cache integration tests (ADR-0028 Stage A) over the real router with the
//! rule-aware resolver. They prove the three properties the cache must hold:
//! a repeat read of an identical view is served from the cache; a write bumps
//! the cube version so the next read recomputes (read-after-write, no staleness);
//! and an element-masked principal never receives another principal's unmasked
//! cached result (fail-closed isolation, the security-critical case).
//!
//! Determinism (ADR-0009): pinned `ManualClock` and seeded `IdGen`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use epiphany_api::{build_router, AppState, CalcFactory, SessionStore, ViewCache};
use epiphany_core::{Cube, Dimension, Fixed};
use epiphany_determinism::{IdGen, ManualClock};
use epiphany_engine::{CellWrite, Engine};
use epiphany_mdx::MdxEvaluator;
use epiphany_persist::Store;
use epiphany_security::{AccessLevel, AuditLog, ObjectKind, Scope, SecurityStore, Subject};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Region(North,South,Total=N+S) x Measure(Sales,Cost) so a read crosses a
/// consolidation (the calc path), and South can be element-restricted.
fn cube() -> Cube {
    let mut region = Dimension::new("Region");
    let n = region.add_leaf("North");
    let s = region.add_leaf("South");
    let t = region.add_consolidated("Total");
    region.add_child(t, n, 1).unwrap();
    region.add_child(t, s, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    measure.add_leaf("Cost");
    Cube::new("Sales", vec![region, measure]).unwrap()
}

struct Harness {
    app: Router,
    engine: Engine,
    security: Arc<Mutex<SecurityStore>>,
    cache: Arc<ViewCache>,
}

fn harness(name: &str) -> Harness {
    let dir =
        std::env::temp_dir().join(format!("epiphany-viewcache-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = Store::create(dir, cube()).unwrap();
    let mut stores = BTreeMap::new();
    stores.insert("Sales".to_string(), store);
    let engine = Engine::from_stores(stores, Arc::new(IdGen::default()));

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

    let mut sec = SecurityStore::with_admin("admin", "pw", true);
    sec.create_user("ann", "pw", false).unwrap();
    sec.create_user("bob", "pw", false).unwrap();
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
    let cache: Arc<ViewCache> = Arc::new(ViewCache::new(256));

    let state = AppState {
        engine: engine.clone(),
        clock: Arc::new(ManualClock::new(1_000)),
        security: security.clone(),
        sessions: Arc::new(Mutex::new(SessionStore::new(60_000))),
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(5, 900_000))),
        events: tokio::sync::broadcast::channel(16).0,
        mdx: Arc::new(MdxEvaluator::new()),
        cells: Arc::new(CalcFactory::new(engine.clone())),
        command_connectors_enabled: false,
        audit: Arc::new(Mutex::new(AuditLog::in_memory())),
        runs: Arc::new(Mutex::new(epiphany_api::RunLedger::in_memory())),
        view_cache: cache.clone(),
    };
    Harness {
        app: build_router(state),
        engine,
        security,
        cache,
    }
}

/// Restrict Region/South to `bob` only (granting bob Read makes the member
/// managed, denying every other non-admin).
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

async fn cellset(app: &Router, token: &str, spec: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/cubes/Sales/cellset")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(spec.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

/// A view over Region(North,South,Total) on rows and Measure(Sales) on columns.
fn region_by_sales() -> Value {
    json!({
        "rows": [
            { "dimension": "Region", "type": "members", "members": ["North", "South", "Total"] }
        ],
        "columns": [
            { "dimension": "Measure", "type": "members", "members": ["Sales"] }
        ]
    })
}

fn row_names(cs: &Value) -> Vec<String> {
    cs["row_tuples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t[0]["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn repeat_read_is_served_from_cache() {
    let h = harness("repeat");
    let t = login(&h.app, "admin").await;

    let (s1, cs1) = cellset(&h.app, &t, region_by_sales()).await;
    assert_eq!(s1, StatusCode::OK);
    // Total Sales rolls up North(100)+South(200)=300.
    assert_eq!(cs1["cells"][2]["value"], "300");
    assert_eq!(h.cache.misses(), 1, "first read is a miss");
    assert_eq!(h.cache.hits(), 0);

    let (s2, cs2) = cellset(&h.app, &t, region_by_sales()).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(cs1, cs2, "the cached read is identical");
    assert_eq!(h.cache.hits(), 1, "second identical read is a hit");
    assert_eq!(h.cache.misses(), 1, "no second compute");
}

#[tokio::test]
async fn write_bumps_version_and_recomputes() {
    let h = harness("invalidate");
    let t = login(&h.app, "admin").await;

    let (_, before) = cellset(&h.app, &t, region_by_sales()).await;
    assert_eq!(before["cells"][2]["value"], "300", "Total Sales before");

    // Write North/Sales 100 -> 500 directly through the engine (same commit path
    // a REST write takes); this bumps the cube version.
    let snap = h.engine.snapshot("Sales").unwrap();
    let region = |m: &str| snap.cube().dimension(0).resolve(m).unwrap();
    let measure = |m: &str| snap.cube().dimension(1).resolve(m).unwrap();
    h.engine
        .apply_batch(
            "Sales",
            None,
            &[CellWrite::Leaf {
                coord: vec![region("North"), measure("Sales")],
                value: Fixed::from(500),
            }],
        )
        .unwrap();

    let (_, after) = cellset(&h.app, &t, region_by_sales()).await;
    // Read-after-write: the new version misses the stale entry and recomputes.
    assert_eq!(after["cells"][0]["value"], "500", "North/Sales after");
    assert_eq!(after["cells"][2]["value"], "700", "Total Sales after");
    assert_eq!(h.cache.misses(), 2, "the post-write read recomputed");
}

#[tokio::test]
async fn masked_principal_never_gets_an_unmasked_cached_result() {
    let h = harness("isolation");
    restrict_south_to_bob(&h.security);

    // Admin (no mask) reads first and populates the shared, unmasked entry.
    let admin = login(&h.app, "admin").await;
    let (_, admin_cs) = cellset(&h.app, &admin, region_by_sales()).await;
    assert_eq!(
        row_names(&admin_cs),
        vec!["North", "South", "Total"],
        "admin sees every member"
    );

    // Ann is denied South, so South is suppressed and Total (rolling up South) is
    // denied. The cache must NOT serve her the admin's three-row cellset.
    let ann = login(&h.app, "ann").await;
    let (s_ann, ann_cs) = cellset(&h.app, &ann, region_by_sales()).await;
    assert_eq!(s_ann, StatusCode::OK);
    assert_eq!(
        row_names(&ann_cs),
        vec!["North"],
        "the masked principal sees only the permitted member, not the cached unmasked result"
    );

    // Bob may see South, so he gets the full rollup (a distinct, correct entry).
    let bob = login(&h.app, "bob").await;
    let (_, bob_cs) = cellset(&h.app, &bob, region_by_sales()).await;
    assert_eq!(
        row_names(&bob_cs),
        vec!["North", "South", "Total"],
        "the permitted principal sees every member"
    );

    // A repeat admin read still hits the shared entry (it was never clobbered).
    let (_, admin_again) = cellset(&h.app, &admin, region_by_sales()).await;
    assert_eq!(admin_cs, admin_again);
    assert!(h.cache.hits() >= 1, "the shared unmasked entry is reusable");
}
