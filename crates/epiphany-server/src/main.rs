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
use epiphany_api::{build_router, AppState, SessionStore};
use epiphany_determinism::SystemClock;
use epiphany_security::{AuditLog, SecurityStore};

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
    let (security, generated) =
        SecurityStore::open_or_bootstrap(security_path, false, admin_override.as_deref())?;
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
    let audit = AuditLog::open(audit_path)?;

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
    };
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
