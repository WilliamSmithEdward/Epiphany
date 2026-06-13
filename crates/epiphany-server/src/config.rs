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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 8080)),
            data_dir: PathBuf::from("data"),
            log_filter: "info".to_string(),
            open_browser: false,
            session_ttl_millis: 8 * 60 * 60 * 1000,
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
    fn malformed_bind_falls_back_to_default() {
        let mut vars = BTreeMap::new();
        vars.insert("EPIPHANY_BIND".to_string(), "not-an-addr".to_string());
        assert_eq!(Config::from_map(&vars).bind_addr.port(), 8080);
    }
}
