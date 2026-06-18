//! Native SQL fetch connectors (ADR-0034), behind the `postgres` and `mysql`
//! features.
//!
//! Connect to a database and run a fixed, admin-defined query, mapping each
//! result row to the same `Row` the other connectors produce, so the flow engine
//! is unchanged. The drivers are async, but the connector contract is a plain
//! synchronous `Vec<Row>` producer: the query runs on a dedicated thread with a
//! current-thread runtime, so it never builds a runtime inside the caller's
//! runtime and does not depend on the server's runtime flavor. The password is
//! resolved by the API from the secret store and passed in; this layer never sees
//! the secret store. Each engine is a separate cargo feature; an unbuilt engine
//! returns a clear "not built" error.

use std::time::Duration;

use epiphany_core::{SqlEngine, SqlSpec};
use epiphany_flow::{Row, MAX_CSV_ROWS};

use crate::ConnectError;

/// Default connect/query timeout when the spec leaves it unset (the REST layer
/// coerces an unset value to this, so 0 only arises from a hand-edited model).
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Run a SQL connection's query and return its rows. `password`, if set, is the
/// secret value the API resolved from the secret store (this layer never reads
/// the store). Capped at [`MAX_CSV_ROWS`] rows.
pub fn fetch_sql(spec: &SqlSpec, password: Option<&str>) -> Result<Vec<Row>, ConnectError> {
    fetch_sql_capped(spec, password, MAX_CSV_ROWS)
}

/// [`fetch_sql`] with an explicit row cap (tests).
pub fn fetch_sql_capped(
    spec: &SqlSpec,
    password: Option<&str>,
    cap: usize,
) -> Result<Vec<Row>, ConnectError> {
    // The drivers are async; run to completion on a dedicated scoped thread with
    // a current-thread runtime so this stays a synchronous producer and never
    // constructs a runtime inside the caller's (Axum worker) runtime.
    std::thread::scope(|scope| {
        scope
            .spawn(|| run_query(spec, password, cap))
            .join()
            .unwrap_or_else(|_| {
                Err(ConnectError::Sql(
                    "the database query thread panicked".to_string(),
                ))
            })
    })
}

fn run_query(spec: &SqlSpec, password: Option<&str>, cap: usize) -> Result<Vec<Row>, ConnectError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| ConnectError::Sql(format!("could not start the database runtime: {e}")))?;
    rt.block_on(run_query_async(spec, password, cap))
}

async fn run_query_async(
    spec: &SqlSpec,
    password: Option<&str>,
    cap: usize,
) -> Result<Vec<Row>, ConnectError> {
    let millis = if spec.timeout_ms == 0 {
        DEFAULT_TIMEOUT_MS
    } else {
        spec.timeout_ms
    };
    let timeout = Duration::from_millis(millis);
    match spec.engine {
        SqlEngine::Postgres => {
            #[cfg(feature = "postgres")]
            {
                postgres::query(spec, password, cap, timeout, millis).await
            }
            #[cfg(not(feature = "postgres"))]
            {
                let _ = (password, cap, timeout, millis);
                Err(not_built("PostgreSQL"))
            }
        }
        SqlEngine::MySql => {
            #[cfg(feature = "mysql")]
            {
                mysql::query(spec, password, cap, timeout, millis).await
            }
            #[cfg(not(feature = "mysql"))]
            {
                let _ = (password, cap, timeout, millis);
                Err(not_built("MySQL"))
            }
        }
    }
}

#[cfg(not(all(feature = "postgres", feature = "mysql")))]
fn not_built(engine: &str) -> ConnectError {
    ConnectError::Sql(format!(
        "the {engine} connector is not built into this server"
    ))
}

/// A typed cell value (or NULL) as a string; NULL is the empty string. Used by
/// the Postgres row mapper (MySQL's text-protocol values arrive already typed).
#[cfg(feature = "postgres")]
fn opt_to_string<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

// ---- PostgreSQL ----

#[cfg(feature = "postgres")]
mod postgres {
    use super::{opt_to_string, ConnectError, Duration, Row, SqlSpec};
    use epiphany_core::SqlSslMode;
    use std::sync::Arc;
    use tokio_postgres::types::Type;

    const DEFAULT_PORT: u16 = 5432;

    pub(super) async fn query(
        spec: &SqlSpec,
        password: Option<&str>,
        cap: usize,
        timeout: Duration,
        millis: u64,
    ) -> Result<Vec<Row>, ConnectError> {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&spec.host)
            .port(if spec.port == 0 {
                DEFAULT_PORT
            } else {
                spec.port
            })
            .dbname(&spec.database)
            .user(&spec.user)
            .connect_timeout(timeout)
            .application_name("epiphany");
        if let Some(pw) = password {
            config.password(pw);
        }

        // The TLS type differs per ssl_mode, so each arm connects, drives its
        // connection task, and runs the query (the Client type is the same).
        let work = async {
            match spec.ssl_mode {
                SqlSslMode::Disable => {
                    let (client, conn) = config
                        .connect(tokio_postgres::NoTls)
                        .await
                        .map_err(sql_err)?;
                    drive(conn);
                    run(&client, &spec.query, cap).await
                }
                SqlSslMode::Require => {
                    let (client, conn) = config.connect(tls(false)?).await.map_err(sql_err)?;
                    drive(conn);
                    run(&client, &spec.query, cap).await
                }
                SqlSslMode::VerifyFull => {
                    let (client, conn) = config.connect(tls(true)?).await.map_err(sql_err)?;
                    drive(conn);
                    run(&client, &spec.query, cap).await
                }
            }
        };
        tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| ConnectError::Sql(format!("the query exceeded its {millis} ms timeout")))?
    }

    /// Spawn the connection's protocol task so the client can make progress; it
    /// ends when the client is dropped. Errors surface through the query itself.
    fn drive<C>(conn: C)
    where
        C: std::future::Future + Send + 'static,
    {
        tokio::spawn(async move {
            let _ = conn.await;
        });
    }

    async fn run(
        client: &tokio_postgres::Client,
        sql: &str,
        cap: usize,
    ) -> Result<Vec<Row>, ConnectError> {
        let rows = client.query(sql, &[]).await.map_err(sql_err)?;
        if rows.len() > cap {
            return Err(ConnectError::Sql(format!(
                "the query returned more than {cap} rows"
            )));
        }
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let cols = row.columns();
            let mut mapped: Row = Vec::with_capacity(cols.len());
            for (i, col) in cols.iter().enumerate() {
                mapped.push((col.name().to_string(), cell(row, i, col)?));
            }
            out.push(mapped);
        }
        Ok(out)
    }

    /// Render one cell as a string; common scalar types natively, NULL as empty,
    /// an unsupported type fails loudly (cast it to text in the query).
    fn cell(
        row: &tokio_postgres::Row,
        i: usize,
        col: &tokio_postgres::Column,
    ) -> Result<String, ConnectError> {
        let value = match *col.type_() {
            Type::BOOL => row.try_get::<_, Option<bool>>(i).map(opt_to_string),
            Type::INT2 => row.try_get::<_, Option<i16>>(i).map(opt_to_string),
            Type::INT4 => row.try_get::<_, Option<i32>>(i).map(opt_to_string),
            Type::INT8 => row.try_get::<_, Option<i64>>(i).map(opt_to_string),
            Type::OID => row.try_get::<_, Option<u32>>(i).map(opt_to_string),
            Type::FLOAT4 => row.try_get::<_, Option<f32>>(i).map(opt_to_string),
            Type::FLOAT8 => row.try_get::<_, Option<f64>>(i).map(opt_to_string),
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
                row.try_get::<_, Option<String>>(i).map(opt_to_string)
            }
            ref other => {
                let name = col.name();
                return Err(ConnectError::Sql(format!(
                    "column '{name}' has type '{other}', which the SQL connector cannot \
                     render directly; cast it to text in the query (e.g. {name}::text)"
                )));
            }
        };
        value.map_err(sql_err)
    }

    fn sql_err(e: tokio_postgres::Error) -> ConnectError {
        ConnectError::Sql(e.to_string())
    }

    /// rustls connector pinned to ring. `verify` => full verification against the
    /// bundled public roots (default); otherwise encrypt-without-verification (the
    /// libpq `sslmode=require` behavior, for self-signed internal-DB certs).
    fn tls(verify: bool) -> Result<tokio_postgres_rustls::MakeRustlsConnect, ConnectError> {
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| ConnectError::Sql(format!("could not configure TLS: {e}")))?;

        let config = if verify {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        } else {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth()
        };
        Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
    }

    /// Accepts any server certificate: used ONLY for the explicit, opt-in
    /// `sslmode = require` mode (encrypt without verifying, for self-signed
    /// internal-DB certs). The default `verify-full` mode never uses it.
    #[derive(Debug)]
    struct NoVerify;

    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

// ---- MySQL / MariaDB ----

#[cfg(feature = "mysql")]
mod mysql {
    use super::{ConnectError, Duration, Row, SqlSpec};
    use epiphany_core::SqlSslMode;
    use mysql_async::prelude::Queryable;
    use mysql_async::{Conn, OptsBuilder, SslOpts, Value};

    const DEFAULT_PORT: u16 = 3306;

    pub(super) async fn query(
        spec: &SqlSpec,
        password: Option<&str>,
        cap: usize,
        timeout: Duration,
        millis: u64,
    ) -> Result<Vec<Row>, ConnectError> {
        let ssl = match spec.ssl_mode {
            // Encrypt over rustls but do not verify the certificate (for
            // self-signed internal-DB certs), the libpq sslmode=require analog.
            SqlSslMode::Require => Some(SslOpts::default().with_danger_accept_invalid_certs(true)),
            // Full verification against the bundled webpki roots (default).
            SqlSslMode::VerifyFull => Some(SslOpts::default()),
            SqlSslMode::Disable => None,
        };
        // The overall connect + query is bounded by the tokio::time::timeout
        // wrapper below, so no per-builder connect timeout is needed.
        let opts = OptsBuilder::default()
            .ip_or_hostname(spec.host.clone())
            .tcp_port(if spec.port == 0 {
                DEFAULT_PORT
            } else {
                spec.port
            })
            .db_name(Some(spec.database.clone()))
            .user(Some(spec.user.clone()))
            .pass(password.map(|p| p.to_string()))
            .ssl_opts(ssl);

        let work = async {
            let mut conn = Conn::new(opts).await.map_err(sql_err)?;
            let rows: Vec<mysql_async::Row> = conn.query(&spec.query).await.map_err(sql_err)?;
            // Best-effort close; an error here does not invalidate the rows read.
            let _ = conn.disconnect().await;
            if rows.len() > cap {
                return Err(ConnectError::Sql(format!(
                    "the query returned more than {cap} rows"
                )));
            }
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                let cols = row.columns_ref();
                let mut mapped: Row = Vec::with_capacity(cols.len());
                for (i, col) in cols.iter().enumerate() {
                    let value = row.as_ref(i).map(cell).unwrap_or_default();
                    mapped.push((col.name_str().to_string(), value));
                }
                out.push(mapped);
            }
            Ok(out)
        };
        tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| ConnectError::Sql(format!("the query exceeded its {millis} ms timeout")))?
    }

    /// Render one MySQL value as a string. Text-protocol values (including
    /// DECIMAL/NUMERIC, dates, and strings) arrive as bytes; NULL is the empty
    /// string. The binary numeric variants are handled too, for completeness.
    fn cell(value: &Value) -> String {
        match value {
            Value::NULL => String::new(),
            Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Double(d) => d.to_string(),
            Value::Date(y, mo, d, h, mi, s, us) => {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
            }
            Value::Time(neg, d, h, mi, s, us) => {
                let sign = if *neg { "-" } else { "" };
                format!(
                    "{sign}{:02}:{:02}:{:02}.{:06}",
                    *d * 24 + u32::from(*h),
                    mi,
                    s,
                    us
                )
            }
        }
    }

    fn sql_err(e: mysql_async::Error) -> ConnectError {
        ConnectError::Sql(e.to_string())
    }
}
