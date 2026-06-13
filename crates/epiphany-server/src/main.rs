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

use std::sync::{Arc, Mutex};

use config::Config;
use epiphany_api::{build_router, AppState, SessionStore};
use epiphany_determinism::SystemClock;
use epiphany_security::SecurityStore;

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

    let (events, _) = tokio::sync::broadcast::channel(256);
    let state = AppState {
        engine,
        clock: Arc::new(SystemClock),
        security: Arc::new(Mutex::new(security)),
        sessions: Arc::new(Mutex::new(SessionStore::new(config.session_ttl_millis))),
        events,
    };
    let app = build_router(state).layer(tower_http::trace::TraceLayer::new_for_http());

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
