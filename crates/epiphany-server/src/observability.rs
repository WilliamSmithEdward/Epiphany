//! Tracing/log initialization. Structured logs; no secrets in logs (RG-13).

use tracing_subscriber::EnvFilter;

/// Initialize the global tracing subscriber from an env-filter directive.
/// Idempotent: a second call (for example in tests) is a no-op.
pub fn init(filter: &str) {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .try_init();
}
