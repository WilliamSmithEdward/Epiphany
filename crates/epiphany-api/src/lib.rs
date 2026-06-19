//! Epiphany API: the REST + WebSocket surface (Axum).
//!
//! [`build_router`] assembles the router from an [`AppState`]; the server binary
//! and every integration test use the same builder, so tested behavior matches
//! served behavior. Responses are clean modern JSON (not OData); errors use the
//! shared [`ApiError`] envelope. Every route except `/healthz` and the login
//! endpoint requires a valid session (the [`auth::AuthPrincipal`] extractor).

// The hand-authored OpenAPI document (openapi.rs) is one large `json!` literal;
// its expansion needs a higher macro recursion limit than the default.
#![recursion_limit = "512"]

use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderValue};
use axum::middleware::map_response;
use axum::response::Response;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::Serialize;

use epiphany_core::SetEvaluator;
use epiphany_determinism::Clock;
use epiphany_engine::{CellResolverFactory, Engine};
use epiphany_persist::AutomationStore;
use epiphany_security::{AuditLog, SecretStore, SecurityStore};
use tokio::sync::broadcast;

mod auth;
mod authz;
mod calc_factory;
mod connection_routes;
mod dimension_routes;
mod dto;
mod error;
mod flow_reader;
mod flow_routes;
mod http_connector;
mod job_routes;
mod login_guard;
mod model_routes;
mod openapi;
mod overview_routes;
mod query_routes;
mod resolve;
mod routes;
mod rule_routes;
mod sandbox_routes;
mod scheduler;
mod secret_routes;
mod security_routes;
mod session;
mod view_cache;
mod ws;

pub use calc_factory::CalcFactory;
pub use epiphany_flow::{RunLedger, RunRecord, RunRetention, RunState};
pub use error::ApiError;
pub use login_guard::LoginGuard;
pub use scheduler::Scheduler;
pub use session::SessionStore;
pub use view_cache::ViewCache;
pub use ws::ChangeEvent;

/// Stable crate identifier.
pub const CRATE: &str = "epiphany-api";

/// Shared application state, cheap to clone into every handler.
#[derive(Clone)]
pub struct AppState {
    /// The concurrent cube engine (snapshot reads, atomic batch commits).
    pub engine: Engine,
    /// The injected clock (real in production, manual in tests).
    pub clock: Arc<dyn Clock>,
    /// Users, groups, and password hashes.
    pub security: Arc<Mutex<SecurityStore>>,
    /// Live session tokens (in memory; lost on restart, by design).
    pub sessions: Arc<Mutex<SessionStore>>,
    /// Per-username login lockout against brute-forcing (ADR-0017; in memory).
    pub login_guard: Arc<Mutex<LoginGuard>>,
    /// Broadcaster of change events to WebSocket clients.
    pub events: broadcast::Sender<ChangeEvent>,
    /// The MDX set evaluator for dynamic subsets (the composition-root injects a
    /// real one; tests inject a `NoSetEvaluator` or the real evaluator as needed).
    pub mdx: Arc<dyn SetEvaluator + Send + Sync>,
    /// The value-resolver factory: builds a per-query resolver over a pinned
    /// snapshot. The server injects a rule-aware [`CalcFactory`]; no-rules tests
    /// inject the engine's `StoredCellsFactory`.
    pub cells: Arc<dyn CellResolverFactory>,
    /// Whether command (process-execution) connections may be defined and run.
    /// Off unless the operator opts in (ADR-0012 decision 6); the second gate,
    /// after admin-only definition.
    pub command_connectors_enabled: bool,
    /// Whether to mark the session cookie `Secure` (HTTPS-only). Set when the
    /// server serves over TLS (ADR-0019); off for plain-HTTP loopback so the dev
    /// cookie is still accepted by the browser.
    pub secure_cookies: bool,
    /// The append-only audit stream (ADR-0010), behind its own lock so audit
    /// writes do not serialize behind the security mutex.
    pub audit: Arc<Mutex<AuditLog>>,
    /// The durable run ledger (ADR-0013): scheduled and submitted flow runs,
    /// behind its own lock. Recovered on startup; an in-flight run at a crash is
    /// re-derived as due by the reconcile loop.
    pub runs: Arc<Mutex<RunLedger>>,
    /// The bounded, version-keyed view (cellset) cache (ADR-0028). Read-through
    /// over view execution; keyed so a cached entry is only served for an
    /// identical read. Shared across the cheap `AppState` clones.
    pub view_cache: Arc<ViewCache>,
    /// The operator secret store (ADR-0030): named credentials HTTP connections
    /// reference by name. Values are write-only over the API and never reach the
    /// model, logs, or audit.
    pub secrets: Arc<Mutex<SecretStore>>,
    /// HTTP connector capability and SSRF host allowlist (ADR-0030). Fail-closed:
    /// disabled with an empty allowlist unless the operator opts in.
    pub http: HttpConnectorConfig,
    /// SQL connector capability and host allowlist (ADR-0034). Fail-closed:
    /// disabled with an empty allowlist unless the operator opts in.
    pub sql: SqlConnectorConfig,
    /// The server-global automation store (ADR-0035): flows, flow tests,
    /// connections, and jobs, owned by no cube. Held behind its own lock,
    /// separate from the cube engine, and persisted under `{data_dir}/automation`.
    pub automation: Arc<Mutex<AutomationStore>>,
}

/// The HTTP connector capability and its SSRF host allowlist (ADR-0030).
/// Fail-closed by default: disabled with an empty allowlist, so enabling the
/// capability is not enough on its own; the operator must also name the hosts.
#[derive(Clone, Debug, Default)]
pub struct HttpConnectorConfig {
    /// Whether HTTP connections may be defined and run.
    pub enabled: bool,
    /// Lowercased hostnames an HTTP connection may target. Empty allows nothing.
    pub allowed_hosts: Vec<String>,
}

impl HttpConnectorConfig {
    /// Whether `host` (case-insensitive) is allowlisted.
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.allowed_hosts.iter().any(|h| h == &host)
    }
}

/// The SQL connector capability and its host allowlist (ADR-0034). Fail-closed
/// by default, like the HTTP connector: disabled with an empty allowlist, so
/// enabling the capability is not enough on its own; the operator must also name
/// the database hosts a connection may target.
#[derive(Clone, Debug, Default)]
pub struct SqlConnectorConfig {
    /// Whether SQL connections may be defined and run.
    pub enabled: bool,
    /// Lowercased hostnames a SQL connection may target. Empty allows nothing.
    pub allowed_hosts: Vec<String>,
}

impl SqlConnectorConfig {
    /// Whether `host` (case-insensitive) is allowlisted.
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.allowed_hosts.iter().any(|h| h == &host)
    }
}

impl AppState {
    /// The injected set evaluator as a plain trait reference (drops the auto
    /// traits the core query functions do not require).
    pub(crate) fn evaluator(&self) -> &dyn SetEvaluator {
        self.mdx.as_ref()
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

/// An explicit cap on the request body size (ADR-0018), bounding per-request
/// memory while staying generous for batch writes and flow imports.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// The Content-Security-Policy applied to every response (ADR-0018). Same-origin
/// only: the embedded single-page UI loads its own bundle and talks only to this
/// origin (REST and the same-origin WebSocket), so a strict policy holds without
/// inline scripts. `style-src` allows inline styles, which the UI injects at
/// runtime. Operators fronting the app differently can relax it.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
     frame-ancestors 'none'; base-uri 'self'; form-action 'self'";

/// Add defensive headers to every response (ADR-0018): disable MIME sniffing,
/// forbid framing (anti-clickjacking), minimize referrer leakage, keep HTTPS
/// clients on HTTPS, and a same-origin Content-Security-Policy.
async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    response
}

/// Build the application router. Used by both the server binary and tests, so
/// what is tested is what is served.
pub fn build_router(state: AppState) -> Router {
    // Protected routes require a valid session via the AuthPrincipal extractor.
    let protected = Router::new()
        .route(
            "/api/v1/cubes",
            get(list_cubes).post(model_routes::create_cube),
        )
        .route("/api/v1/cubes/{cube}", get(routes::get_cube))
        // Shared dimension library (ADR-0024): reusable dimensions referenced by
        // cubes; growing one fans out to every cube that references it.
        .route(
            "/api/v1/dimensions",
            get(dimension_routes::list_dimensions).post(dimension_routes::register_dimension),
        )
        .route(
            "/api/v1/dimensions/{id}",
            get(dimension_routes::get_dimension).delete(dimension_routes::delete_dimension),
        )
        .route(
            "/api/v1/dimensions/{id}/elements",
            post(dimension_routes::grow_dimension),
        )
        // Structural edit of a registry dimension by id (ADR-0036); fans out to
        // every referencing cube.
        .route(
            "/api/v1/dimensions/{id}/edit",
            post(dimension_routes::edit_dimension_by_id),
        )
        // Promote a cube's embedded dimension into the global registry (ADR-0031).
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/promote",
            post(dimension_routes::promote_dimension),
        )
        // Model editing (ADR-0021): add members/edges and define/set attributes.
        .route(
            "/api/v1/cubes/{cube}/elements",
            post(model_routes::add_elements),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}",
            put(model_routes::define_attribute),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/attributes/{attr}/values",
            put(model_routes::set_attribute_values),
        )
        // Structural edit of a cube's dimension (ADR-0036): reorder, reparent, set
        // kind, delete, or insert, remapping the cube's stored cells.
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/edit",
            post(model_routes::edit_cube_dimension),
        )
        .route("/api/v1/cubes/{cube}/cells/read", post(routes::read_cells))
        .route("/api/v1/cubes/{cube}/cell", put(routes::write_cell))
        .route(
            "/api/v1/cubes/{cube}/cells/batch",
            post(routes::batch_write),
        )
        .route(
            "/api/v1/cubes/{cube}/cells/spread",
            post(routes::spread_cells),
        )
        // Subsets (dimension-scoped).
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets",
            get(query_routes::list_subsets).post(query_routes::create_subset),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/preview",
            post(query_routes::preview_subset),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/mdx/preview",
            post(query_routes::preview_mdx),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/{name}",
            get(query_routes::get_subset)
                .put(query_routes::replace_subset)
                .delete(query_routes::delete_subset),
        )
        .route(
            "/api/v1/cubes/{cube}/dimensions/{dim}/subsets/{name}/members",
            get(query_routes::subset_members),
        )
        // Views (cube-scoped).
        .route(
            "/api/v1/cubes/{cube}/views",
            get(query_routes::list_views).post(query_routes::create_view),
        )
        .route(
            "/api/v1/cubes/{cube}/views/{name}",
            get(query_routes::get_view)
                .put(query_routes::replace_view)
                .delete(query_routes::delete_view),
        )
        .route(
            "/api/v1/cubes/{cube}/views/{name}/execute",
            post(query_routes::execute_saved_view),
        )
        .route(
            "/api/v1/cubes/{cube}/cellset",
            post(query_routes::execute_adhoc),
        )
        .route("/api/v1/cubes/{cube}/mdx", post(query_routes::execute_mdx))
        // Rules (cube-scoped) and the calc affordances.
        .route(
            "/api/v1/cubes/{cube}/rules",
            get(rule_routes::get_rules)
                .put(rule_routes::put_rules)
                .delete(rule_routes::delete_rules),
        )
        .route(
            "/api/v1/cubes/{cube}/rules/preview",
            post(rule_routes::preview_rules),
        )
        .route(
            "/api/v1/cubes/{cube}/cells/explain",
            post(rule_routes::explain_cell),
        )
        .route(
            "/api/v1/cubes/{cube}/feeders/diagnostics",
            get(rule_routes::feeder_diagnostics),
        )
        .route(
            "/api/v1/cubes/{cube}/rules/tests",
            get(rule_routes::list_rule_tests).post(rule_routes::put_rule_test),
        )
        .route(
            "/api/v1/cubes/{cube}/rules/tests/run",
            post(rule_routes::run_rule_tests_handler),
        )
        .route(
            "/api/v1/cubes/{cube}/rules/tests/{name}",
            axum::routing::delete(rule_routes::delete_rule_test),
        )
        // Flows (TypeScript ETL) are server-global (ADR-0035): a flow's body names
        // the cubes and dimensions it acts on. Static sub-paths (preview/tests)
        // take precedence over the `{name}` route, so "preview"/"tests" are
        // reserved flow names. The guided CSV import stays cube-scoped (it loads
        // one named cube).
        .route("/api/v1/flows", get(flow_routes::list_flows))
        .route("/api/v1/flows/preview", post(flow_routes::preview_flow))
        .route("/api/v1/cubes/{cube}/import", post(flow_routes::import_csv))
        .route(
            "/api/v1/flows/tests",
            get(flow_routes::list_flow_tests).post(flow_routes::put_flow_test),
        )
        .route(
            "/api/v1/flows/tests/run",
            post(flow_routes::run_flow_tests_handler),
        )
        .route(
            "/api/v1/flows/tests/{name}",
            axum::routing::delete(flow_routes::delete_flow_test),
        )
        .route(
            "/api/v1/flows/{name}",
            get(flow_routes::get_flow)
                .put(flow_routes::put_flow)
                .delete(flow_routes::delete_flow),
        )
        .route(
            "/api/v1/flows/{name}/run",
            post(flow_routes::run_flow_handler),
        )
        // Scheduled jobs (ADR-0013) are server-global (ADR-0035), exposed as
        // "schedules": CRUD, a manual async kick, and the global run queries.
        .route("/api/v1/schedules", get(job_routes::list_jobs))
        .route(
            "/api/v1/schedules/{name}",
            get(job_routes::get_job)
                .put(job_routes::put_job)
                .delete(job_routes::delete_job),
        )
        .route("/api/v1/schedules/{name}/run", post(job_routes::run_job))
        // Data-source connections (admin-defined; command kind also requires the
        // server opt-in) are server-global (ADR-0035).
        .route(
            "/api/v1/connections",
            get(connection_routes::list_connections),
        )
        .route(
            "/api/v1/connections/{name}",
            get(connection_routes::get_connection)
                .put(connection_routes::put_connection)
                .delete(connection_routes::delete_connection),
        )
        .route(
            "/api/v1/connections/{name}/preview",
            post(connection_routes::preview_connection),
        )
        // Sandboxes (per-user what-if overlays, ADR-0014). Data endpoints select
        // one with the X-Epiphany-Sandbox header; lifecycle is by path here.
        .route(
            "/api/v1/cubes/{cube}/sandboxes",
            get(sandbox_routes::list_sandboxes).post(sandbox_routes::create_sandbox),
        )
        .route(
            "/api/v1/cubes/{cube}/sandboxes/{name}",
            get(sandbox_routes::get_sandbox).delete(sandbox_routes::delete_sandbox),
        )
        .route(
            "/api/v1/cubes/{cube}/sandboxes/{name}/commit",
            post(sandbox_routes::commit_sandbox),
        )
        // Security administration (admin only, ADR-0015 + ADR-0010).
        .route(
            "/api/v1/users",
            get(security_routes::list_users).post(security_routes::create_user),
        )
        .route(
            "/api/v1/users/{username}",
            patch(security_routes::patch_user).delete(security_routes::delete_user),
        )
        .route(
            "/api/v1/users/{username}/reset-password",
            post(security_routes::reset_user_password),
        )
        .route(
            "/api/v1/groups",
            get(security_routes::list_groups).post(security_routes::create_group),
        )
        .route(
            "/api/v1/groups/{name}",
            delete(security_routes::delete_group),
        )
        .route(
            "/api/v1/acl/elements",
            get(security_routes::list_element_acls).put(security_routes::put_element_acl),
        )
        .route(
            "/api/v1/acl/grants",
            get(security_routes::list_grants).put(security_routes::put_grant),
        )
        .route("/api/v1/runs", get(job_routes::list_all_runs))
        .route("/api/v1/runs/{id}", get(job_routes::get_run))
        .route("/api/v1/overview", get(overview_routes::overview))
        .route("/api/v1/secrets", get(secret_routes::list_secrets))
        .route(
            "/api/v1/secrets/{name}",
            put(secret_routes::put_secret).delete(secret_routes::delete_secret),
        )
        .route("/api/v1/audit", get(security_routes::query_audit))
        .route("/api/v1/ws", get(ws::ws))
        .route("/api/v1/auth/me", get(auth::me))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/auth/password", post(auth::change_password));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/openapi.json", get(openapi::openapi_json))
        .route("/api/v1/auth/login", post(auth::login))
        .merge(protected)
        .fallback(not_found)
        .with_state(state)
        // Defensive HTTP-surface hardening (ADR-0018), applied to every route so
        // the server and tests share it: security headers and a body-size cap.
        .layer(map_response(security_headers))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// One cube in the cube list.
#[derive(Debug, Serialize)]
pub struct CubeSummary {
    pub name: String,
    pub rank: usize,
    pub cell_count: usize,
    pub string_cell_count: usize,
}

#[derive(Serialize)]
struct CubeListResponse {
    cubes: Vec<CubeSummary>,
}

async fn list_cubes(
    auth: auth::AuthPrincipal,
    State(state): State<AppState>,
) -> Json<CubeListResponse> {
    let cubes = state
        .engine
        .cube_names()
        .into_iter()
        // A cube is listed only if the caller may at least read it (ADR-0015).
        .filter(|name| {
            authz::cube_level(&state, &auth.principal.username, name)
                >= epiphany_security::AccessLevel::Read
        })
        .map(|name| {
            let (rank, cell_count, string_cell_count) = state
                .engine
                .snapshot(&name)
                .map(|s| {
                    (
                        s.cube().rank(),
                        s.cube().cell_count(),
                        s.cube().string_cell_count(),
                    )
                })
                .unwrap_or((0, 0, 0));
            CubeSummary {
                name,
                rank,
                cell_count,
                string_cell_count,
            }
        })
        .collect();
    Json(CubeListResponse { cubes })
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use epiphany_core::{Cube, Dimension};
    use epiphany_determinism::{IdGen, ManualClock};
    use epiphany_engine::Engine;
    use epiphany_persist::Store;
    use http_body_util::BodyExt;
    use std::collections::BTreeMap;

    const TTL: u64 = 60_000;

    fn test_state_with_clock(name: &str, clock: Arc<dyn Clock>) -> AppState {
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        let cube = Cube::new("Sales", vec![region]).unwrap();
        let dir = std::env::temp_dir().join(format!("epiphany-api-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::create(dir, cube).unwrap();
        let mut stores = BTreeMap::new();
        stores.insert("Sales".to_string(), store);
        let automation_dir =
            std::env::temp_dir().join(format!("epiphany-api-auto-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&automation_dir).ok();
        AppState {
            engine: Engine::from_stores(stores, Arc::new(IdGen::default())),
            clock,
            security: Arc::new(Mutex::new(SecurityStore::with_admin("admin", "pw", true))),
            sessions: Arc::new(Mutex::new(SessionStore::new(TTL))),
            login_guard: Arc::new(Mutex::new(LoginGuard::new(5, 900_000))),
            events: broadcast::channel(16).0,
            mdx: Arc::new(epiphany_core::NoSetEvaluator),
            cells: Arc::new(epiphany_engine::StoredCellsFactory),
            command_connectors_enabled: false,
            secure_cookies: false,
            audit: Arc::new(Mutex::new(AuditLog::in_memory())),
            runs: Arc::new(Mutex::new(RunLedger::in_memory())),
            view_cache: Default::default(),
            secrets: Default::default(),
            http: HttpConnectorConfig::default(),
            sql: SqlConnectorConfig::default(),
            automation: Arc::new(Mutex::new(AutomationStore::open(automation_dir).unwrap())),
        }
    }

    fn test_state(name: &str) -> AppState {
        test_state_with_clock(name, Arc::new(ManualClock::new(1_000)))
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn login(router: &Router, user: &str, pass: &str) -> (StatusCode, serde_json::Value) {
        let body = serde_json::json!({ "username": user, "password": pass }).to_string();
        let resp = router
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_is_public() {
        let app = build_router(test_state("healthz"));
        let resp = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn login_then_access_protected_route() {
        let app = build_router(test_state("login-ok"));

        let (status, json) = login(&app, "admin", "pw").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["user"]["is_admin"], true);
        let token = json["token"].as_str().unwrap().to_string();

        // With the bearer token, the protected cube list is reachable.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/v1/cubes")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["cubes"][0]["name"], "Sales");
    }

    #[tokio::test]
    async fn bad_credentials_are_401() {
        let app = build_router(test_state("login-bad"));
        let (status, json) = login(&app, "admin", "wrong").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    }

    #[tokio::test]
    async fn protected_route_without_token_is_401() {
        let app = build_router(test_state("no-token"));
        let resp = app
            .oneshot(Request::get("/api/v1/cubes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logout_revokes_the_session() {
        let app = build_router(test_state("logout"));
        let (_, json) = login(&app, "admin", "pw").await;
        let token = json["token"].as_str().unwrap().to_string();
        let auth = format!("Bearer {token}");

        let logout = app
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/logout")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::NO_CONTENT);

        // The revoked token no longer authenticates.
        let resp = app
            .oneshot(
                Request::get("/api/v1/auth/me")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn session_expires_after_ttl() {
        // Keep a typed handle to the manual clock so the test can advance it.
        let manual = Arc::new(ManualClock::new(1_000));
        let app = build_router(test_state_with_clock("expiry", manual.clone()));
        let (_, json) = login(&app, "admin", "pw").await;
        let token = json["token"].as_str().unwrap().to_string();

        // Advance past the TTL: the token is now rejected.
        manual.advance(TTL + 1);

        let resp = app
            .oneshot(
                Request::get("/api/v1/auth/me")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
