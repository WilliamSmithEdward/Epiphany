//! HTTP(S) fetch connector (ADR-0030), behind the `http` feature.
//!
//! Fetch a URL (GET) and parse the response body as CSV/JSON into the same rows
//! the command connector produces, so the flow engine is unchanged. Redirects
//! are NOT followed: the fetch only ever contacts the configured URL's host (the
//! API gates that host against an operator allowlist), so a 3xx cannot bounce the
//! request to an internal host (SSRF). Bounded by the spec timeout and a
//! response-size cap. The client is `ureq` over rustls with the ring provider;
//! the connector never sees the secret store (the API resolves a credential into
//! the `Authorization` header value and passes it in).

use std::io::Read;
use std::time::Duration;

use epiphany_core::HttpSpec;
use epiphany_flow::Row;

use crate::{parse_output, ConnectError, MAX_OUTPUT_BYTES};

/// Default request timeout when the spec leaves it unset (the REST layer coerces
/// an unset value to this, so 0 only arises from a hand-edited model).
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Fetch an HTTP(S) connection and return its rows. `auth_header`, if set, is the
/// full `Authorization` header value the API resolved from a secret.
pub fn fetch_http(spec: &HttpSpec, auth_header: Option<&str>) -> Result<Vec<Row>, ConnectError> {
    fetch_http_capped(spec, auth_header, MAX_OUTPUT_BYTES)
}

/// [`fetch_http`] with an explicit response-size cap (tests).
pub fn fetch_http_capped(
    spec: &HttpSpec,
    auth_header: Option<&str>,
    cap: usize,
) -> Result<Vec<Row>, ConnectError> {
    let timeout = if spec.timeout_ms == 0 {
        DEFAULT_TIMEOUT_MS
    } else {
        spec.timeout_ms
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(timeout))
        // No redirects: the fetch must only reach the configured (allowlisted)
        // host, so a 3xx cannot steer it to an internal host (SSRF).
        .redirects(0)
        .build();

    let mut req = agent.get(&spec.url);
    for (name, value) in &spec.headers {
        req = req.set(name, value);
    }
    if let Some(header) = auth_header {
        req = req.set("Authorization", header);
    }

    let response = req.call().map_err(map_ureq_error)?;

    // Read the body, capped: take cap+1 bytes so an overflow is detectable.
    let mut buf = Vec::new();
    response
        .into_reader()
        .take(cap as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| ConnectError::Http(e.to_string()))?;
    if buf.len() > cap {
        return Err(ConnectError::OutputTooLarge { cap });
    }
    let text = String::from_utf8(buf)
        .map_err(|_| ConnectError::BadOutput("response body was not valid UTF-8".to_string()))?;
    parse_output(&text, spec.format, spec.json_path.as_deref())
}

/// Map a ureq error: a non-2xx status carries the (truncated) body; everything
/// else (DNS, connect, TLS, read, or a future variant) is a transport error.
fn map_ureq_error(err: ureq::Error) -> ConnectError {
    match err {
        ureq::Error::Status(code, response) => {
            let mut body = response.into_string().unwrap_or_default();
            body.truncate(2048);
            ConnectError::HttpStatus { code, body }
        }
        other => ConnectError::Http(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::SourceFormat;
    use std::io::Write;
    use std::net::TcpListener;

    /// Bind a localhost listener that answers exactly one request with the given
    /// status line and body, then return the port. The fetch under test connects
    /// to it over plain HTTP (no TLS), exercising the full request/parse path.
    fn serve_once(status_line: &'static str, body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf); // consume the request headers
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        port
    }

    fn spec(port: u16, format: SourceFormat) -> HttpSpec {
        HttpSpec {
            url: format!("http://127.0.0.1:{port}/"),
            headers: Vec::new(),
            auth: None,
            format,
            json_path: None,
            timeout_ms: 5_000,
        }
    }

    #[test]
    fn fetches_and_parses_csv() {
        let port = serve_once("200 OK", "Region,Value\nNorth,100\nSouth,200\n");
        let rows = fetch_http(&spec(port, SourceFormat::Csv), None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], ("Region".to_string(), "North".to_string()));
        assert_eq!(rows[1][1], ("Value".to_string(), "200".to_string()));
    }

    #[test]
    fn fetches_and_parses_json() {
        let port = serve_once("200 OK", r#"[{"Region":"North","Value":"100"}]"#);
        let rows = fetch_http(&spec(port, SourceFormat::Json), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains(&("Region".to_string(), "North".to_string())));
    }

    #[test]
    fn non_2xx_is_an_error() {
        let port = serve_once("404 Not Found", "nope");
        let err = fetch_http(&spec(port, SourceFormat::Csv), None).unwrap_err();
        assert!(
            matches!(err, ConnectError::HttpStatus { code: 404, .. }),
            "{err}"
        );
    }

    #[test]
    fn body_over_the_cap_is_rejected() {
        let port = serve_once("200 OK", "aaaaaaaaaaaaaaaaaaaaaaaa");
        let err = fetch_http_capped(&spec(port, SourceFormat::Csv), None, 4).unwrap_err();
        assert!(matches!(err, ConnectError::OutputTooLarge { .. }), "{err}");
    }
}
