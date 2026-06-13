//! Graceful-shutdown signal: resolves on Ctrl-C.

/// A future that completes when the process is asked to stop. Durability does not
/// depend on a clean shutdown (the WAL is fsynced per write), so this only drains
/// in-flight requests.
pub async fn signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        // Could not install the handler; wait forever and rely on a hard kill.
        std::future::pending::<()>().await;
    }
}
