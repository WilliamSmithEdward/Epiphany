//! API-side helpers for the HTTP connector (ADR-0030): URL host extraction and
//! the SSRF allowlist gate, output-format tokens, and resolving a connection's
//! credential into an `Authorization` header from the secret store. The actual
//! fetch lives in `epiphany-connect` behind its `http` feature; the secret store
//! is never handed to the connector (the value is resolved here and passed in as
//! a header string).

use epiphany_core::{HttpAuth, HttpAuthKind, SourceFormat};
use epiphany_security::SecretStore;

use crate::{ApiError, AppState};

/// The output-format token for a connection DTO.
pub(crate) fn format_token(format: SourceFormat) -> &'static str {
    match format {
        SourceFormat::Csv => "csv",
        SourceFormat::Json => "json",
    }
}

/// Parse an output-format token (empty defaults to CSV).
pub(crate) fn parse_format(token: &str) -> Result<SourceFormat, ApiError> {
    match token {
        "csv" | "" => Ok(SourceFormat::Csv),
        "json" => Ok(SourceFormat::Json),
        other => Err(ApiError::bad_request(format!(
            "unknown output format '{other}' (expected 'csv' or 'json')"
        ))),
    }
}

/// The lowercased host of an absolute http(s) URL, or an error if the URL does
/// not parse as one.
pub(crate) fn url_host(url: &str) -> Result<String, ApiError> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| {
            ApiError::unprocessable("INVALID_URL", "url must start with http:// or https://")
        })?;
    // The authority is everything up to the first path/query/fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Strip any userinfo (`user:pass@`) and the `:port`.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = authority.split(':').next().unwrap_or_default();
    if host.is_empty() {
        return Err(ApiError::unprocessable("INVALID_URL", "url has no host"));
    }
    Ok(host.to_ascii_lowercase())
}

/// Gate: the URL's host must be in the operator allowlist (ADR-0030 SSRF
/// control). A non-parseable URL or a non-allowlisted host is rejected.
pub(crate) fn require_http_host_allowed(state: &AppState, url: &str) -> Result<(), ApiError> {
    let host = url_host(url)?;
    if state.http.allows_host(&host) {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "host '{host}' is not in the HTTP connector allowlist (set EPIPHANY_HTTP_ALLOWED_HOSTS)"
        )))
    }
}

/// Resolve an HTTP connection's credential into the concrete `Authorization`
/// header value, from the secret store. The value is used only to build the
/// header (passed straight to the connector); a missing secret fails the
/// resolution (fail-closed).
pub(crate) fn resolve_auth_header(
    secrets: &SecretStore,
    auth: &HttpAuth,
) -> Result<String, ApiError> {
    let value = secrets.get(&auth.secret).ok_or_else(|| {
        ApiError::unprocessable(
            "UNKNOWN_SECRET",
            format!("no secret named '{}'", auth.secret),
        )
    })?;
    Ok(match auth.kind {
        HttpAuthKind::Bearer => format!("Bearer {value}"),
        HttpAuthKind::Basic => format!("Basic {}", base64_encode(value.as_bytes())),
    })
}

/// Standard base64 (with padding): a tiny dependency-free encoder for Basic auth.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_extraction() {
        assert_eq!(
            url_host("https://api.example.com/x?y=1").unwrap(),
            "api.example.com"
        );
        assert_eq!(url_host("http://Host:8080/p").unwrap(), "host");
        assert_eq!(
            url_host("https://u:p@h.example.com/").unwrap(),
            "h.example.com"
        );
        assert!(url_host("ftp://x/").is_err());
        assert!(url_host("https:///nohost").is_err());
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }
}
