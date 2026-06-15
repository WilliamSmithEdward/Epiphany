//! Server configuration: built-in defaults overridden by `EPIPHANY_*` environment.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Zero-config-friendly server settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to bind. Default `127.0.0.1:8080` (loopback only).
    pub bind_addr: SocketAddr,
    /// Directory holding the durable model. Default `./data`.
    pub data_dir: PathBuf,
    /// `tracing` env-filter directive. Default `info`.
    pub log_filter: String,
    /// Whether to hint at opening a browser after binding. Default off.
    pub open_browser: bool,
    /// Session lifetime in milliseconds. Default 8 hours.
    pub session_ttl_millis: u64,
    /// The scheduler reconcile-tick period in milliseconds (ADR-0013). Default
    /// 1000 (1s); `0` disables the scheduler loop entirely.
    pub scheduler_tick_millis: u64,
    /// The audit log's retained-record cap (ADR-0010, Phase 8). Default 100_000;
    /// `0` keeps everything.
    pub audit_max_records: usize,
    /// The audit log's retention window in milliseconds (`None` = no age limit).
    pub audit_retention_millis: Option<u64>,
    /// The run ledger's retained-run cap (ADR-0013). Default 50_000; `0` keeps
    /// everything.
    pub run_ledger_max_runs: usize,
    /// Consecutive failed logins before a username is locked out (ADR-0017).
    /// Default 5; `0` disables the lockout.
    pub login_max_failures: u32,
    /// Login lockout cooldown in milliseconds (ADR-0017). Default 15 minutes;
    /// `0` disables the lockout.
    pub login_lockout_millis: u64,
    /// Minimum length for a user-set password (ADR-0017). Default 12.
    pub password_min_length: usize,
    /// Reject common/guessable passwords (ADR-0017). Default true.
    pub password_reject_common: bool,
    /// Serve a self-signed certificate generated into the data directory
    /// (ADR-0019): the zero-config HTTPS path (`EPIPHANY_TLS=on`).
    pub tls_self_signed: bool,
    /// Path to a PEM certificate (chain) for HTTPS; with [`tls_key`] this serves
    /// the operator's own certificate and takes precedence over self-signed.
    pub tls_cert: Option<PathBuf>,
    /// Path to the PEM private key paired with [`tls_cert`].
    pub tls_key: Option<PathBuf>,
}

impl Config {
    /// Whether HTTPS should be served: an operator certificate (both cert and
    /// key) is set, or self-signed is requested (ADR-0019).
    pub fn wants_tls(&self) -> bool {
        self.tls_self_signed || (self.tls_cert.is_some() && self.tls_key.is_some())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 8080)),
            data_dir: PathBuf::from("data"),
            log_filter: "info".to_string(),
            open_browser: false,
            session_ttl_millis: 8 * 60 * 60 * 1000,
            scheduler_tick_millis: 1000,
            audit_max_records: 100_000,
            audit_retention_millis: None,
            run_ledger_max_runs: 50_000,
            login_max_failures: 5,
            login_lockout_millis: 15 * 60 * 1000,
            password_min_length: 12,
            password_reject_common: true,
            tls_self_signed: false,
            tls_cert: None,
            tls_key: None,
        }
    }
}

impl Config {
    /// Read configuration from the process environment (`EPIPHANY_*`).
    pub fn from_env() -> Self {
        let vars: BTreeMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("EPIPHANY_"))
            .collect();
        Self::from_map(&vars)
    }

    /// Build configuration from a map of `EPIPHANY_*` values over the defaults.
    /// Pure (no real environment access), so configuration precedence is testable.
    pub fn from_map(vars: &BTreeMap<String, String>) -> Self {
        let mut config = Config::default();
        if let Some(addr) = vars.get("EPIPHANY_BIND").and_then(|v| v.parse().ok()) {
            config.bind_addr = addr;
        }
        if let Some(dir) = vars.get("EPIPHANY_DATA_DIR") {
            config.data_dir = PathBuf::from(dir);
        }
        if let Some(filter) = vars.get("EPIPHANY_LOG") {
            config.log_filter = filter.clone();
        }
        if let Some(flag) = vars.get("EPIPHANY_OPEN_BROWSER") {
            config.open_browser = matches!(flag.as_str(), "1" | "true" | "yes");
        }
        if let Some(secs) = vars
            .get("EPIPHANY_SESSION_TTL_SECS")
            .and_then(|v| v.parse::<u64>().ok())
        {
            config.session_ttl_millis = secs.saturating_mul(1000);
        }
        if let Some(secs) = vars
            .get("EPIPHANY_SCHEDULER_TICK_SECS")
            .and_then(|v| v.parse::<u64>().ok())
        {
            config.scheduler_tick_millis = secs.saturating_mul(1000);
        }
        if let Some(max) = vars
            .get("EPIPHANY_AUDIT_MAX_RECORDS")
            .and_then(|v| v.parse::<usize>().ok())
        {
            config.audit_max_records = max;
        }
        if let Some(days) = vars
            .get("EPIPHANY_AUDIT_RETENTION_DAYS")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|d| *d > 0)
        {
            config.audit_retention_millis = Some(days.saturating_mul(24 * 60 * 60 * 1000));
        }
        if let Some(max) = vars
            .get("EPIPHANY_RUN_LEDGER_MAX_RUNS")
            .and_then(|v| v.parse::<usize>().ok())
        {
            config.run_ledger_max_runs = max;
        }
        if let Some(n) = vars
            .get("EPIPHANY_LOGIN_MAX_FAILURES")
            .and_then(|v| v.parse::<u32>().ok())
        {
            config.login_max_failures = n;
        }
        if let Some(secs) = vars
            .get("EPIPHANY_LOGIN_LOCKOUT_SECS")
            .and_then(|v| v.parse::<u64>().ok())
        {
            config.login_lockout_millis = secs.saturating_mul(1000);
        }
        if let Some(n) = vars
            .get("EPIPHANY_PASSWORD_MIN_LENGTH")
            .and_then(|v| v.parse::<usize>().ok())
        {
            config.password_min_length = n;
        }
        if let Some(v) = vars.get("EPIPHANY_PASSWORD_REJECT_COMMON") {
            // Any explicit off-ish value disables the common-password reject list.
            config.password_reject_common = !matches!(
                v.to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no"
            );
        }
        // TLS (ADR-0019). `EPIPHANY_TLS=on` (or self-signed/1/true/yes) serves a
        // generated self-signed cert; an explicit cert+key takes precedence.
        if let Some(v) = vars.get("EPIPHANY_TLS") {
            config.tls_self_signed = matches!(
                v.to_ascii_lowercase().as_str(),
                "on" | "self-signed" | "1" | "true" | "yes"
            );
        }
        config.tls_cert = vars.get("EPIPHANY_TLS_CERT").map(PathBuf::from);
        config.tls_key = vars.get("EPIPHANY_TLS_KEY").map(PathBuf::from);
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_loopback_8080() {
        let c = Config::default();
        assert_eq!(c.bind_addr.port(), 8080);
        assert!(c.bind_addr.ip().is_loopback());
        assert!(!c.open_browser);
    }

    #[test]
    fn env_map_overrides_defaults() {
        let mut vars = BTreeMap::new();
        vars.insert("EPIPHANY_BIND".to_string(), "127.0.0.1:9999".to_string());
        vars.insert("EPIPHANY_DATA_DIR".to_string(), "/tmp/epi".to_string());
        vars.insert("EPIPHANY_OPEN_BROWSER".to_string(), "true".to_string());
        let c = Config::from_map(&vars);
        assert_eq!(c.bind_addr.port(), 9999);
        assert_eq!(c.data_dir, PathBuf::from("/tmp/epi"));
        assert!(c.open_browser);
    }

    #[test]
    fn login_lockout_knobs_parse_with_secure_defaults() {
        // Secure defaults out of the box (ADR-0017).
        let d = Config::default();
        assert_eq!(d.login_max_failures, 5);
        assert_eq!(d.login_lockout_millis, 15 * 60 * 1000);
        // Overridable from the environment; secs are converted to millis.
        let mut vars = BTreeMap::new();
        vars.insert("EPIPHANY_LOGIN_MAX_FAILURES".to_string(), "3".to_string());
        vars.insert("EPIPHANY_LOGIN_LOCKOUT_SECS".to_string(), "60".to_string());
        let c = Config::from_map(&vars);
        assert_eq!(c.login_max_failures, 3);
        assert_eq!(c.login_lockout_millis, 60_000);
    }

    #[test]
    fn tls_is_off_by_default_and_configurable() {
        // Off out of the box (ADR-0019).
        assert!(!Config::default().wants_tls());
        // One variable enables self-signed HTTPS.
        let mut vars = BTreeMap::new();
        vars.insert("EPIPHANY_TLS".to_string(), "on".to_string());
        let c = Config::from_map(&vars);
        assert!(c.tls_self_signed);
        assert!(c.wants_tls());
        // An operator certificate is picked up from cert+key paths.
        let mut vars = BTreeMap::new();
        vars.insert(
            "EPIPHANY_TLS_CERT".to_string(),
            "/etc/epi/cert.pem".to_string(),
        );
        vars.insert(
            "EPIPHANY_TLS_KEY".to_string(),
            "/etc/epi/key.pem".to_string(),
        );
        let c = Config::from_map(&vars);
        assert!(c.wants_tls());
        assert_eq!(c.tls_cert.unwrap(), PathBuf::from("/etc/epi/cert.pem"));
        // A cert without a key is not enough to enable TLS.
        let mut vars = BTreeMap::new();
        vars.insert(
            "EPIPHANY_TLS_CERT".to_string(),
            "/etc/epi/cert.pem".to_string(),
        );
        assert!(!Config::from_map(&vars).wants_tls());
    }

    #[test]
    fn malformed_bind_falls_back_to_default() {
        let mut vars = BTreeMap::new();
        vars.insert("EPIPHANY_BIND".to_string(), "not-an-addr".to_string());
        assert_eq!(Config::from_map(&vars).bind_addr.port(), 8080);
    }
}
