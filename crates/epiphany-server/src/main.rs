//! Epiphany server: the daemon and composition root (Phase 2).
//!
//! Zero-config startup: read `EPIPHANY_*` config over sane defaults, initialize
//! tracing, load the durable model (or materialize the bundled demo on first
//! run), build the API router, and serve on loopback until Ctrl-C.

mod boot;
mod config;
mod demo;
mod observability;
mod shutdown;

use config::Config;
use epiphany_api::{build_router, AppState};

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

    let app =
        build_router(AppState { engine }).layer(tower_http::trace::TraceLayer::new_for_http());

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
