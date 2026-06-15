//! Epiphany server: the daemon and composition root (Phase 2).
//!
//! Zero-config startup: read `EPIPHANY_*` config over sane defaults, initialize
//! tracing, load the durable model (or materialize the bundled demo on first
//! run), bootstrap the user store, build the API router, and serve on loopback
//! until Ctrl-C.

mod boot;
mod config;
mod demo;
mod observability;
mod shutdown;
#[cfg(feature = "embed-ui")]
mod ui;

use std::sync::{Arc, Mutex};

use config::Config;
use epiphany_api::{build_router, AppState, RunLedger, RunRetention, Scheduler, SessionStore};
use epiphany_determinism::SystemClock;
use epiphany_security::{AuditLog, RetentionPolicy, SecurityStore};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env();
    observability::init(&config.log_filter);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting Epiphany server"
    );

    let engine = boot::load_or_init(&config.data_dir)?;
    tracing::info!("model loaded: {:?}", engine.cube_names());

    // Users and password hashes, persisted separately from the cube model.
    let security_path = config.data_dir.join("server").join("security.model");
    let admin_override = std::env::var("EPIPHANY_ADMIN_PASSWORD").ok();
    let (mut security, generated) =
        SecurityStore::open_or_bootstrap(security_path, false, admin_override.as_deref())?;
    // Ungranted-cube posture (ADR-0015 decision 2a): closed unless the operator
    // opts into the trusted-single-org open mode.
    security.set_default_cube_open(config.default_cube_open);
    if config.default_cube_open {
        tracing::warn!(
            "ungranted cubes are OPEN to any authenticated user (EPIPHANY_DEFAULT_CUBE_ACCESS=open)"
        );
    }
    if let Some(password) = &generated {
        // Shown once on the operator console (not the structured log).
        println!(
            "\nFirst run: created admin user 'admin' with password:\n    {}\nChange it after you log in.\n",
            password.0
        );
    }

    // The audit stream (ADR-0010), a sibling of the security artifact. Recovery
    // is non-gating, so a damaged audit file never blocks startup.
    let audit_path = config.data_dir.join("server").join("audit.log");
    let audit = AuditLog::open_with_policy(
        audit_path,
        RetentionPolicy {
            max_records: config.audit_max_records,
            max_age_millis: config.audit_retention_millis,
        },
    )?;

    // The durable run ledger (ADR-0013), a sibling of the audit stream. Recovery
    // is non-gating and re-records any run in flight at a crash as interrupted,
    // so the reconcile loop re-derives its firing as due.
    let runs_path = config.data_dir.join("server").join("runs.log");
    let runs = RunLedger::open_with_policy(
        runs_path,
        RunRetention {
            max_runs: config.run_ledger_max_runs,
        },
    )?;

    let (events, _) = tokio::sync::broadcast::channel(256);
    // Inject the real MDX evaluator (dynamic subsets) and the rule-aware cell
    // resolver factory (calc); these are the composition-root injections.
    let cells = Arc::new(epiphany_api::CalcFactory::new(engine.clone()));
    // Command (process-execution) connectors are arbitrary code execution, so
    // they are off unless the operator explicitly opts in (ADR-0012).
    let command_connectors_enabled = std::env::var("EPIPHANY_ENABLE_COMMAND_CONNECTORS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if command_connectors_enabled {
        tracing::warn!(
            "command (process-execution) connectors are ENABLED: admin-defined connections may run host programs"
        );
    }
    let state = AppState {
        engine,
        clock: Arc::new(SystemClock),
        security: Arc::new(Mutex::new(security)),
        sessions: Arc::new(Mutex::new(SessionStore::new(config.session_ttl_millis))),
        events,
        mdx: Arc::new(epiphany_mdx::MdxEvaluator::new()),
        cells,
        command_connectors_enabled,
        audit: Arc::new(Mutex::new(audit)),
        runs: Arc::new(Mutex::new(runs)),
    };

    // Start the scheduler reconcile loop (ADR-0013) on the real clock unless it
    // is disabled (tick = 0). It is a detached background task: durability never
    // depends on a clean stop, since an interrupted run recovers on restart.
    if config.scheduler_tick_millis > 0 {
        tracing::info!(
            tick_millis = config.scheduler_tick_millis,
            "starting the job scheduler"
        );
        Scheduler::spawn(state.clone(), config.scheduler_tick_millis);
    }

    let router = build_router(state);
    #[cfg(feature = "embed-ui")]
    let router = router.fallback(ui::fallback);
    let app = router.layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    let addr = listener.local_addr()?;
    tracing::info!("listening on http://{addr}/");
    if config.open_browser {
        tracing::info!("open http://{addr}/ in your browser");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown::signal())
        .await?;
    tracing::info!("shut down cleanly");
    Ok(())
}
