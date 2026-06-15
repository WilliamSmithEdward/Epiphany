//! Graceful-shutdown signal: resolves on Ctrl-C (SIGINT) or, on Unix, SIGTERM.
//!
//! Service managers and container runtimes stop a process by sending SIGTERM
//! (systemd's default `KillSignal`, `docker stop`, launchd), so handling it is
//! what lets the server drain in-flight requests cleanly when run as a service
//! rather than being hard-killed. Durability does not depend on a clean shutdown
//! (the WAL is fsynced per write); this only drains in-flight requests.

/// A future that completes when the process is asked to stop (Ctrl-C / SIGINT
/// everywhere, plus SIGTERM on Unix).
pub async fn signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // Could not install the handler; wait forever and rely on a hard kill.
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut term) => {
                    term.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}
