//! Server configuration: built-in defaults overridden by `EPIPHANY_*` environment.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

/// A diagnostic produced while reading `EPIPHANY_*` configuration.
///
/// Parsing is intentionally lenient for most knobs (an unparseable value keeps
/// the safe default) but must not do so *silently* — an operator who wrote a
/// value expects it to take effect. Every present-but-unusable value yields a
/// [`ConfigIssue`] the caller logs after tracing is initialized.
///
/// A `fatal` issue additionally aborts startup: it is reserved for a
/// security-critical knob where guessing either direction is wrong. Today the
/// only fatal case is an unrecognized [`EPIPHANY_TLS`](Config::from_map) value —
/// silently disabling TLS would serve plaintext to an operator who asked for
/// HTTPS (fail-open), and silently enabling it could equally surprise, so we
/// fail closed and make the operator fix the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    /// The offending environment variable (e.g. `EPIPHANY_BIND`).
    pub var: String,
    /// A human-readable explanation of why the value was rejected.
    pub message: String,
    /// When true, startup must abort rather than fall back to a default.
    pub fatal: bool,
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.var, self.message)
    }
}

/// Parse a boolean `EPIPHANY_*` flag with one grammar across the whole binary.
///
/// Truthy: `1`/`true`/`yes`/`on`. Falsy: `0`/`false`/`no`/`off`. Case- and
/// whitespace-insensitive. Returns `None` for anything else so the caller can
/// decide how to treat an unrecognized value (warn-and-default vs. hard error).
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

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
    /// Idle-timeout window in milliseconds (ADR-0017): a session with no activity
    /// for this long expires before its absolute TTL. `None` disables idle expiry.
    /// Default 30 minutes.
    pub session_idle_millis: Option<u64>,
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
    /// The view (cellset) cache's saved-view entry cap (ADR-0028). Default 256;
    /// `0` disables the cache. The ad-hoc pool is a fraction of this.
    pub view_cache_entries: usize,
    /// Serve a self-signed certificate generated into the data directory
    /// (ADR-0019): the zero-config HTTPS path (`EPIPHANY_TLS=on`).
    pub tls_self_signed: bool,
    /// Path to a PEM certificate (chain) for HTTPS; with [`tls_key`] this serves
    /// the operator's own certificate and takes precedence over self-signed.
    pub tls_cert: Option<PathBuf>,
    /// Path to the PEM private key paired with [`tls_cert`].
    pub tls_key: Option<PathBuf>,
    /// Force the session cookie's `Secure` attribute on or off, overriding the
    /// default (derived from whether HTTPS is actually served). `None` = derive.
    /// Set `EPIPHANY_SECURE_COOKIES=1` when TLS is terminated at a reverse proxy
    /// and this backend serves plain HTTP (ADR-0018 / DEPLOYMENT.md).
    pub secure_cookies_override: Option<bool>,
}

impl Config {
    /// Whether HTTPS should be served: an operator certificate (both cert and
    /// key) is set, or self-signed is requested (ADR-0019).
    pub fn wants_tls(&self) -> bool {
        self.tls_self_signed || (self.tls_cert.is_some() && self.tls_key.is_some())
    }

    /// Whether the session cookie should carry the `Secure` attribute.
    ///
    /// `serving_tls` is whether the process is *actually* serving HTTPS on this
    /// connection (which differs from [`wants_tls`](Self::wants_tls) when a build
    /// lacks the `tls` feature and falls back to plain HTTP). The explicit
    /// `EPIPHANY_SECURE_COOKIES` override wins so a TLS-terminating reverse proxy
    /// fronting plain HTTP can still mark cookies `Secure`; otherwise the flag
    /// follows what is truly on the wire, so a `Secure` cookie is never set over
    /// a plaintext non-localhost origin (which browsers refuse to store).
    pub fn secure_cookies(&self, serving_tls: bool) -> bool {
        self.secure_cookies_override.unwrap_or(serving_tls)
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
            session_idle_millis: Some(30 * 60 * 1000),
            scheduler_tick_millis: 1000,
            audit_max_records: 100_000,
            audit_retention_millis: None,
            run_ledger_max_runs: 50_000,
            login_max_failures: 5,
            login_lockout_millis: 15 * 60 * 1000,
            password_min_length: 12,
            password_reject_common: true,
            view_cache_entries: 256,
            tls_self_signed: false,
            tls_cert: None,
            tls_key: None,
            secure_cookies_override: None,
        }
    }
}

impl Config {
    /// The `tracing` env-filter directive alone, read directly from the
    /// environment so tracing can be initialized *before* the full config parse
    /// (whose diagnostics are emitted through tracing). Falls back to the default
    /// filter when unset.
    pub fn log_filter_from_env() -> String {
        std::env::var("EPIPHANY_LOG").unwrap_or_else(|_| Config::default().log_filter)
    }

    /// Read configuration from the process environment (`EPIPHANY_*`), logging a
    /// warning for every present-but-unusable value.
    ///
    /// Returns `Err` (with the offending variables) when a *fatal* issue is
    /// present so the caller can abort startup rather than serve a surprising
    /// configuration — see [`ConfigIssue`]. Non-fatal issues are logged here (so
    /// they must be called after tracing is initialized) and parsing proceeds
    /// with the safe default for each rejected value.
    pub fn from_env() -> Result<Self, Vec<ConfigIssue>> {
        let vars: BTreeMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("EPIPHANY_"))
            .collect();
        let (config, issues) = Self::from_map(&vars);
        let fatal: Vec<ConfigIssue> = issues.iter().filter(|i| i.fatal).cloned().collect();
        for issue in &issues {
            if issue.fatal {
                tracing::error!(var = %issue.var, "{}", issue.message);
            } else {
                tracing::warn!(var = %issue.var, "{}", issue.message);
            }
        }
        if fatal.is_empty() {
            Ok(config)
        } else {
            Err(fatal)
        }
    }

    /// Build configuration from a map of `EPIPHANY_*` values over the defaults.
    /// Pure (no real environment access), so configuration precedence is testable.
    ///
    /// Returns the resolved config plus a [`ConfigIssue`] per present-but-unusable
    /// value: an unparseable non-fatal knob keeps its default (the issue is a
    /// warning), while an unrecognized `EPIPHANY_TLS` value is fatal (fail-closed,
    /// see [`ConfigIssue`]).
    pub fn from_map(vars: &BTreeMap<String, String>) -> (Self, Vec<ConfigIssue>) {
        let mut config = Config::default();
        let mut issues: Vec<ConfigIssue> = Vec::new();

        if let Some(raw) = vars.get("EPIPHANY_BIND") {
            match raw.parse::<SocketAddr>() {
                Ok(addr) => config.bind_addr = addr,
                Err(_) => warn(
                    &mut issues,
                    "EPIPHANY_BIND",
                    format!(
                        "not a socket address (need IP:PORT, e.g. 127.0.0.1:8080; hostnames are \
                         not resolved): {raw:?}; keeping {}",
                        config.bind_addr
                    ),
                ),
            }
        }
        if let Some(dir) = vars.get("EPIPHANY_DATA_DIR") {
            config.data_dir = PathBuf::from(dir);
        }
        if let Some(filter) = vars.get("EPIPHANY_LOG") {
            config.log_filter = filter.clone();
        }
        if let Some(raw) = vars.get("EPIPHANY_OPEN_BROWSER") {
            match parse_bool(raw) {
                Some(b) => config.open_browser = b,
                None => warn(
                    &mut issues,
                    "EPIPHANY_OPEN_BROWSER",
                    format!(
                        "not a boolean (1/true/yes/on or 0/false/no/off): {raw:?}; keeping off"
                    ),
                ),
            }
        }
        parse_secs(vars, "EPIPHANY_SESSION_TTL_SECS", &mut issues, |secs| {
            config.session_ttl_millis = secs.saturating_mul(1000)
        });
        parse_secs(vars, "EPIPHANY_SESSION_IDLE_SECS", &mut issues, |secs| {
            // 0 disables the idle timeout (absolute TTL still applies).
            config.session_idle_millis = (secs > 0).then(|| secs.saturating_mul(1000));
        });
        parse_secs(vars, "EPIPHANY_SCHEDULER_TICK_SECS", &mut issues, |secs| {
            config.scheduler_tick_millis = secs.saturating_mul(1000)
        });
        parse_num::<usize>(vars, "EPIPHANY_AUDIT_MAX_RECORDS", &mut issues, |max| {
            config.audit_max_records = max
        });
        parse_num::<u64>(vars, "EPIPHANY_AUDIT_RETENTION_DAYS", &mut issues, |days| {
            // 0 (or absent) means no age limit.
            config.audit_retention_millis =
                (days > 0).then(|| days.saturating_mul(24 * 60 * 60 * 1000));
        });
        parse_num::<usize>(vars, "EPIPHANY_RUN_LEDGER_MAX_RUNS", &mut issues, |max| {
            config.run_ledger_max_runs = max
        });
        parse_num::<u32>(vars, "EPIPHANY_LOGIN_MAX_FAILURES", &mut issues, |n| {
            config.login_max_failures = n
        });
        parse_secs(vars, "EPIPHANY_LOGIN_LOCKOUT_SECS", &mut issues, |secs| {
            config.login_lockout_millis = secs.saturating_mul(1000)
        });
        parse_num::<usize>(vars, "EPIPHANY_PASSWORD_MIN_LENGTH", &mut issues, |n| {
            config.password_min_length = n
        });
        if let Some(raw) = vars.get("EPIPHANY_PASSWORD_REJECT_COMMON") {
            match parse_bool(raw) {
                Some(b) => config.password_reject_common = b,
                None => warn(
                    &mut issues,
                    "EPIPHANY_PASSWORD_REJECT_COMMON",
                    format!(
                        "not a boolean (1/true/yes/on or 0/false/no/off): {raw:?}; keeping the \
                         common-password reject list on"
                    ),
                ),
            }
        }
        parse_num::<usize>(vars, "EPIPHANY_VIEW_CACHE_ENTRIES", &mut issues, |n| {
            config.view_cache_entries = n
        });
        // Session-cookie `Secure` override for a TLS-terminating reverse proxy
        // (ADR-0018 / DEPLOYMENT.md); otherwise the flag follows the served scheme.
        if let Some(raw) = vars.get("EPIPHANY_SECURE_COOKIES") {
            match parse_bool(raw) {
                Some(b) => config.secure_cookies_override = Some(b),
                None => warn(
                    &mut issues,
                    "EPIPHANY_SECURE_COOKIES",
                    format!(
                        "not a boolean (1/true/yes/on or 0/false/no/off): {raw:?}; deriving the \
                         cookie Secure flag from the served scheme instead"
                    ),
                ),
            }
        }
        // TLS (ADR-0019). `EPIPHANY_TLS=on` (or self-signed/1/true/yes) serves a
        // generated self-signed cert; an explicit cert+key takes precedence. An
        // unrecognized value is FATAL, not silently off: serving plaintext to an
        // operator who asked for HTTPS is fail-open, so we fail closed instead.
        if let Some(raw) = vars.get("EPIPHANY_TLS") {
            match raw.trim().to_ascii_lowercase().as_str() {
                "on" | "self-signed" | "1" | "true" | "yes" => config.tls_self_signed = true,
                "off" | "0" | "false" | "no" => config.tls_self_signed = false,
                _ => issues.push(ConfigIssue {
                    var: "EPIPHANY_TLS".to_string(),
                    message: format!(
                        "unrecognized value {raw:?}: refusing to start rather than guess whether \
                         HTTPS was intended. Use `on` (self-signed HTTPS), `off` (plain HTTP), or \
                         set EPIPHANY_TLS_CERT and EPIPHANY_TLS_KEY for your own certificate"
                    ),
                    fatal: true,
                }),
            }
        }
        config.tls_cert = vars.get("EPIPHANY_TLS_CERT").map(PathBuf::from);
        config.tls_key = vars.get("EPIPHANY_TLS_KEY").map(PathBuf::from);
        (config, issues)
    }
}

/// Record a non-fatal warning that a present `EPIPHANY_*` value was rejected and
/// the default kept.
fn warn(issues: &mut Vec<ConfigIssue>, var: &str, message: String) {
    issues.push(ConfigIssue {
        var: var.to_string(),
        message,
        fatal: false,
    });
}

/// Parse an integer `EPIPHANY_*` value, warning (and keeping the default) on a
/// present-but-unparseable value, then handing a valid parse to `apply`.
fn parse_num<T>(
    vars: &BTreeMap<String, String>,
    var: &str,
    issues: &mut Vec<ConfigIssue>,
    apply: impl FnOnce(T),
) where
    T: std::str::FromStr,
{
    if let Some(raw) = vars.get(var) {
        match raw.trim().parse::<T>() {
            Ok(n) => apply(n),
            Err(_) => issues.push(ConfigIssue {
                var: var.to_string(),
                message: format!("not a valid integer: {raw:?}; keeping the default"),
                fatal: false,
            }),
        }
    }
}

/// Like [`parse_num`] but specialized to a `u64` count of seconds, for the
/// several `*_SECS` knobs.
fn parse_secs(
    vars: &BTreeMap<String, String>,
    var: &str,
    issues: &mut Vec<ConfigIssue>,
    apply: impl FnOnce(u64),
) {
    parse_num::<u64>(vars, var, issues, apply);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build config from a `[(key, value)]` slice, asserting there were no
    /// issues (for the happy-path tests).
    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let (config, issues) = config_and_issues(pairs);
        assert!(issues.is_empty(), "unexpected config issues: {issues:?}");
        config
    }

    fn config_and_issues(pairs: &[(&str, &str)]) -> (Config, Vec<ConfigIssue>) {
        let vars: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_map(&vars)
    }

    #[test]
    fn defaults_are_loopback_8080() {
        let c = Config::default();
        assert_eq!(c.bind_addr.port(), 8080);
        assert!(c.bind_addr.ip().is_loopback());
        assert!(!c.open_browser);
    }

    #[test]
    fn env_map_overrides_defaults() {
        let c = cfg(&[
            ("EPIPHANY_BIND", "127.0.0.1:9999"),
            ("EPIPHANY_DATA_DIR", "/tmp/epi"),
            ("EPIPHANY_OPEN_BROWSER", "true"),
        ]);
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
        let c = cfg(&[
            ("EPIPHANY_LOGIN_MAX_FAILURES", "3"),
            ("EPIPHANY_LOGIN_LOCKOUT_SECS", "60"),
        ]);
        assert_eq!(c.login_max_failures, 3);
        assert_eq!(c.login_lockout_millis, 60_000);
    }

    #[test]
    fn tls_is_off_by_default_and_configurable() {
        // Off out of the box (ADR-0019).
        assert!(!Config::default().wants_tls());
        // One variable enables self-signed HTTPS.
        let c = cfg(&[("EPIPHANY_TLS", "on")]);
        assert!(c.tls_self_signed);
        assert!(c.wants_tls());
        // An operator certificate is picked up from cert+key paths.
        let c = cfg(&[
            ("EPIPHANY_TLS_CERT", "/etc/epi/cert.pem"),
            ("EPIPHANY_TLS_KEY", "/etc/epi/key.pem"),
        ]);
        assert!(c.wants_tls());
        assert_eq!(c.tls_cert.unwrap(), PathBuf::from("/etc/epi/cert.pem"));
        // A cert without a key is not enough to enable TLS.
        let c = cfg(&[("EPIPHANY_TLS_CERT", "/etc/epi/cert.pem")]);
        assert!(!c.wants_tls());
    }

    #[test]
    fn tls_off_value_does_not_enable_tls() {
        // Explicit falsy values keep TLS off, with no diagnostic.
        for v in ["off", "0", "false", "no", "OFF", " off "] {
            let (c, issues) = config_and_issues(&[("EPIPHANY_TLS", v)]);
            assert!(!c.tls_self_signed, "EPIPHANY_TLS={v:?} must not enable TLS");
            assert!(!c.wants_tls());
            assert!(issues.is_empty(), "EPIPHANY_TLS={v:?} should be clean");
        }
    }

    #[test]
    fn unrecognized_tls_value_is_fatal_not_silently_off() {
        // The security-critical case: an operator who wrote EPIPHANY_TLS=enabled
        // (or =require/=https/typo) must NOT get silent plaintext. It is a fatal
        // issue so startup aborts rather than fail open.
        for v in ["enabled", "require", "https", "tls", "yep"] {
            let (c, issues) = config_and_issues(&[("EPIPHANY_TLS", v)]);
            // The default field value is unchanged (off), but the issue is fatal,
            // so the caller (from_env) refuses to start on it.
            assert!(!c.tls_self_signed);
            let tls_issue = issues
                .iter()
                .find(|i| i.var == "EPIPHANY_TLS")
                .unwrap_or_else(|| panic!("EPIPHANY_TLS={v:?} should raise an issue"));
            assert!(
                tls_issue.fatal,
                "EPIPHANY_TLS={v:?} must be fatal (fail-closed), not a silent default"
            );
        }
    }

    #[test]
    fn view_cache_entries_default_and_override() {
        // A cache is on by default (ADR-0028).
        assert_eq!(Config::default().view_cache_entries, 256);
        assert_eq!(
            cfg(&[("EPIPHANY_VIEW_CACHE_ENTRIES", "0")]).view_cache_entries,
            0
        );
        assert_eq!(
            cfg(&[("EPIPHANY_VIEW_CACHE_ENTRIES", "1024")]).view_cache_entries,
            1024
        );
    }

    #[test]
    fn malformed_bind_falls_back_to_default_with_a_warning() {
        // A hostname (SocketAddr does not resolve names) or garbage keeps the
        // safe loopback default, but is surfaced as a warning, not silent.
        for v in ["not-an-addr", "localhost:8080"] {
            let (c, issues) = config_and_issues(&[("EPIPHANY_BIND", v)]);
            assert_eq!(c.bind_addr.port(), 8080);
            assert!(c.bind_addr.ip().is_loopback());
            assert!(
                issues.iter().any(|i| i.var == "EPIPHANY_BIND" && !i.fatal),
                "EPIPHANY_BIND={v:?} should warn (not silently ignore)"
            );
        }
    }

    #[test]
    fn malformed_numeric_and_bool_knobs_warn_and_keep_defaults() {
        let (c, issues) = config_and_issues(&[
            ("EPIPHANY_SESSION_TTL_SECS", "soon"),
            ("EPIPHANY_LOGIN_MAX_FAILURES", "lots"),
            ("EPIPHANY_OPEN_BROWSER", "maybe"),
        ]);
        // Defaults preserved.
        assert_eq!(c.session_ttl_millis, 8 * 60 * 60 * 1000);
        assert_eq!(c.login_max_failures, 5);
        assert!(!c.open_browser);
        // Each surfaced as a non-fatal warning.
        for var in [
            "EPIPHANY_SESSION_TTL_SECS",
            "EPIPHANY_LOGIN_MAX_FAILURES",
            "EPIPHANY_OPEN_BROWSER",
        ] {
            assert!(
                issues.iter().any(|i| i.var == var && !i.fatal),
                "{var} should warn"
            );
        }
    }

    #[test]
    fn boolean_grammar_is_unified_across_flags() {
        // The same falsy/truthy grammar applies to every boolean flag.
        assert!(!cfg(&[("EPIPHANY_OPEN_BROWSER", "off")]).open_browser);
        assert!(cfg(&[("EPIPHANY_OPEN_BROWSER", "on")]).open_browser);
        assert!(!cfg(&[("EPIPHANY_PASSWORD_REJECT_COMMON", "no")]).password_reject_common);
        assert!(cfg(&[("EPIPHANY_PASSWORD_REJECT_COMMON", "yes")]).password_reject_common);
    }

    #[test]
    fn secure_cookies_follow_served_scheme_unless_overridden() {
        let base = Config::default();
        // No override: the flag mirrors what is actually on the wire.
        assert!(!base.secure_cookies(false), "plain HTTP -> not Secure");
        assert!(base.secure_cookies(true), "HTTPS -> Secure");

        // Explicit override wins in both directions (proxy terminating TLS in
        // front of plain HTTP still wants Secure cookies).
        let forced_on = cfg(&[("EPIPHANY_SECURE_COOKIES", "1")]);
        assert_eq!(forced_on.secure_cookies_override, Some(true));
        assert!(forced_on.secure_cookies(false));
        let forced_off = cfg(&[("EPIPHANY_SECURE_COOKIES", "off")]);
        assert_eq!(forced_off.secure_cookies_override, Some(false));
        assert!(!forced_off.secure_cookies(true));
    }
}
