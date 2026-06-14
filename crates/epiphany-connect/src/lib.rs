//! Data-source connectors: fetch rows from outside the model (ADR-0012).
//!
//! A connector performs the impure fetch at the edge and produces the same
//! `Row`s a flow's `ctx.input()` consumes, so the flow engine, sandbox, and
//! determinism model are unchanged. This crate is the I/O layer; `epiphany-flow`
//! stays pure.
//!
//! The first connector is `command`: run an external program and read its
//! stdout, parsed as CSV or JSON. Python, PowerShell, and a plain executable are
//! all this one connector with a different configured `program`/`args`. Because
//! running a program is arbitrary code execution, the safety controls live above
//! this crate (ADR-0012 decision 6): the command is admin-defined and fixed
//! (never flow-supplied), the server must opt in at runtime, and only an admin
//! can define one. This crate enforces the *mechanical* safety: the program is
//! spawned directly with an argv array (no shell, so no command injection), with
//! a timeout, a stdout size cap, and a non-zero-exit error.
//!
//! Limitation: on a timeout only the spawned process is killed, not any
//! grandchildren it forked (process-group/job-object termination is platform
//! work deferred for now). Configure a connection to run the target program
//! directly (e.g. `python script.py`) rather than wrapping it in a shell that
//! forks, so a kill reaches the real worker.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use epiphany_core::{CommandSpec, SourceFormat};
use epiphany_flow::{parse_csv, Row};

/// Default cap on a command's captured stdout (16 MiB): output beyond this fails
/// the run rather than risking memory exhaustion.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// How long to poll between process liveness checks.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Why a connector did not produce rows.
#[derive(Debug)]
pub enum ConnectError {
    /// The program could not be spawned (not found, not executable, ...).
    Spawn(std::io::Error),
    /// The program ran longer than its timeout and was killed.
    Timeout { millis: u64 },
    /// The program's output exceeded the size cap.
    OutputTooLarge { cap: usize },
    /// The program exited non-zero.
    NonZeroExit { code: Option<i32>, stderr: String },
    /// The program's output could not be parsed as the configured format.
    BadOutput(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Spawn(e) => write!(f, "could not start the program: {e}"),
            ConnectError::Timeout { millis } => {
                write!(
                    f,
                    "the program exceeded its {millis} ms timeout and was killed"
                )
            }
            ConnectError::OutputTooLarge { cap } => {
                write!(f, "the program's output exceeded the {cap}-byte cap")
            }
            ConnectError::NonZeroExit { code, stderr } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                let tail = stderr.trim();
                if tail.is_empty() {
                    write!(f, "the program exited with status {code}")
                } else {
                    write!(f, "the program exited with status {code}: {tail}")
                }
            }
            ConnectError::BadOutput(m) => write!(f, "could not parse the program's output: {m}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Run a command connection and return its output rows. Uses the default output
/// cap; see [`run_command_capped`] to override it (tests).
pub fn run_command(spec: &CommandSpec) -> Result<Vec<Row>, ConnectError> {
    run_command_capped(spec, MAX_OUTPUT_BYTES)
}

/// Run a command connection with an explicit stdout cap.
///
/// Spawns `spec.program` with `spec.args` directly (no shell), with no stdin,
/// reading stdout and stderr concurrently (so neither pipe can deadlock), and
/// killing the process if it runs past `spec.timeout_ms`. On a clean exit the
/// stdout is parsed per `spec.format` into rows.
pub fn run_command_capped(spec: &CommandSpec, cap: usize) -> Result<Vec<Row>, ConnectError> {
    let mut child = Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ConnectError::Spawn)?;

    // Drain both pipes on threads so a chatty program cannot deadlock on a full
    // pipe buffer; stdout is capped, stderr is bounded small for error context.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_reader = spawn_reader(stdout, cap);
    let err_reader = spawn_reader(stderr, 64 * 1024);

    // Poll for exit, enforcing the timeout.
    let timeout = Duration::from_millis(spec.timeout_ms);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if spec.timeout_ms != 0 && start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ConnectError::Timeout {
                        millis: spec.timeout_ms,
                    });
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(ConnectError::Spawn(e)),
        }
    };

    let (out_bytes, overflow) = out_reader.join().unwrap_or((Vec::new(), false));
    let (err_bytes, _) = err_reader.join().unwrap_or((Vec::new(), false));

    if overflow {
        return Err(ConnectError::OutputTooLarge { cap });
    }
    if !status.success() {
        return Err(ConnectError::NonZeroExit {
            code: status.code(),
            stderr: String::from_utf8_lossy(&err_bytes).into_owned(),
        });
    }

    let text = String::from_utf8(out_bytes)
        .map_err(|_| ConnectError::BadOutput("output was not valid UTF-8".to_string()))?;
    parse_output(&text, spec.format, spec.json_path.as_deref())
}

/// Read a stream to EOF on a background thread, storing up to `cap` bytes and
/// reporting whether more was produced (draining the rest so the writer never
/// blocks). Returns `(bytes, overflowed)`.
fn spawn_reader(
    mut stream: impl Read + Send + 'static,
    cap: usize,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut total = 0usize;
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if buf.len() < cap {
                        let take = (cap - buf.len()).min(n);
                        buf.extend_from_slice(&chunk[..take]);
                    }
                    // Beyond the cap we keep reading but discard, so the child
                    // can finish writing and exit.
                }
                Err(_) => break,
            }
        }
        (buf, total > cap)
    })
}

/// Parse a command's stdout into rows per the configured format.
fn parse_output(
    text: &str,
    format: SourceFormat,
    json_path: Option<&str>,
) -> Result<Vec<Row>, ConnectError> {
    // Empty output means "no rows" for either format (a program that legitimately
    // produced nothing), rather than a JSON parse error on an empty document.
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    match format {
        SourceFormat::Csv => parse_csv(text).map_err(|e| ConnectError::BadOutput(e.to_string())),
        SourceFormat::Json => parse_json(text, json_path),
    }
}

/// Parse JSON output into rows: an array of objects, each becoming a row of
/// `(key, value-as-string)`. `json_path` (dotted) navigates to the array when it
/// is nested under object keys.
fn parse_json(text: &str, json_path: Option<&str>) -> Result<Vec<Row>, ConnectError> {
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ConnectError::BadOutput(e.to_string()))?;

    let mut node = &root;
    if let Some(path) = json_path {
        for segment in path.split('.').filter(|s| !s.is_empty()) {
            // Distinguish "not an object to navigate into" from "key missing", so
            // a mis-typed path is debuggable.
            if !node.is_object() {
                return Err(ConnectError::BadOutput(format!(
                    "json_path: expected an object at '{segment}', found {}",
                    json_type_name(node)
                )));
            }
            node = node.get(segment).ok_or_else(|| {
                ConnectError::BadOutput(format!("json_path segment '{segment}' not found"))
            })?;
        }
    }

    let array = node.as_array().ok_or_else(|| {
        ConnectError::BadOutput("expected a JSON array of record objects".to_string())
    })?;

    let mut rows = Vec::with_capacity(array.len());
    for (i, item) in array.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| ConnectError::BadOutput(format!("record {i} is not a JSON object")))?;
        let row: Row = obj
            .iter()
            .map(|(k, v)| (k.clone(), json_scalar(v)))
            .collect();
        rows.push(row);
    }
    Ok(rows)
}

/// A human name for a JSON value's type, for diagnostics.
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Render a JSON scalar as the string a cell value expects. Objects/arrays are
/// serialized compactly (a flow can re-parse if it needs structure).
fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command spec that runs the platform shell to emit `script`'s effect.
    /// On Unix this is `sh -c <script>`; on Windows `cmd /C <script>`. (The shell
    /// here is the *test's* own choosing, not a flow's - production specs name a
    /// program directly.)
    fn shell(script: &str, format: SourceFormat, timeout_ms: u64) -> CommandSpec {
        #[cfg(windows)]
        let (program, args) = (
            "cmd".to_string(),
            vec!["/C".to_string(), script.to_string()],
        );
        #[cfg(not(windows))]
        let (program, args) = ("sh".to_string(), vec!["-c".to_string(), script.to_string()]);
        CommandSpec {
            program,
            args,
            format,
            json_path: None,
            timeout_ms,
        }
    }

    #[test]
    fn runs_a_program_and_parses_csv() {
        #[cfg(windows)]
        let spec = shell(
            "echo Region,Value&&echo North,100&&echo South,200",
            SourceFormat::Csv,
            10_000,
        );
        #[cfg(not(windows))]
        let spec = shell(
            "printf 'Region,Value\\nNorth,100\\nSouth,200\\n'",
            SourceFormat::Csv,
            10_000,
        );

        let rows = run_command(&spec).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], ("Region".to_string(), "North".to_string()));
        assert_eq!(rows[1][1], ("Value".to_string(), "200".to_string()));
    }

    // JSON parsing is a pure function over the program's stdout; test it directly
    // (emitting exact JSON through a shell echo is not portable).
    #[test]
    fn parses_json_array() {
        let json = r#"[{"Region":"North","Value":"100"},{"Region":"South","Value":"200"}]"#;
        let rows = parse_output(json, SourceFormat::Json, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains(&("Region".to_string(), "North".to_string())));
        assert!(rows[1].contains(&("Value".to_string(), "200".to_string())));
    }

    #[test]
    fn json_path_navigates_to_nested_array() {
        let json = r#"{"data":{"rows":[{"R":"North","V":"5"}]}}"#;
        let rows = parse_output(json, SourceFormat::Json, Some("data.rows")).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains(&("R".to_string(), "North".to_string())));
    }

    #[test]
    fn empty_output_is_zero_rows_for_both_formats() {
        assert!(parse_output("", SourceFormat::Csv, None)
            .unwrap()
            .is_empty());
        assert!(parse_output("   \n", SourceFormat::Json, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn json_path_into_a_non_object_reports_the_type() {
        let err = parse_output(r#"{"a":42}"#, SourceFormat::Json, Some("a.b")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("number"), "{msg}");
    }

    #[test]
    fn json_scalars_stringify_and_non_array_errors() {
        let rows =
            parse_output(r#"[{"n":42,"b":true,"x":null}]"#, SourceFormat::Json, None).unwrap();
        assert!(rows[0].contains(&("n".to_string(), "42".to_string())));
        assert!(rows[0].contains(&("b".to_string(), "true".to_string())));
        assert!(rows[0].contains(&("x".to_string(), String::new())));
        assert!(parse_output(r#"{"not":"an array"}"#, SourceFormat::Json, None).is_err());
    }

    #[test]
    fn end_to_end_json_through_a_program() {
        // Echo a single-quoted JSON document via the Unix shell (portable there);
        // on Windows the shell-quoting is unreliable, so this leg is Unix-only and
        // the parser itself is covered by the pure tests above.
        #[cfg(not(windows))]
        {
            let json = r#"[{"Region":"North","Value":"100"}]"#;
            let spec = shell(&format!("printf '%s' '{json}'"), SourceFormat::Json, 10_000);
            let rows = run_command(&spec).unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].contains(&("Region".to_string(), "North".to_string())));
        }
    }

    #[test]
    fn non_zero_exit_is_an_error() {
        #[cfg(windows)]
        let spec = shell("exit /b 3", SourceFormat::Csv, 10_000);
        #[cfg(not(windows))]
        let spec = shell("echo oops 1>&2; exit 3", SourceFormat::Csv, 10_000);

        let err = run_command(&spec).unwrap_err();
        assert!(matches!(err, ConnectError::NonZeroExit { .. }), "{err}");
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        let spec = CommandSpec {
            program: "epiphany-no-such-program-xyz".to_string(),
            args: vec![],
            format: SourceFormat::Csv,
            json_path: None,
            timeout_ms: 10_000,
        };
        assert!(matches!(run_command(&spec), Err(ConnectError::Spawn(_))));
    }

    #[test]
    fn a_slow_program_times_out() {
        #[cfg(windows)]
        let spec = shell("ping -n 5 127.0.0.1 >NUL", SourceFormat::Csv, 300);
        #[cfg(not(windows))]
        let spec = shell("sleep 5", SourceFormat::Csv, 300);

        let err = run_command(&spec).unwrap_err();
        assert!(matches!(err, ConnectError::Timeout { .. }), "{err}");
    }

    #[test]
    fn output_over_the_cap_is_rejected() {
        #[cfg(windows)]
        let spec = shell("echo aaaaaaaaaaaaaaaaaaaa", SourceFormat::Csv, 10_000);
        #[cfg(not(windows))]
        let spec = shell("printf 'aaaaaaaaaaaaaaaaaaaa'", SourceFormat::Csv, 10_000);

        let err = run_command_capped(&spec, 4).unwrap_err();
        assert!(matches!(err, ConnectError::OutputTooLarge { .. }), "{err}");
    }
}
