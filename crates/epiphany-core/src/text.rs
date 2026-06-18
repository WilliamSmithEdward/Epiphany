//! Model-as-code: canonical TOML (de)serialization (ADR-0003).
//!
//! A cube and its dimensions round-trip losslessly through a human-readable,
//! Git-friendly TOML document. Serialization is canonical: elements in
//! definition order, edges, attributes, and cells sorted, so re-serializing a
//! parsed model reproduces byte-identical text (verified by a round-trip test).
//!
//! The format is model-shaped: top-level `[[dimension]]` blocks plus a `[cube]`
//! that references them by name, so it stays forward-compatible with a future
//! multi-cube model that shares dimensions.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::query::{
    AxisSpec, CommandSpec, Connection, ConnectionSpec, Flow, FlowTest, HttpAuth, HttpAuthKind,
    HttpSpec, Job, Model, RuleSet, RuleTest, Sandbox, SourceFormat, SqlEngine, SqlSpec, SqlSslMode,
    Subset, SubsetKind, TestCell, Trigger, View, Visibility,
};
use crate::{AttributeKind, AttributeValue, Cube, Dimension, ElementKind, Fixed, ModelError};

const FORMAT_TAG: &str = "epiphany-model/v0";

/// An error loading a model from text.
#[derive(Debug)]
pub enum LoadError {
    /// The TOML could not be parsed.
    Toml(toml::de::Error),
    /// The document's `format` tag was missing or unrecognized.
    UnknownFormat(String),
    /// A cube referenced a dimension that is not defined.
    UnknownDimension(String),
    /// An edge or cell referenced an element not present in its dimension.
    UnknownElement { dimension: String, element: String },
    /// A cell coordinate had the wrong number of components for its cube.
    CoordRank {
        cube: String,
        expected: usize,
        got: usize,
    },
    /// Building the model failed a structural rule.
    Model(ModelError),
    /// The model file could not be read.
    Io(std::io::Error),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Toml(e) => write!(f, "invalid model TOML: {e}"),
            LoadError::UnknownFormat(tag) => {
                write!(
                    f,
                    "unknown model format tag '{tag}' (expected '{FORMAT_TAG}')"
                )
            }
            LoadError::UnknownDimension(name) => {
                write!(f, "cube references undefined dimension '{name}'")
            }
            LoadError::UnknownElement { dimension, element } => {
                write!(f, "unknown element '{element}' in dimension '{dimension}'")
            }
            LoadError::CoordRank {
                cube,
                expected,
                got,
            } => write!(
                f,
                "cell in cube '{cube}' has {got} coordinates but the cube has {expected} dimensions"
            ),
            LoadError::Model(e) => write!(f, "{e}"),
            LoadError::Io(e) => write!(f, "could not read model file: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<ModelError> for LoadError {
    fn from(e: ModelError) -> Self {
        LoadError::Model(e)
    }
}

/// An error saving a model to text.
#[derive(Debug)]
pub enum SaveError {
    /// TOML serialization failed.
    Toml(toml::ser::Error),
    /// The model file could not be written.
    Io(std::io::Error),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Toml(e) => write!(f, "failed to serialize model: {e}"),
            SaveError::Io(e) => write!(f, "failed to write model file: {e}"),
        }
    }
}

impl std::error::Error for SaveError {}

// ---- serialized document shape ----

#[derive(Serialize, Deserialize)]
struct ModelDoc {
    format: String,
    cube: CubeDoc,
    #[serde(default, rename = "dimension")]
    dimensions: Vec<DimDoc>,
    #[serde(default, rename = "cell")]
    cells: Vec<CellDoc>,
    #[serde(default, rename = "string_cell", skip_serializing_if = "Vec::is_empty")]
    string_cells: Vec<StringCellDoc>,
    // Subsets and views are optional and skipped when empty, so a model without
    // them serializes byte-identically to the pre-3E (cube-only) format.
    #[serde(default, rename = "subset", skip_serializing_if = "Vec::is_empty")]
    subsets: Vec<SubsetDoc>,
    #[serde(default, rename = "view", skip_serializing_if = "Vec::is_empty")]
    views: Vec<ViewDoc>,
    // Rules and rule tests are optional and skipped when empty, so a model
    // without them serializes byte-identically to the pre-4H format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rules: Option<RuleSetDoc>,
    #[serde(default, rename = "rule_test", skip_serializing_if = "Vec::is_empty")]
    rule_tests: Vec<RuleTestDoc>,
    // Flows and flow tests are optional and skipped when empty, so a model
    // without them serializes byte-identically to the pre-5A format.
    #[serde(default, rename = "flow", skip_serializing_if = "Vec::is_empty")]
    flows: Vec<FlowDoc>,
    #[serde(default, rename = "flow_test", skip_serializing_if = "Vec::is_empty")]
    flow_tests: Vec<FlowTestDoc>,
    // Connections are optional and skipped when empty (backward-compatible).
    #[serde(default, rename = "connection", skip_serializing_if = "Vec::is_empty")]
    connections: Vec<ConnectionDoc>,
    // Sandboxes are optional and skipped when empty, so a model without any
    // serializes byte-identically to the pre-6A format.
    #[serde(default, rename = "sandbox", skip_serializing_if = "Vec::is_empty")]
    sandboxes: Vec<SandboxDoc>,
    // Jobs are optional and skipped when empty, so a model without any serializes
    // byte-identically to the pre-8B format.
    #[serde(default, rename = "job", skip_serializing_if = "Vec::is_empty")]
    jobs: Vec<JobDoc>,
}

#[derive(Serialize, Deserialize)]
struct CubeDoc {
    name: String,
    dimensions: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct DimDoc {
    name: String,
    elements: Vec<ElDoc>,
    #[serde(default)]
    edges: Vec<EdgeDoc>,
    #[serde(default)]
    attributes: Vec<AttrDefDoc>,
    #[serde(default)]
    attribute_values: Vec<AttrValDoc>,
}

#[derive(Serialize, Deserialize)]
struct ElDoc {
    name: String,
    kind: KindDoc,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum KindDoc {
    Leaf,
    String,
    Consolidated,
}

#[derive(Serialize, Deserialize)]
struct EdgeDoc {
    parent: String,
    child: String,
    weight: i64,
}

#[derive(Serialize, Deserialize)]
struct AttrDefDoc {
    name: String,
    kind: AttrKindDoc,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum AttrKindDoc {
    Text,
    Numeric,
    Alias,
}

#[derive(Serialize, Deserialize)]
struct AttrValDoc {
    element: String,
    attribute: String,
    value: String,
}

#[derive(Serialize, Deserialize)]
struct CellDoc {
    coord: Vec<String>,
    value: String,
}

#[derive(Serialize, Deserialize)]
struct StringCellDoc {
    coord: Vec<String>,
    value: String,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum VisibilityDoc {
    Public,
    Private,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum SubsetKindDoc {
    Static,
    Dynamic,
}

#[derive(Serialize, Deserialize)]
struct SubsetDoc {
    name: String,
    dimension: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    visibility: VisibilityDoc,
    kind: SubsetKindDoc,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    members: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mdx: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum AxisSpecTypeDoc {
    Subset,
    Members,
}

#[derive(Serialize, Deserialize)]
struct AxisSpecDoc {
    dimension: String,
    #[serde(rename = "type")]
    spec_type: AxisSpecTypeDoc,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subset: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    members: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct ContextDoc {
    dimension: String,
    member: String,
}

#[derive(Serialize, Deserialize)]
struct ViewDoc {
    name: String,
    cube: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    visibility: VisibilityDoc,
    #[serde(default)]
    suppress_zeros: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rows: Vec<AxisSpecDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    columns: Vec<AxisSpecDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    context: Vec<ContextDoc>,
}

impl From<Visibility> for VisibilityDoc {
    fn from(v: Visibility) -> Self {
        match v {
            Visibility::Public => VisibilityDoc::Public,
            Visibility::Private => VisibilityDoc::Private,
        }
    }
}

impl From<VisibilityDoc> for Visibility {
    fn from(v: VisibilityDoc) -> Self {
        match v {
            VisibilityDoc::Public => Visibility::Public,
            VisibilityDoc::Private => Visibility::Private,
        }
    }
}

fn subset_doc(subset: &Subset) -> SubsetDoc {
    let (kind, members, mdx) = match &subset.kind {
        SubsetKind::Static { members } => (SubsetKindDoc::Static, members.clone(), None),
        SubsetKind::Dynamic { mdx } => (SubsetKindDoc::Dynamic, Vec::new(), Some(mdx.clone())),
    };
    SubsetDoc {
        name: subset.name.clone(),
        dimension: subset.dimension.clone(),
        owner: subset.owner.clone(),
        visibility: subset.visibility.into(),
        kind,
        members,
        mdx,
    }
}

fn build_subset(doc: &SubsetDoc) -> Subset {
    let kind = match doc.kind {
        SubsetKindDoc::Static => SubsetKind::Static {
            members: doc.members.clone(),
        },
        SubsetKindDoc::Dynamic => SubsetKind::Dynamic {
            mdx: doc.mdx.clone().unwrap_or_default(),
        },
    };
    Subset {
        name: doc.name.clone(),
        dimension: doc.dimension.clone(),
        owner: doc.owner.clone(),
        visibility: doc.visibility.into(),
        kind,
    }
}

fn axis_doc(axis: &[AxisSpec]) -> Vec<AxisSpecDoc> {
    axis.iter()
        .map(|spec| match spec {
            AxisSpec::Subset { dimension, subset } => AxisSpecDoc {
                dimension: dimension.clone(),
                spec_type: AxisSpecTypeDoc::Subset,
                subset: Some(subset.clone()),
                members: Vec::new(),
            },
            AxisSpec::Members { dimension, members } => AxisSpecDoc {
                dimension: dimension.clone(),
                spec_type: AxisSpecTypeDoc::Members,
                subset: None,
                members: members.clone(),
            },
        })
        .collect()
}

fn build_axis(docs: &[AxisSpecDoc]) -> Vec<AxisSpec> {
    docs.iter()
        .map(|d| match d.spec_type {
            AxisSpecTypeDoc::Subset => AxisSpec::Subset {
                dimension: d.dimension.clone(),
                subset: d.subset.clone().unwrap_or_default(),
            },
            AxisSpecTypeDoc::Members => AxisSpec::Members {
                dimension: d.dimension.clone(),
                members: d.members.clone(),
            },
        })
        .collect()
}

fn view_doc(view: &View) -> ViewDoc {
    // Context is sorted by dimension for canonical, order-independent output.
    let mut context: Vec<ContextDoc> = view
        .context
        .iter()
        .map(|(dimension, member)| ContextDoc {
            dimension: dimension.clone(),
            member: member.clone(),
        })
        .collect();
    context.sort_by(|a, b| a.dimension.cmp(&b.dimension));
    ViewDoc {
        name: view.name.clone(),
        cube: view.cube.clone(),
        owner: view.owner.clone(),
        visibility: view.visibility.into(),
        suppress_zeros: view.suppress_zeros,
        rows: axis_doc(&view.rows),
        columns: axis_doc(&view.columns),
        context,
    }
}

fn build_view(doc: &ViewDoc) -> View {
    View {
        name: doc.name.clone(),
        cube: doc.cube.clone(),
        owner: doc.owner.clone(),
        visibility: doc.visibility.into(),
        rows: build_axis(&doc.rows),
        columns: build_axis(&doc.columns),
        context: doc
            .context
            .iter()
            .map(|c| (c.dimension.clone(), c.member.clone()))
            .collect(),
        suppress_zeros: doc.suppress_zeros,
    }
}

#[derive(Serialize, Deserialize)]
struct RuleSetDoc {
    source: String,
}

#[derive(Serialize, Deserialize)]
struct CoordEntryDoc {
    dimension: String,
    member: String,
}

#[derive(Serialize, Deserialize)]
struct TestCellDoc {
    coord: Vec<CoordEntryDoc>,
    value: String,
}

#[derive(Serialize, Deserialize)]
struct RuleTestDoc {
    name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fixtures: Vec<TestCellDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assertions: Vec<TestCellDoc>,
}

fn test_cell_doc(cell: &TestCell) -> TestCellDoc {
    // Coordinate entries sorted by dimension (the BTreeMap iterates sorted) for
    // canonical output.
    TestCellDoc {
        coord: cell
            .coord
            .iter()
            .map(|(dimension, member)| CoordEntryDoc {
                dimension: dimension.clone(),
                member: member.clone(),
            })
            .collect(),
        value: cell.value.clone(),
    }
}

fn build_test_cell(doc: &TestCellDoc) -> TestCell {
    TestCell {
        coord: doc
            .coord
            .iter()
            .map(|c| (c.dimension.clone(), c.member.clone()))
            .collect(),
        value: doc.value.clone(),
    }
}

fn rule_test_doc(test: &RuleTest) -> RuleTestDoc {
    RuleTestDoc {
        name: test.name.clone(),
        fixtures: test.fixtures.iter().map(test_cell_doc).collect(),
        assertions: test.assertions.iter().map(test_cell_doc).collect(),
    }
}

fn build_rule_test(doc: &RuleTestDoc) -> RuleTest {
    RuleTest {
        name: doc.name.clone(),
        fixtures: doc.fixtures.iter().map(build_test_cell).collect(),
        assertions: doc.assertions.iter().map(build_test_cell).collect(),
    }
}

#[derive(Serialize, Deserialize)]
struct FlowDoc {
    name: String,
    source: String,
}

#[derive(Serialize, Deserialize)]
struct ParamEntryDoc {
    name: String,
    value: String,
}

#[derive(Serialize, Deserialize)]
struct FlowTestDoc {
    name: String,
    // Required: a flow test must name the flow it runs (always serialized, so a
    // model that omits it is rejected at load rather than silently defaulting).
    flow: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    input: String,
    #[serde(default, rename = "param", skip_serializing_if = "Vec::is_empty")]
    params: Vec<ParamEntryDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assertions: Vec<TestCellDoc>,
}

fn flow_doc(flow: &Flow) -> FlowDoc {
    FlowDoc {
        name: flow.name.clone(),
        source: flow.source.clone(),
    }
}

fn build_flow(doc: &FlowDoc) -> Flow {
    Flow {
        name: doc.name.clone(),
        source: doc.source.clone(),
    }
}

fn flow_test_doc(test: &FlowTest) -> FlowTestDoc {
    FlowTestDoc {
        name: test.name.clone(),
        flow: test.flow.clone(),
        input: test.input.clone(),
        // Params sorted by name (the BTreeMap iterates sorted) for canonical output.
        params: test
            .params
            .iter()
            .map(|(name, value)| ParamEntryDoc {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
        assertions: test.assertions.iter().map(test_cell_doc).collect(),
    }
}

fn build_flow_test(doc: &FlowTestDoc) -> FlowTest {
    FlowTest {
        name: doc.name.clone(),
        flow: doc.flow.clone(),
        input: doc.input.clone(),
        params: doc
            .params
            .iter()
            .map(|p| (p.name.clone(), p.value.clone()))
            .collect(),
        assertions: doc.assertions.iter().map(build_test_cell).collect(),
    }
}

#[derive(Serialize, Deserialize)]
struct ConnectionDoc {
    name: String,
    // The connection kind ("command" or "http"); the per-kind fields below apply.
    kind: String,
    // ---- command fields ----
    #[serde(default, skip_serializing_if = "String::is_empty")]
    program: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(default)]
    format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    json_path: Option<String>,
    #[serde(default)]
    timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    working_dir: Option<String>,
    // ---- http fields (ADR-0030); a secret is referenced by name, never value ----
    #[serde(default, skip_serializing_if = "String::is_empty")]
    url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    headers: Vec<HeaderDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<AuthDoc>,
    // ---- sql fields (ADR-0034); the password is referenced by secret name ----
    #[serde(default, skip_serializing_if = "String::is_empty")]
    engine: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    host: String,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    database: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password_secret: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    query: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    ssl_mode: String,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

#[derive(Serialize, Deserialize)]
struct HeaderDoc {
    name: String,
    value: String,
}

#[derive(Serialize, Deserialize)]
struct AuthDoc {
    kind: String,
    secret: String,
}

fn auth_kind_token(kind: HttpAuthKind) -> String {
    match kind {
        HttpAuthKind::Bearer => "bearer",
        HttpAuthKind::Basic => "basic",
    }
    .to_string()
}

fn build_auth_kind(token: &str) -> HttpAuthKind {
    match token {
        "basic" => HttpAuthKind::Basic,
        _ => HttpAuthKind::Bearer,
    }
}

fn format_token(format: SourceFormat) -> String {
    match format {
        SourceFormat::Csv => "csv",
        SourceFormat::Json => "json",
    }
    .to_string()
}

fn build_format(token: &str) -> SourceFormat {
    match token {
        "json" => SourceFormat::Json,
        _ => SourceFormat::Csv,
    }
}

fn sql_engine_token(engine: SqlEngine) -> String {
    match engine {
        SqlEngine::Postgres => "postgres",
        SqlEngine::MySql => "mysql",
    }
    .to_string()
}

fn build_sql_engine(token: &str) -> SqlEngine {
    match token {
        "mysql" | "mariadb" => SqlEngine::MySql,
        // Empty or unknown loads as Postgres (forward-compatible, matches the
        // connection-kind fallback) rather than failing the model.
        _ => SqlEngine::Postgres,
    }
}

fn ssl_mode_token(mode: SqlSslMode) -> String {
    match mode {
        SqlSslMode::VerifyFull => "verify-full",
        SqlSslMode::Require => "require",
        SqlSslMode::Disable => "disable",
    }
    .to_string()
}

fn build_ssl_mode(token: &str) -> SqlSslMode {
    match token {
        "require" => SqlSslMode::Require,
        "disable" => SqlSslMode::Disable,
        // Empty or unknown loads as the secure default (verify-full).
        _ => SqlSslMode::VerifyFull,
    }
}

fn connection_doc(conn: &Connection) -> ConnectionDoc {
    let base = ConnectionDoc {
        name: conn.name.clone(),
        kind: String::new(),
        program: String::new(),
        args: Vec::new(),
        format: String::new(),
        json_path: None,
        timeout_ms: 0,
        working_dir: None,
        url: String::new(),
        headers: Vec::new(),
        auth: None,
        engine: String::new(),
        host: String::new(),
        port: 0,
        database: String::new(),
        user: String::new(),
        password_secret: None,
        query: String::new(),
        ssl_mode: String::new(),
    };
    match &conn.spec {
        ConnectionSpec::Command(cmd) => ConnectionDoc {
            kind: "command".to_string(),
            program: cmd.program.clone(),
            args: cmd.args.clone(),
            format: format_token(cmd.format),
            json_path: cmd.json_path.clone(),
            timeout_ms: cmd.timeout_ms,
            working_dir: cmd.working_dir.clone(),
            ..base
        },
        ConnectionSpec::Http(http) => ConnectionDoc {
            kind: "http".to_string(),
            format: format_token(http.format),
            json_path: http.json_path.clone(),
            timeout_ms: http.timeout_ms,
            url: http.url.clone(),
            headers: http
                .headers
                .iter()
                .map(|(name, value)| HeaderDoc {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            auth: http.auth.as_ref().map(|a| AuthDoc {
                kind: auth_kind_token(a.kind),
                secret: a.secret.clone(),
            }),
            ..base
        },
        ConnectionSpec::Sql(sql) => ConnectionDoc {
            kind: "sql".to_string(),
            engine: sql_engine_token(sql.engine),
            host: sql.host.clone(),
            port: sql.port,
            database: sql.database.clone(),
            user: sql.user.clone(),
            password_secret: sql.password_secret.clone(),
            query: sql.query.clone(),
            ssl_mode: ssl_mode_token(sql.ssl_mode),
            timeout_ms: sql.timeout_ms,
            ..base
        },
    }
}

fn build_connection(doc: &ConnectionDoc) -> Connection {
    let spec = match doc.kind.as_str() {
        "http" => ConnectionSpec::Http(HttpSpec {
            url: doc.url.clone(),
            headers: doc
                .headers
                .iter()
                .map(|h| (h.name.clone(), h.value.clone()))
                .collect(),
            auth: doc.auth.as_ref().map(|a| HttpAuth {
                kind: build_auth_kind(&a.kind),
                secret: a.secret.clone(),
            }),
            format: build_format(&doc.format),
            json_path: doc.json_path.clone(),
            timeout_ms: doc.timeout_ms,
        }),
        "sql" => ConnectionSpec::Sql(SqlSpec {
            engine: build_sql_engine(&doc.engine),
            host: doc.host.clone(),
            port: doc.port,
            database: doc.database.clone(),
            user: doc.user.clone(),
            password_secret: doc.password_secret.clone(),
            query: doc.query.clone(),
            ssl_mode: build_ssl_mode(&doc.ssl_mode),
            timeout_ms: doc.timeout_ms,
        }),
        // The command kind (and, forward-compatibly, any unknown kind) builds a
        // command from the present fields rather than failing the load.
        _ => ConnectionSpec::Command(CommandSpec {
            program: doc.program.clone(),
            args: doc.args.clone(),
            format: build_format(&doc.format),
            json_path: doc.json_path.clone(),
            timeout_ms: doc.timeout_ms,
            working_dir: doc.working_dir.clone(),
        }),
    };
    Connection {
        name: doc.name.clone(),
        spec,
    }
}

/// A scheduled job (ADR-0013) as model-as-code. The trigger is flattened to its
/// scalar fields (only `Interval` exists in the Phase 8 cut), mirroring how the
/// connection block flattens its single kind.
#[derive(Serialize, Deserialize)]
struct JobDoc {
    name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    steps: Vec<String>,
    /// The interval trigger period in milliseconds.
    every_millis: u64,
    enabled: bool,
}

fn job_doc(job: &Job) -> JobDoc {
    let Trigger::Interval { every_millis } = job.trigger;
    JobDoc {
        name: job.name.clone(),
        steps: job.steps.clone(),
        every_millis,
        enabled: job.enabled,
    }
}

fn build_job(doc: &JobDoc) -> Job {
    Job {
        name: doc.name.clone(),
        steps: doc.steps.clone(),
        trigger: Trigger::Interval {
            every_millis: doc.every_millis,
        },
        enabled: doc.enabled,
    }
}

#[derive(Serialize, Deserialize)]
struct SandboxCellDoc {
    coord: Vec<String>,
    value: String,
}

#[derive(Serialize, Deserialize)]
struct SandboxDoc {
    name: String,
    owner: String,
    created: u64,
    updated: u64,
    #[serde(default, rename = "cell", skip_serializing_if = "Vec::is_empty")]
    cells: Vec<SandboxCellDoc>,
    #[serde(default, rename = "string_cell", skip_serializing_if = "Vec::is_empty")]
    string_cells: Vec<SandboxCellDoc>,
}

fn sandbox_doc(cube: &Cube, sb: &Sandbox) -> SandboxDoc {
    let coord_names = |coord: &[u32]| -> Vec<String> {
        coord
            .iter()
            .enumerate()
            .map(|(d, &idx)| {
                cube.dimension(d)
                    .element(idx)
                    .expect("valid index")
                    .name
                    .clone()
            })
            .collect()
    };
    // The BTreeMaps iterate sorted by coordinate, so output is canonical.
    let cells = sb
        .cells
        .iter()
        .map(|(coord, value)| SandboxCellDoc {
            coord: coord_names(coord),
            value: value.to_string(),
        })
        .collect();
    let string_cells = sb
        .string_cells
        .iter()
        .map(|(coord, value)| SandboxCellDoc {
            coord: coord_names(coord),
            value: value.clone(),
        })
        .collect();
    SandboxDoc {
        name: sb.name.clone(),
        owner: sb.owner.clone(),
        created: sb.created,
        updated: sb.updated,
        cells,
        string_cells,
    }
}

fn build_sandbox(cube: &Cube, cube_name: &str, doc: &SandboxDoc) -> Result<Sandbox, LoadError> {
    let mut cells = BTreeMap::new();
    for c in &doc.cells {
        let coord = resolve_coord(cube, cube_name, &c.coord)?;
        cells.insert(coord, Fixed::from_str(&c.value)?);
    }
    let mut string_cells = BTreeMap::new();
    for c in &doc.string_cells {
        let coord = resolve_coord(cube, cube_name, &c.coord)?;
        string_cells.insert(coord, c.value.clone());
    }
    Ok(Sandbox {
        name: doc.name.clone(),
        owner: doc.owner.clone(),
        created: doc.created,
        updated: doc.updated,
        cells,
        string_cells,
    })
}

impl From<ElementKind> for KindDoc {
    fn from(kind: ElementKind) -> Self {
        match kind {
            ElementKind::Leaf => KindDoc::Leaf,
            ElementKind::String => KindDoc::String,
            ElementKind::Consolidated => KindDoc::Consolidated,
        }
    }
}

impl From<AttributeKind> for AttrKindDoc {
    fn from(kind: AttributeKind) -> Self {
        match kind {
            AttributeKind::Text => AttrKindDoc::Text,
            AttributeKind::Numeric => AttrKindDoc::Numeric,
            AttributeKind::Alias => AttrKindDoc::Alias,
        }
    }
}

impl From<AttrKindDoc> for AttributeKind {
    fn from(kind: AttrKindDoc) -> Self {
        match kind {
            AttrKindDoc::Text => AttributeKind::Text,
            AttrKindDoc::Numeric => AttributeKind::Numeric,
            AttrKindDoc::Alias => AttributeKind::Alias,
        }
    }
}

fn dim_doc(dim: &Dimension) -> DimDoc {
    let elements = dim
        .iter_elements()
        .map(|el| ElDoc {
            name: el.name.clone(),
            kind: el.kind.into(),
        })
        .collect();

    let edges = dim
        .edges()
        .into_iter()
        .map(|(parent, child, weight)| EdgeDoc {
            parent: dim.element(parent).expect("valid index").name.clone(),
            child: dim.element(child).expect("valid index").name.clone(),
            weight,
        })
        .collect();

    let attributes = dim
        .attribute_defs()
        .iter()
        .map(|a| AttrDefDoc {
            name: a.name.clone(),
            kind: a.kind.into(),
        })
        .collect();

    let attribute_values = dim
        .attribute_values()
        .into_iter()
        .map(|(element, attr_index, value)| AttrValDoc {
            element: dim.element(element).expect("valid index").name.clone(),
            attribute: dim.attribute_defs()[attr_index as usize].name.clone(),
            value: match value {
                AttributeValue::Text(text) => text,
                AttributeValue::Numeric(number) => number.to_string(),
            },
        })
        .collect();

    DimDoc {
        name: dim.name().to_string(),
        elements,
        edges,
        attributes,
        attribute_values,
    }
}

fn build_dimension(dim_doc: &DimDoc) -> Result<Dimension, LoadError> {
    let mut dim = Dimension::new(&dim_doc.name);
    for el in &dim_doc.elements {
        match el.kind {
            KindDoc::Leaf => dim.add_leaf(&el.name),
            KindDoc::String => dim.add_string(&el.name),
            KindDoc::Consolidated => dim.add_consolidated(&el.name),
        };
    }
    for edge in &dim_doc.edges {
        let parent = dim
            .index_of(&edge.parent)
            .ok_or_else(|| LoadError::UnknownElement {
                dimension: dim_doc.name.clone(),
                element: edge.parent.clone(),
            })?;
        let child = dim
            .index_of(&edge.child)
            .ok_or_else(|| LoadError::UnknownElement {
                dimension: dim_doc.name.clone(),
                element: edge.child.clone(),
            })?;
        dim.add_child(parent, child, edge.weight)?;
    }
    for attr in &dim_doc.attributes {
        dim.add_attribute(&attr.name, attr.kind.into());
    }
    for av in &dim_doc.attribute_values {
        let element = dim
            .index_of(&av.element)
            .ok_or_else(|| LoadError::UnknownElement {
                dimension: dim_doc.name.clone(),
                element: av.element.clone(),
            })?;
        let kind = dim
            .attribute_index(&av.attribute)
            .and_then(|i| dim.attribute_defs().get(i as usize).map(|d| d.kind))
            .ok_or_else(|| ModelError::AttributeNotFound {
                dimension: dim_doc.name.clone(),
                attribute: av.attribute.clone(),
            })?;
        let value = match kind {
            AttributeKind::Numeric => AttributeValue::Numeric(Fixed::from_str(&av.value)?),
            AttributeKind::Text | AttributeKind::Alias => AttributeValue::Text(av.value.clone()),
        };
        dim.set_attribute(element, &av.attribute, value)?;
    }
    Ok(dim)
}

/// Resolve a coordinate's element names to indices for `cube`, validating rank.
fn resolve_coord(cube: &Cube, cube_name: &str, names: &[String]) -> Result<Vec<u32>, LoadError> {
    if names.len() != cube.rank() {
        return Err(LoadError::CoordRank {
            cube: cube_name.to_string(),
            expected: cube.rank(),
            got: names.len(),
        });
    }
    let mut coord = Vec::with_capacity(names.len());
    for (d, name) in names.iter().enumerate() {
        let idx = cube
            .dimension(d)
            .index_of(name)
            .ok_or_else(|| LoadError::UnknownElement {
                dimension: cube.dimension(d).name().to_string(),
                element: name.clone(),
            })?;
        coord.push(idx);
    }
    Ok(coord)
}

/// Build the canonical serialized document for a cube plus already-built object
/// docs. Shared by [`Cube::to_model_text`] (empty collections) and
/// [`Model::to_model_text`]. The per-object-type doc vectors are passed
/// positionally; bundling them into a struct would not aid this single internal
/// assembly point.
#[allow(clippy::too_many_arguments)]
fn build_model_doc(
    cube: &Cube,
    subsets: Vec<SubsetDoc>,
    views: Vec<ViewDoc>,
    rules: Option<RuleSetDoc>,
    rule_tests: Vec<RuleTestDoc>,
    flows: Vec<FlowDoc>,
    flow_tests: Vec<FlowTestDoc>,
    connections: Vec<ConnectionDoc>,
    sandboxes: Vec<SandboxDoc>,
    jobs: Vec<JobDoc>,
) -> ModelDoc {
    let dimensions: Vec<DimDoc> = cube.dimensions().iter().map(dim_doc).collect();

    let coord_names = |coord: &[u32]| -> Vec<String> {
        coord
            .iter()
            .enumerate()
            .map(|(d, &idx)| {
                cube.dimension(d)
                    .element(idx)
                    .expect("valid index")
                    .name
                    .clone()
            })
            .collect()
    };

    // Cells, sorted by coordinate (element-index tuple) for canonical output.
    let mut sorted: Vec<(Vec<u32>, Fixed)> = cube.cell_entries().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let cells: Vec<CellDoc> = sorted
        .into_iter()
        .map(|(coord, value)| CellDoc {
            coord: coord_names(&coord),
            value: value.to_string(),
        })
        .collect();

    // String cells, sorted by coordinate for canonical output.
    let mut sorted_strings: Vec<(Vec<u32>, String)> = cube
        .string_cell_entries()
        .map(|(coord, value)| (coord, value.to_string()))
        .collect();
    sorted_strings.sort_by(|a, b| a.0.cmp(&b.0));
    let string_cells: Vec<StringCellDoc> = sorted_strings
        .into_iter()
        .map(|(coord, value)| StringCellDoc {
            coord: coord_names(&coord),
            value,
        })
        .collect();

    ModelDoc {
        format: FORMAT_TAG.to_string(),
        cube: CubeDoc {
            name: cube.name().to_string(),
            dimensions: cube
                .dimensions()
                .iter()
                .map(|d| d.name().to_string())
                .collect(),
        },
        dimensions,
        cells,
        string_cells,
        subsets,
        views,
        rules,
        rule_tests,
        flows,
        flow_tests,
        connections,
        sandboxes,
        jobs,
    }
}

/// Build a cube from a parsed document (dimensions, then cube, then cells).
fn build_cube_from_doc(doc: &ModelDoc) -> Result<Cube, LoadError> {
    // Build each dimension, keyed by name.
    let mut dims_by_name: HashMap<String, Dimension> = HashMap::new();
    for dim_doc in &doc.dimensions {
        dims_by_name.insert(dim_doc.name.clone(), build_dimension(dim_doc)?);
    }

    // Assemble the cube's dimensions in referenced order.
    let mut cube_dims = Vec::with_capacity(doc.cube.dimensions.len());
    for name in &doc.cube.dimensions {
        let dim = dims_by_name
            .get(name)
            .ok_or_else(|| LoadError::UnknownDimension(name.clone()))?;
        cube_dims.push(dim.clone());
    }
    let mut cube = Cube::new(&doc.cube.name, cube_dims)?;

    // Populate numeric cells, then string cells.
    for cell in &doc.cells {
        let coord = resolve_coord(&cube, &doc.cube.name, &cell.coord)?;
        cube.set_leaf(&coord, Fixed::from_str(&cell.value)?)?;
    }
    for cell in &doc.string_cells {
        let coord = resolve_coord(&cube, &doc.cube.name, &cell.coord)?;
        cube.set_string(&coord, &cell.value)?;
    }
    Ok(cube)
}

impl Model {
    /// Serialize this model (cube, subsets, views) to canonical model-as-code
    /// TOML. Subsets are emitted sorted by `(dimension, name)`, views sorted by
    /// name; a model with neither is byte-identical to [`Cube::to_model_text`].
    pub fn to_model_text(&self) -> Result<String, SaveError> {
        let subsets: Vec<SubsetDoc> = self.subsets.values().map(subset_doc).collect();
        let views: Vec<ViewDoc> = self.views.values().map(view_doc).collect();
        let rules = if self.rules.is_empty() {
            None
        } else {
            Some(RuleSetDoc {
                source: self.rules.source.clone(),
            })
        };
        let rule_tests: Vec<RuleTestDoc> = self.tests.values().map(rule_test_doc).collect();
        let flows: Vec<FlowDoc> = self.flows.values().map(flow_doc).collect();
        let flow_tests: Vec<FlowTestDoc> = self.flow_tests.values().map(flow_test_doc).collect();
        let connections: Vec<ConnectionDoc> =
            self.connections.values().map(connection_doc).collect();
        let sandboxes: Vec<SandboxDoc> = self
            .sandboxes
            .values()
            .map(|sb| sandbox_doc(&self.cube, sb))
            .collect();
        let jobs: Vec<JobDoc> = self.jobs.values().map(job_doc).collect();
        let doc = build_model_doc(
            &self.cube,
            subsets,
            views,
            rules,
            rule_tests,
            flows,
            flow_tests,
            connections,
            sandboxes,
            jobs,
        );
        toml::to_string(&doc).map_err(SaveError::Toml)
    }

    /// Parse a model (cube, subsets, views, rules, tests) from model-as-code TOML.
    /// Rule source is stored verbatim (opaque to core); its validity is checked
    /// when `epiphany-calc` compiles it.
    pub fn from_model_text(text: &str) -> Result<Model, LoadError> {
        let doc: ModelDoc = toml::from_str(text).map_err(LoadError::Toml)?;
        if doc.format != FORMAT_TAG {
            return Err(LoadError::UnknownFormat(doc.format));
        }
        let cube = build_cube_from_doc(&doc)?;

        let mut subsets = BTreeMap::new();
        for sd in &doc.subsets {
            // A subset must reference a real dimension of the cube.
            if !cube.dimensions().iter().any(|d| d.name() == sd.dimension) {
                return Err(LoadError::UnknownDimension(sd.dimension.clone()));
            }
            subsets.insert((sd.dimension.clone(), sd.name.clone()), build_subset(sd));
        }
        let mut views = BTreeMap::new();
        for vd in &doc.views {
            views.insert(vd.name.clone(), build_view(vd));
        }
        let rules = RuleSet {
            source: doc.rules.map(|r| r.source).unwrap_or_default(),
        };
        let mut tests = BTreeMap::new();
        for td in &doc.rule_tests {
            tests.insert(td.name.clone(), build_rule_test(td));
        }
        let mut flows = BTreeMap::new();
        for fd in &doc.flows {
            flows.insert(fd.name.clone(), build_flow(fd));
        }
        let mut flow_tests = BTreeMap::new();
        for ftd in &doc.flow_tests {
            flow_tests.insert(ftd.name.clone(), build_flow_test(ftd));
        }
        let mut connections = BTreeMap::new();
        for cd in &doc.connections {
            connections.insert(cd.name.clone(), build_connection(cd));
        }
        let mut sandboxes = BTreeMap::new();
        for sd in &doc.sandboxes {
            sandboxes.insert(sd.name.clone(), build_sandbox(&cube, &doc.cube.name, sd)?);
        }
        let mut jobs = BTreeMap::new();
        for jd in &doc.jobs {
            jobs.insert(jd.name.clone(), build_job(jd));
        }
        Ok(Model {
            cube,
            subsets,
            views,
            rules,
            tests,
            flows,
            flow_tests,
            connections,
            sandboxes,
            jobs,
        })
    }

    /// Save this model to a model-as-code file (canonical TOML).
    pub fn save_to_path(&self, path: impl AsRef<std::path::Path>) -> Result<(), SaveError> {
        let text = self.to_model_text()?;
        std::fs::write(path, text).map_err(SaveError::Io)
    }

    /// Load a model from a model-as-code file.
    pub fn load_from_path(path: impl AsRef<std::path::Path>) -> Result<Model, LoadError> {
        let text = std::fs::read_to_string(path).map_err(LoadError::Io)?;
        Model::from_model_text(&text)
    }
}

impl Dimension {
    /// Serialize this dimension (elements, edges, attributes) to canonical TOML.
    /// Used to persist a shared, registry-owned dimension (ADR-0024); the cube
    /// snapshot keeps embedding its dimensions via [`Cube::to_model_text`].
    pub fn to_model_text(&self) -> Result<String, SaveError> {
        toml::to_string(&dim_doc(self)).map_err(SaveError::Toml)
    }

    /// Parse a dimension from the TOML produced by
    /// [`to_model_text`](Self::to_model_text).
    pub fn from_model_text(text: &str) -> Result<Dimension, LoadError> {
        let doc: DimDoc = toml::from_str(text).map_err(LoadError::Toml)?;
        build_dimension(&doc)
    }
}

impl Cube {
    /// Serialize this cube and its dimensions to canonical model-as-code TOML.
    pub fn to_model_text(&self) -> Result<String, SaveError> {
        let doc = build_model_doc(
            self,
            Vec::new(),
            Vec::new(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        toml::to_string(&doc).map_err(SaveError::Toml)
    }

    /// Parse a cube and its dimensions from model-as-code TOML. Any subset/view
    /// tables present are ignored (use [`Model::from_model_text`] to keep them).
    pub fn from_model_text(text: &str) -> Result<Cube, LoadError> {
        let doc: ModelDoc = toml::from_str(text).map_err(LoadError::Toml)?;
        if doc.format != FORMAT_TAG {
            return Err(LoadError::UnknownFormat(doc.format));
        }
        build_cube_from_doc(&doc)
    }

    /// Save this cube to a model-as-code file (canonical TOML).
    pub fn save_to_path(&self, path: impl AsRef<std::path::Path>) -> Result<(), SaveError> {
        let text = self.to_model_text()?;
        std::fs::write(path, text).map_err(SaveError::Io)
    }

    /// Load a cube from a model-as-code file.
    pub fn load_from_path(path: impl AsRef<std::path::Path>) -> Result<Cube, LoadError> {
        let text = std::fs::read_to_string(path).map_err(LoadError::Io)?;
        Cube::from_model_text(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dimension;

    fn sample_cube() -> Cube {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        let east = region.add_leaf("East");
        let total = region.add_consolidated("Total");
        let coastal = region.add_consolidated("Coastal");
        for leaf in [north, south, east] {
            region.add_child(total, leaf, 1).unwrap();
        }
        region.add_child(coastal, north, 1).unwrap();
        region.add_child(coastal, east, 1).unwrap();
        region.add_attribute("Code", AttributeKind::Text);
        region.add_attribute("FullName", AttributeKind::Alias);
        region
            .set_attribute(north, "Code", AttributeValue::Text("N".into()))
            .unwrap();
        region
            .set_attribute(north, "FullName", AttributeValue::Text("Northern".into()))
            .unwrap();

        let mut version = Dimension::new("Version");
        let actual = version.add_leaf("Actual");
        let budget = version.add_leaf("Budget");
        let variance = version.add_consolidated("Variance");
        version.add_child(variance, actual, 1).unwrap();
        version.add_child(variance, budget, -1).unwrap();

        let mut cube = Cube::new("Sales", vec![region, version]).unwrap();
        cube.set_leaf(&[north, actual], Fixed::from(100)).unwrap();
        cube.set_leaf(&[north, budget], Fixed::from(80)).unwrap();
        cube.set_leaf(&[south, actual], Fixed::from_str("12.5").unwrap())
            .unwrap();
        cube
    }

    #[test]
    fn round_trips_through_text_canonically() {
        let cube = sample_cube();
        let text1 = cube.to_model_text().unwrap();
        let cube2 = Cube::from_model_text(&text1).unwrap();
        let text2 = cube2.to_model_text().unwrap();
        assert_eq!(text1, text2, "model must round-trip to byte-identical text");
    }

    #[test]
    fn round_trip_preserves_values_and_consolidation() {
        let cube = sample_cube();
        let text = cube.to_model_text().unwrap();
        let cube2 = Cube::from_model_text(&text).unwrap();

        assert_eq!(cube2.rank(), cube.rank());
        assert_eq!(cube2.cell_count(), cube.cell_count());

        let total = cube2.dimension(0).index_of("Total").unwrap();
        let actual = cube2.dimension(1).index_of("Actual").unwrap();
        let variance = cube2.dimension(1).index_of("Variance").unwrap();
        // Total / Actual = 100 + 12.5
        assert_eq!(
            cube2.get(&[total, actual]).unwrap(),
            Fixed::from_str("112.5").unwrap()
        );
        // Total / Variance = (100 - 80) + (12.5 - 0)
        assert_eq!(
            cube2.get(&[total, variance]).unwrap(),
            Fixed::from_str("32.5").unwrap()
        );
    }

    #[test]
    fn round_trip_preserves_attributes_and_aliases() {
        let cube = sample_cube();
        let text = cube.to_model_text().unwrap();
        let cube2 = Cube::from_model_text(&text).unwrap();
        let region = cube2.dimension(0);
        let north = region.index_of("North").unwrap();
        assert_eq!(
            region.attribute(north, "Code"),
            Some(&AttributeValue::Text("N".into()))
        );
        assert_eq!(region.resolve("Northern"), Some(north));
    }

    #[test]
    fn rejects_unknown_format() {
        let text = "format = \"nope\"\n\n[cube]\nname = \"X\"\ndimensions = []\n";
        assert!(matches!(
            Cube::from_model_text(text).unwrap_err(),
            LoadError::UnknownFormat(_)
        ));
    }

    #[test]
    fn saves_and_loads_through_a_file() {
        let cube = sample_cube();
        let path =
            std::env::temp_dir().join(format!("epiphany-model-test-{}.toml", std::process::id()));
        cube.save_to_path(&path).unwrap();
        let loaded = Cube::load_from_path(&path).unwrap();
        std::fs::remove_file(&path).ok();
        // Identical canonical text after a full disk round-trip ("restart and recover").
        assert_eq!(
            loaded.to_model_text().unwrap(),
            cube.to_model_text().unwrap()
        );
    }

    #[test]
    fn round_trips_string_cells() {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let mut measure = Dimension::new("Measure");
        let sales = measure.add_leaf("Sales");
        let comment = measure.add_string("Comment");
        let mut cube = Cube::new("Sales", vec![region, measure]).unwrap();
        cube.set_leaf(&[north, sales], Fixed::from(42)).unwrap();
        cube.set_string(&[north, comment], "high").unwrap();

        let text = cube.to_model_text().unwrap();
        let cube2 = Cube::from_model_text(&text).unwrap();
        // Canonical fixed point, including the string cell and string element.
        assert_eq!(text, cube2.to_model_text().unwrap());

        let region2 = cube2.dimension(0).index_of("North").unwrap();
        let measure2 = cube2.dimension(1);
        let comment2 = measure2.index_of("Comment").unwrap();
        let sales2 = measure2.index_of("Sales").unwrap();
        assert_eq!(
            measure2.element(comment2).unwrap().kind,
            ElementKind::String
        );
        assert_eq!(
            cube2.get_string(&[region2, comment2]).unwrap(),
            Some("high")
        );
        assert_eq!(cube2.get_leaf(&[region2, sales2]).unwrap(), Fixed::from(42));
    }

    #[test]
    fn model_without_subsets_or_views_matches_cube_text() {
        let cube = sample_cube();
        let model = Model::new(cube.clone());
        // Backward compatibility: the pre-3E (cube-only) bytes are unchanged.
        assert_eq!(
            model.to_model_text().unwrap(),
            cube.to_model_text().unwrap()
        );
    }

    #[test]
    fn model_round_trips_subsets_and_views_canonically() {
        use crate::{AxisSpec, Subset, SubsetKind, View, Visibility};

        let mut model = Model::new(sample_cube());
        model.subsets.insert(
            ("Region".into(), "Core".into()),
            Subset {
                name: "Core".into(),
                dimension: "Region".into(),
                owner: Some("ann".into()),
                visibility: Visibility::Private,
                kind: SubsetKind::Static {
                    members: vec!["North".into(), "South".into()],
                },
            },
        );
        model.subsets.insert(
            ("Region".into(), "Rolled".into()),
            Subset {
                name: "Rolled".into(),
                dimension: "Region".into(),
                owner: None,
                visibility: Visibility::Public,
                kind: SubsetKind::Dynamic {
                    mdx: "[Region].[Total].Children".into(),
                },
            },
        );
        model.views.insert(
            "Grid".into(),
            View {
                name: "Grid".into(),
                cube: "Sales".into(),
                owner: None,
                visibility: Visibility::Public,
                rows: vec![AxisSpec::Subset {
                    dimension: "Region".into(),
                    subset: "Core".into(),
                }],
                columns: vec![AxisSpec::Members {
                    dimension: "Version".into(),
                    members: vec!["Actual".into(), "Budget".into()],
                }],
                context: Vec::new(),
                suppress_zeros: true,
            },
        );

        let text1 = model.to_model_text().unwrap();
        let model2 = Model::from_model_text(&text1).unwrap();
        let text2 = model2.to_model_text().unwrap();
        assert_eq!(
            text1, text2,
            "a model with subsets and views must round-trip byte-identically"
        );

        // The loaded model carries the objects (owner/visibility/opaque MDX).
        let core = model2.subset("Region", "Core").unwrap();
        assert_eq!(core.owner.as_deref(), Some("ann"));
        assert_eq!(core.visibility, Visibility::Private);
        let rolled = model2.subset("Region", "Rolled").unwrap();
        assert_eq!(
            rolled.kind,
            SubsetKind::Dynamic {
                mdx: "[Region].[Total].Children".into()
            }
        );
        assert!(model2.view("Grid").unwrap().suppress_zeros);
    }

    #[test]
    fn model_round_trips_rules_and_tests() {
        use crate::{RuleSet, RuleTest, TestCell};

        let mut model = Model::new(sample_cube());
        model.rules = RuleSet {
            source:
                "['Version':'Variance'] = value['Version':'Actual'] - value['Version':'Budget'];"
                    .into(),
        };
        let cell = |region: &str, version: &str, value: &str| {
            let mut coord = std::collections::BTreeMap::new();
            coord.insert("Region".to_string(), region.to_string());
            coord.insert("Version".to_string(), version.to_string());
            TestCell {
                coord,
                value: value.to_string(),
            }
        };
        model.tests.insert(
            "variance_test".to_string(),
            RuleTest {
                name: "variance_test".to_string(),
                fixtures: vec![
                    cell("North", "Actual", "100"),
                    cell("North", "Budget", "80"),
                ],
                assertions: vec![cell("North", "Variance", "20")],
            },
        );

        let text1 = model.to_model_text().unwrap();
        let model2 = Model::from_model_text(&text1).unwrap();
        let text2 = model2.to_model_text().unwrap();
        assert_eq!(
            text1, text2,
            "rules and tests must round-trip byte-identically"
        );
        assert!(model2.rules.source.contains("Variance"));
        assert_eq!(model2.tests.len(), 1);
        let t = &model2.tests["variance_test"];
        assert_eq!(t.fixtures.len(), 2);
        assert_eq!(t.assertions[0].value, "20");
    }

    #[test]
    fn model_round_trips_flows_and_flow_tests() {
        use crate::{Flow, FlowTest, TestCell};

        let mut model = Model::new(sample_cube());
        model.flows.insert(
            "load".to_string(),
            Flow {
                name: "load".to_string(),
                source: "export function rows(ctx: FlowContext): void {\n  for (const r of ctx.input()) ctx.writeCells([]);\n}".to_string(),
            },
        );
        let mut params = std::collections::BTreeMap::new();
        params.insert("version".to_string(), "Actual".to_string());
        params.insert("scale".to_string(), "1000".to_string());
        let mut coord = std::collections::BTreeMap::new();
        coord.insert("Region".to_string(), "North".to_string());
        coord.insert("Version".to_string(), "Actual".to_string());
        model.flow_tests.insert(
            "load_test".to_string(),
            FlowTest {
                name: "load_test".to_string(),
                flow: "load".to_string(),
                input: "Region,Value\nNorth,100\n".to_string(),
                params,
                assertions: vec![TestCell {
                    coord,
                    value: "100".to_string(),
                }],
            },
        );

        let text1 = model.to_model_text().unwrap();
        let model2 = Model::from_model_text(&text1).unwrap();
        let text2 = model2.to_model_text().unwrap();
        assert_eq!(
            text1, text2,
            "flows and flow tests must round-trip byte-identically"
        );
        assert_eq!(model2.flows.len(), 1);
        assert!(model2.flows["load"].source.contains("writeCells"));
        assert_eq!(model2.flow_tests.len(), 1);
        let ft = &model2.flow_tests["load_test"];
        assert_eq!(ft.flow, "load");
        assert_eq!(ft.input, "Region,Value\nNorth,100\n");
        assert_eq!(ft.params["version"], "Actual");
        assert_eq!(ft.params["scale"], "1000");
        assert_eq!(ft.params.len(), 2);
        assert_eq!(ft.assertions[0].value, "100");
    }

    #[test]
    fn model_round_trips_sandboxes() {
        use crate::Sandbox;

        let mut model = Model::new(sample_cube());
        let north = model.cube.dimension(0).resolve("North").unwrap();
        let south = model.cube.dimension(0).resolve("South").unwrap();
        let actual = model.cube.dimension(1).resolve("Actual").unwrap();
        let budget = model.cube.dimension(1).resolve("Budget").unwrap();

        let mut sb = Sandbox::new("whatif", "ann", 7);
        sb.updated = 9;
        // Mixed numeric overrides plus a string override (sample_cube has no
        // string element, so reuse a numeric coord's name space for the test).
        sb.cells
            .insert(vec![north, actual], Fixed::from_str("123.5").unwrap());
        sb.cells.insert(vec![south, budget], Fixed::from(40));
        sb.string_cells
            .insert(vec![north, budget], "needs review".to_string());
        model.sandboxes.insert("whatif".to_string(), sb);

        let text1 = model.to_model_text().unwrap();
        let model2 = Model::from_model_text(&text1).unwrap();
        let text2 = model2.to_model_text().unwrap();
        assert_eq!(text1, text2, "sandboxes must round-trip byte-identically");

        let sb2 = model2.sandbox("whatif").unwrap();
        assert_eq!(sb2.owner, "ann");
        assert_eq!(sb2.created, 7);
        assert_eq!(sb2.updated, 9);
        assert_eq!(sb2.cells.len(), 2);
        assert_eq!(
            sb2.cell(&[north, actual]),
            Some(Fixed::from_str("123.5").unwrap())
        );
        assert_eq!(sb2.cell(&[south, budget]), Some(Fixed::from(40)));
        assert_eq!(sb2.string_cell(&[north, budget]), Some("needs review"));
        assert_eq!(sb2.len(), 3);
    }

    #[test]
    fn model_round_trips_connections() {
        use crate::{CommandSpec, Connection, ConnectionSpec, SourceFormat};

        let mut model = Model::new(sample_cube());
        model.connections.insert(
            "py_extract".to_string(),
            Connection {
                name: "py_extract".to_string(),
                spec: ConnectionSpec::Command(CommandSpec {
                    program: "python".to_string(),
                    args: vec![
                        "scripts/extract.py".to_string(),
                        "--region=North".to_string(),
                    ],
                    format: SourceFormat::Json,
                    json_path: Some("data.rows".to_string()),
                    timeout_ms: 30_000,
                    working_dir: Some("/srv/epiphany/scripts".to_string()),
                }),
            },
        );

        let text1 = model.to_model_text().unwrap();
        let model2 = Model::from_model_text(&text1).unwrap();
        let text2 = model2.to_model_text().unwrap();
        assert_eq!(text1, text2, "connections must round-trip byte-identically");
        assert_eq!(model2.connections.len(), 1);
        let ConnectionSpec::Command(cmd) = &model2.connections["py_extract"].spec else {
            panic!("expected a command connection");
        };
        assert_eq!(cmd.program, "python");
        assert_eq!(cmd.args.len(), 2);
        assert_eq!(cmd.format, SourceFormat::Json);
        assert_eq!(cmd.json_path.as_deref(), Some("data.rows"));
        assert_eq!(cmd.timeout_ms, 30_000);
        assert_eq!(cmd.working_dir.as_deref(), Some("/srv/epiphany/scripts"));
    }

    #[test]
    fn model_round_trips_http_connections() {
        use crate::{Connection, ConnectionSpec, HttpAuth, HttpAuthKind, HttpSpec, SourceFormat};

        let mut model = Model::new(sample_cube());
        model.connections.insert(
            "rates_api".to_string(),
            Connection {
                name: "rates_api".to_string(),
                spec: ConnectionSpec::Http(HttpSpec {
                    url: "https://api.example.com/rates.csv".to_string(),
                    headers: vec![("Accept".to_string(), "text/csv".to_string())],
                    auth: Some(HttpAuth {
                        kind: HttpAuthKind::Bearer,
                        secret: "rates_token".to_string(),
                    }),
                    format: SourceFormat::Csv,
                    json_path: None,
                    timeout_ms: 15_000,
                }),
            },
        );

        let text1 = model.to_model_text().unwrap();
        // The model text references the secret by NAME, never the value (RG-13).
        assert!(text1.contains("rates_token"));
        let model2 = Model::from_model_text(&text1).unwrap();
        let text2 = model2.to_model_text().unwrap();
        assert_eq!(
            text1, text2,
            "http connections must round-trip byte-identically"
        );
        let ConnectionSpec::Http(http) = &model2.connections["rates_api"].spec else {
            panic!("expected an http connection");
        };
        assert_eq!(http.url, "https://api.example.com/rates.csv");
        assert_eq!(
            http.headers,
            vec![("Accept".to_string(), "text/csv".to_string())]
        );
        assert_eq!(http.timeout_ms, 15_000);
        let auth = http.auth.as_ref().expect("auth");
        assert_eq!(auth.kind, HttpAuthKind::Bearer);
        assert_eq!(auth.secret, "rates_token");
    }

    #[test]
    fn model_round_trips_sql_connections() {
        use crate::{Connection, ConnectionSpec, SqlEngine, SqlSpec, SqlSslMode};

        let mut model = Model::new(sample_cube());
        model.connections.insert(
            "warehouse".to_string(),
            Connection {
                name: "warehouse".to_string(),
                spec: ConnectionSpec::Sql(SqlSpec {
                    engine: SqlEngine::Postgres,
                    host: "db.internal".to_string(),
                    port: 5432,
                    database: "analytics".to_string(),
                    user: "reporting".to_string(),
                    password_secret: Some("warehouse_pw".to_string()),
                    query: "SELECT region, amount::text FROM sales".to_string(),
                    ssl_mode: SqlSslMode::Require,
                    timeout_ms: 20_000,
                }),
            },
        );

        let text1 = model.to_model_text().unwrap();
        // The model text references the password by secret NAME, never a value.
        assert!(text1.contains("warehouse_pw"));
        let model2 = Model::from_model_text(&text1).unwrap();
        let text2 = model2.to_model_text().unwrap();
        assert_eq!(
            text1, text2,
            "sql connections must round-trip byte-identically"
        );
        let ConnectionSpec::Sql(sql) = &model2.connections["warehouse"].spec else {
            panic!("expected a sql connection");
        };
        assert_eq!(sql.engine, SqlEngine::Postgres);
        assert_eq!(sql.host, "db.internal");
        assert_eq!(sql.port, 5432);
        assert_eq!(sql.database, "analytics");
        assert_eq!(sql.user, "reporting");
        assert_eq!(sql.password_secret.as_deref(), Some("warehouse_pw"));
        assert_eq!(sql.query, "SELECT region, amount::text FROM sales");
        assert_eq!(sql.ssl_mode, SqlSslMode::Require);
        assert_eq!(sql.timeout_ms, 20_000);
    }

    #[test]
    fn model_round_trips_mysql_engine() {
        use crate::{Connection, ConnectionSpec, SqlEngine, SqlSpec, SqlSslMode};

        let mut model = Model::new(sample_cube());
        model.connections.insert(
            "mariadb".to_string(),
            Connection {
                name: "mariadb".to_string(),
                spec: ConnectionSpec::Sql(SqlSpec {
                    engine: SqlEngine::MySql,
                    host: "db.internal".to_string(),
                    port: 3306,
                    database: "app".to_string(),
                    user: "reporting".to_string(),
                    password_secret: None,
                    query: "SELECT 1".to_string(),
                    ssl_mode: SqlSslMode::VerifyFull,
                    timeout_ms: 0,
                }),
            },
        );
        let text1 = model.to_model_text().unwrap();
        assert!(text1.contains("mysql"), "engine token must serialize");
        let model2 = Model::from_model_text(&text1).unwrap();
        assert_eq!(text1, model2.to_model_text().unwrap());
        let ConnectionSpec::Sql(sql) = &model2.connections["mariadb"].spec else {
            panic!("expected a sql connection");
        };
        assert_eq!(sql.engine, SqlEngine::MySql);
        assert_eq!(sql.port, 3306);
    }

    #[test]
    fn model_round_trips_jobs() {
        use crate::{Job, Trigger};

        let mut model = Model::new(sample_cube());
        model.jobs.insert(
            "nightly".to_string(),
            Job {
                name: "nightly".to_string(),
                steps: vec!["load".to_string(), "rollup".to_string()],
                trigger: Trigger::Interval {
                    every_millis: 86_400_000,
                },
                enabled: true,
            },
        );

        let text1 = model.to_model_text().unwrap();
        let model2 = Model::from_model_text(&text1).unwrap();
        let text2 = model2.to_model_text().unwrap();
        assert_eq!(text1, text2, "jobs must round-trip byte-identically");
        assert_eq!(model2.jobs.len(), 1);
        let job = &model2.jobs["nightly"];
        assert_eq!(job.steps, vec!["load".to_string(), "rollup".to_string()]);
        assert_eq!(
            job.trigger,
            Trigger::Interval {
                every_millis: 86_400_000
            }
        );
        assert!(job.enabled);
    }

    #[test]
    fn interval_next_due_is_pure_millis_arithmetic() {
        use crate::Trigger;
        let t = Trigger::Interval { every_millis: 1000 };
        // Never fired: due immediately (fires on the first reconcile tick).
        assert_eq!(t.next_due(None), 0);
        // After firing at 5000, next due is 6000.
        assert_eq!(t.next_due(Some(5000)), 6000);
    }
}
