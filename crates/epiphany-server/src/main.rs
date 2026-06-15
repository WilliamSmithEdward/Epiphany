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
#[cfg(windows)]
mod service_windows;
mod shutdown;
#[cfg(feature = "tls")]
mod tls;
#[cfg(feature = "embed-ui")]
mod ui;

use std::future::Future;
use std::sync::{Arc, Mutex};

use config::Config;
use epiphany_api::{build_router, AppState, RunLedger, RunRetention, Scheduler, SessionStore};
use epiphany_determinism::SystemClock;
use epiphany_security::{AuditLog, RetentionPolicy, SecurityStore};

/// Write a one-time secret to `path`, owner-only (`0600`) **from creation** on
/// Unix so it is never briefly world-readable; elsewhere the data directory's
/// inherited ACL governs. The content is the exact secret with no trailing
/// newline (ADR-0017).
fn write_secret_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

/// Entry point. Without arguments the server runs in the foreground until
/// Ctrl-C / SIGTERM. `service install|uninstall|run` manages and hosts a native
/// Windows service (Windows only); on other platforms use systemd/launchd/Docker
/// (see docs/DEPLOYMENT.md).
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("service") {
        let sub = args.get(2).map(String::as_str).unwrap_or("");
        #[cfg(windows)]
        {
            return service_windows::handle(sub);
        }
        #[cfg(not(windows))]
        {
            let _ = sub;
            eprintln!(
                "the 'service' command is Windows-only; on this platform run under \
                 systemd, launchd, or Docker (see docs/DEPLOYMENT.md)"
            );
            std::process::exit(2);
        }
    }

    // Foreground: drain on Ctrl-C / SIGTERM.
    let config = Config::from_env();
    observability::init(&config.log_filter);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_server(config, shutdown::signal()))
}

/// Build the application state, start the scheduler, and serve (HTTP or HTTPS)
/// until `shutdown` resolves, draining in-flight requests. The caller supplies
/// the shutdown trigger: a signal future in the foreground, or the service
/// control handler's stop notification when hosted as a Windows service.
pub(crate) async fn run_server<F>(
    config: Config,
    shutdown: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: Future<Output = ()> + Send + 'static,
{
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
        // Deliver the one-time admin password via an owner-only file, never
        // stdout or the structured log (ADR-0017, RG-13). The operator reads it
        // once and deletes it.
        let pw_path = config.data_dir.join("server").join("admin-password.txt");
        match write_secret_file(&pw_path, &password.0) {
            Ok(()) => tracing::warn!(
                path = %pw_path.display(),
                "first run: the generated admin password was written to this file; read it once, then delete it"
            ),
            Err(e) => {
                // Only if the file cannot be written do we fall back to stdout,
                // so the operator is never locked out of the first login.
                tracing::error!(error = %e, "could not write the admin password file; printing it once instead");
                println!(
                    "\nFirst run: admin password:\n    {}\nChange it after you log in.\n",
                    password.0
                );
            }
        }
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
        login_guard: Arc::new(Mutex::new(epiphany_api::LoginGuard::new(
            config.login_max_failures,
            config.login_lockout_millis,
        ))),
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

    // Optional HTTPS (ADR-0019): serve TLS when configured and the `tls` feature
    // is built in; otherwise plain HTTP. The default is unchanged (HTTP).
    if config.wants_tls() {
        #[cfg(feature = "tls")]
        {
            if config.open_browser {
                tracing::info!("open https://{}/ in your browser", config.bind_addr);
            }
            tls::serve_https(
                config.bind_addr,
                app,
                config.tls_cert.clone(),
                config.tls_key.clone(),
                &config.data_dir,
                shutdown,
            )
            .await?;
            tracing::info!("shut down cleanly");
            return Ok(());
        }
        #[cfg(not(feature = "tls"))]
        tracing::warn!(
            "TLS was requested (EPIPHANY_TLS*) but this build lacks the `tls` feature; serving plain HTTP"
        );
    }

    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    let addr = listener.local_addr()?;
    tracing::info!("listening on http://{addr}/");
    if config.open_browser {
        tracing::info!("open http://{addr}/ in your browser");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    tracing::info!("shut down cleanly");
    Ok(())
}
