//! Optional HTTPS serving (ADR-0019), compiled only with the `tls` feature.
//!
//! Serves the same router over TLS using `axum-server` and `rustls` (the `ring`
//! provider, pinned at build time). The certificate is either the operator's
//! (`EPIPHANY_TLS_CERT` + `EPIPHANY_TLS_KEY`) or a self-signed one generated into
//! the data directory on first run, which makes `EPIPHANY_TLS=on` a one-variable
//! way to get working HTTPS for local and internal use.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;

use crate::shutdown;

/// Serve `app` over HTTPS on `addr`. Uses the operator certificate when both
/// `cert` and `key` are set, otherwise a self-signed certificate persisted under
/// `{data_dir}/server/tls/`. Graceful shutdown is wired through the handle, so a
/// signal drains in-flight requests before exit.
pub(crate) async fn serve_https(
    addr: SocketAddr,
    app: Router,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    data_dir: &Path,
) -> std::io::Result<()> {
    // The build pins the ring provider; install it as the process default (a
    // no-op if something already did).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let tls = match (cert, key) {
        (Some(cert), Some(key)) => {
            tracing::info!(cert = %cert.display(), "TLS: serving the configured certificate");
            RustlsConfig::from_pem_file(cert, key).await?
        }
        _ => {
            let (cert_pem, key_pem) = load_or_generate_self_signed(data_dir)?;
            RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes()).await?
        }
    };

    let handle = Handle::new();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        shutdown::signal().await;
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
    });

    tracing::info!("listening on https://{addr}/");
    axum_server::bind_rustls(addr, tls)
        .handle(handle)
        .serve(app.into_make_service())
        .await
}

/// Load the persisted self-signed certificate and key, generating them owner-only
/// on first run (subject alternative names `localhost`, `127.0.0.1`, `::1`).
/// Returns `(cert_pem, key_pem)`.
fn load_or_generate_self_signed(data_dir: &Path) -> std::io::Result<(String, String)> {
    let dir = data_dir.join("server").join("tls");
    let cert_path = dir.join("self-signed-cert.pem");
    let key_path = dir.join("self-signed-key.pem");
    if cert_path.exists() && key_path.exists() {
        return Ok((
            std::fs::read_to_string(&cert_path)?,
            std::fs::read_to_string(&key_path)?,
        ));
    }
    let names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(names).map_err(std::io::Error::other)?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    std::fs::create_dir_all(&dir)?;
    // The key is a secret; the cert is public, but owner-only is harmless.
    crate::write_secret_file(&cert_path, &cert_pem)?;
    crate::write_secret_file(&key_path, &key_pem)?;
    tracing::warn!(
        dir = %dir.display(),
        "TLS: generated a self-signed certificate (browsers will warn); set EPIPHANY_TLS_CERT and EPIPHANY_TLS_KEY for a trusted certificate"
    );
    Ok((cert_pem, key_pem))
}
