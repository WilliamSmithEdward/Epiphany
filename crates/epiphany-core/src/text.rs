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
    Automation, AxisSpec, CommandSpec, Connection, ConnectionSpec, Flow, FlowInput,
    FlowInputBinding, FlowTest, HttpAuth, HttpAuthKind, HttpSpec, Job, Model, RuleSet, RuleTest,
    Sandbox, SourceFormat, SqlEngine, SqlSpec, SqlSslMode, Subset, SubsetKind, TestCell, Trigger,
    View, Visibility,
};
use crate::{AttributeKind, AttributeValue, Cube, Dimension, ElementKind, Fixed, ModelError};

const FORMAT_TAG: &str = "epiphany-model/v0";
/// The format tag for the server-global automation model file (ADR-0035).
const AUTOMATION_FORMAT_TAG: &str = "epiphany-automation/v0";

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
    // Sandboxes are optional and skipped when empty, so a model without any
    // serializes byte-identically to the pre-6A format.
    #[serde(default, rename = "sandbox", skip_serializing_if = "Vec::is_empty")]
    sandboxes: Vec<SandboxDoc>,
    // Flows, flow tests, connections, and jobs are no longer per-cube (ADR-0035):
    // they live in the server-global automation model. A `[[flow]]`/`[[job]]`/
    // `[[connection]]`/`[[flow_test]]` block in an old cube model is ignored on
    // load (boot migration lifts them into the global store), and is no longer
    // emitted, so a model without them stays byte-identical to the pre-5A format.
    #[serde(default, rename = "flow", skip_serializing)]
    flows: Vec<FlowDoc>,
    #[serde(default, rename = "flow_test", skip_serializing)]
    flow_tests: Vec<FlowTestDoc>,
    #[serde(default, rename = "connection", skip_serializing)]
    connections: Vec<ConnectionDoc>,
    #[serde(default, rename = "job", skip_serializing)]
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
    /// Drop result rows whose values are all zero across the shown columns.
    #[serde(default)]
    suppress_zero_rows: bool,
    /// Drop result columns whose values are all zero across the shown rows.
    #[serde(default)]
    suppress_zero_columns: bool,
    /// Legacy single zero-suppression flag (pre-split). Read for back-compat
    /// only: an old document with `suppress_zeros: true` normalizes to BOTH new
    /// flags true (see [`build_view`]); it is never written back (new documents
    /// emit only the two split fields).
    #[serde(default, skip_serializing)]
    suppress_zeros: Option<bool>,
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
        suppress_zero_rows: view.suppress_zero_rows,
        suppress_zero_columns: view.suppress_zero_columns,
        // Never written back; only read for back-compat on load.
        suppress_zeros: None,
        rows: axis_doc(&view.rows),
        columns: axis_doc(&view.columns),
        context,
    }
}

fn build_view(doc: &ViewDoc) -> View {
    // Back-compat: a pre-split document carries the single `suppress_zeros` flag
    // and neither new field. When present, it sets BOTH new flags (true -> both
    // true, false -> both false). A new document omits it, so the two split
    // fields stand on their own. A present legacy flag takes precedence over any
    // split fields in the same document; only a hand-edited or transitional file
    // would carry both, and real legacy data has no split fields.
    let (suppress_zero_rows, suppress_zero_columns) = match doc.suppress_zeros {
        Some(legacy) => (legacy, legacy),
        None => (doc.suppress_zero_rows, doc.suppress_zero_columns),
    };
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
        suppress_zero_rows,
        suppress_zero_columns,
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
struct FlowInputDoc {
    name: String,
    /// "global" (references a server-global connection by name) or "local" (an
    /// embedded flow-scoped connection).
    scope: String,
    /// For a local input, the embedded connection spec (its `name` mirrors the
    /// input `name`). Absent for a global input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection: Option<ConnectionDoc>,
}

#[derive(Serialize, Deserialize)]
struct FlowDoc {
    name: String,
    source: String,
    // Flow metadata (ADR-0035), all optional so a flow without them stays
    // byte-compatible with the pre-0035 single-field shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_cube: Option<String>,
    #[serde(default, rename = "input", skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<FlowInputDoc>,
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
    // The target cube the assertions check (ADR-0035); `None` uses the flow's
    // default cube.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cube: Option<String>,
    #[serde(default, rename = "param", skip_serializing_if = "Vec::is_empty")]
    params: Vec<ParamEntryDoc>,
    // Named-source contents for a multi-source flow (ADR-0035): address -> inline
    // content (the `name` field holds the source address, `value` the content).
    #[serde(default, rename = "source", skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<ParamEntryDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assertions: Vec<TestCellDoc>,
}

fn flow_doc(flow: &Flow) -> FlowDoc {
    FlowDoc {
        name: flow.name.clone(),
        source: flow.source.clone(),
        owner: flow.owner.clone(),
        default_cube: flow.default_cube.clone(),
        inputs: flow.inputs.iter().map(flow_input_doc).collect(),
    }
}

fn flow_input_doc(input: &FlowInput) -> FlowInputDoc {
    match &input.binding {
        FlowInputBinding::Global => FlowInputDoc {
            name: input.name.clone(),
            scope: "global".to_string(),
            connection: None,
        },
        FlowInputBinding::Local(spec) => FlowInputDoc {
            name: input.name.clone(),
            scope: "local".to_string(),
            connection: Some(connection_doc(&Connection {
                name: input.name.clone(),
                spec: spec.clone(),
            })),
        },
    }
}

fn build_flow(doc: &FlowDoc) -> Flow {
    Flow {
        name: doc.name.clone(),
        source: doc.source.clone(),
        owner: doc.owner.clone(),
        default_cube: doc.default_cube.clone(),
        inputs: doc.inputs.iter().map(build_flow_input).collect(),
    }
}

fn build_flow_input(doc: &FlowInputDoc) -> FlowInput {
    let binding = match doc.scope.as_str() {
        "local" => {
            // A local input embeds its connection spec; a malformed/absent block
            // degrades to an empty command spec rather than failing the load (the
            // model file is a full-trust boundary).
            let spec = doc
                .connection
                .as_ref()
                .map(|cd| build_connection(cd).spec)
                .unwrap_or_else(|| ConnectionSpec::Command(CommandSpec::default()));
            FlowInputBinding::Local(spec)
        }
        // "global" (and, forward-compatibly, any unknown scope) references a
        // server-global connection by name.
        _ => FlowInputBinding::Global,
    };
    FlowInput {
        name: doc.name.clone(),
        binding,
    }
}

fn flow_test_doc(test: &FlowTest) -> FlowTestDoc {
    FlowTestDoc {
        name: test.name.clone(),
        flow: test.flow.clone(),
        input: test.input.clone(),
        cube: test.cube.clone(),
        // Params and named sources sorted by name (BTreeMap iterates sorted) for
        // canonical output.
        params: test
            .params
            .iter()
            .map(|(name, value)| ParamEntryDoc {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
        inputs: test
            .inputs
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
        inputs: doc
            .inputs
            .iter()
            .map(|p| (p.name.clone(), p.value.clone()))
            .collect(),
        cube: doc.cube.clone(),
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
fn build_model_doc(
    cube: &Cube,
    subsets: Vec<SubsetDoc>,
    views: Vec<ViewDoc>,
    rules: Option<RuleSetDoc>,
    rule_tests: Vec<RuleTestDoc>,
    sandboxes: Vec<SandboxDoc>,
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
        sandboxes,
        // Parse-only (ADR-0035): never emitted; a cube model carries no automation.
        flows: Vec::new(),
        flow_tests: Vec::new(),
        connections: Vec::new(),
        jobs: Vec::new(),
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
        let sandboxes: Vec<SandboxDoc> = self
            .sandboxes
            .values()
            .map(|sb| sandbox_doc(&self.cube, sb))
            .collect();
        let doc = build_model_doc(&self.cube, subsets, views, rules, rule_tests, sandboxes);
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
        let mut sandboxes = BTreeMap::new();
        for sd in &doc.sandboxes {
            sandboxes.insert(sd.name.clone(), build_sandbox(&cube, &doc.cube.name, sd)?);
        }
        // Any `[[flow]]`/`[[flow_test]]`/`[[connection]]`/`[[job]]` blocks in an
        // old cube model are parsed but ignored here (ADR-0035): boot migration
        // lifts them into the global automation model via
        // [`extract_legacy_automation`].
        Ok(Model {
            cube,
            subsets,
            views,
            rules,
            tests,
            sandboxes,
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

/// The serialized shape of the server-global automation model (ADR-0035).
#[derive(Serialize, Deserialize)]
struct AutomationDoc {
    format: String,
    #[serde(default, rename = "flow", skip_serializing_if = "Vec::is_empty")]
    flows: Vec<FlowDoc>,
    #[serde(default, rename = "flow_test", skip_serializing_if = "Vec::is_empty")]
    flow_tests: Vec<FlowTestDoc>,
    #[serde(default, rename = "connection", skip_serializing_if = "Vec::is_empty")]
    connections: Vec<ConnectionDoc>,
    #[serde(default, rename = "job", skip_serializing_if = "Vec::is_empty")]
    jobs: Vec<JobDoc>,
}

/// Assemble an [`Automation`] from already-parsed doc collections.
fn automation_from_docs(
    flows: &[FlowDoc],
    flow_tests: &[FlowTestDoc],
    connections: &[ConnectionDoc],
    jobs: &[JobDoc],
) -> Automation {
    let mut a = Automation::new();
    for fd in flows {
        a.flows.insert(fd.name.clone(), build_flow(fd));
    }
    for ftd in flow_tests {
        a.flow_tests.insert(ftd.name.clone(), build_flow_test(ftd));
    }
    for cd in connections {
        a.connections.insert(cd.name.clone(), build_connection(cd));
    }
    for jd in jobs {
        a.jobs.insert(jd.name.clone(), build_job(jd));
    }
    a
}

impl Automation {
    /// Serialize the global automation model (flows, flow tests, connections,
    /// jobs) to canonical model-as-code TOML (ADR-0035). Each collection is
    /// emitted sorted by name (the `BTreeMap`s iterate sorted), so re-serializing
    /// a parsed model reproduces byte-identical text.
    pub fn to_model_text(&self) -> Result<String, SaveError> {
        let doc = AutomationDoc {
            format: AUTOMATION_FORMAT_TAG.to_string(),
            flows: self.flows.values().map(flow_doc).collect(),
            flow_tests: self.flow_tests.values().map(flow_test_doc).collect(),
            connections: self.connections.values().map(connection_doc).collect(),
            jobs: self.jobs.values().map(job_doc).collect(),
        };
        toml::to_string(&doc).map_err(SaveError::Toml)
    }

    /// Parse the global automation model from model-as-code TOML.
    pub fn from_model_text(text: &str) -> Result<Automation, LoadError> {
        let doc: AutomationDoc = toml::from_str(text).map_err(LoadError::Toml)?;
        if doc.format != AUTOMATION_FORMAT_TAG {
            return Err(LoadError::UnknownFormat(doc.format));
        }
        Ok(automation_from_docs(
            &doc.flows,
            &doc.flow_tests,
            &doc.connections,
            &doc.jobs,
        ))
    }

    /// Save the automation model to a model-as-code file (canonical TOML).
    pub fn save_to_path(&self, path: impl AsRef<std::path::Path>) -> Result<(), SaveError> {
        let text = self.to_model_text()?;
        std::fs::write(path, text).map_err(SaveError::Io)
    }

    /// Load the automation model from a model-as-code file.
    pub fn load_from_path(path: impl AsRef<std::path::Path>) -> Result<Automation, LoadError> {
        let text = std::fs::read_to_string(path).map_err(LoadError::Io)?;
        Automation::from_model_text(&text)
    }
}

/// Extract the legacy per-cube automation objects from an old cube model's text
/// (ADR-0035 migration): any `[[flow]]`/`[[flow_test]]`/`[[connection]]`/`[[job]]`
/// blocks become an [`Automation`]. Returns an empty automation if the text has
/// none. The caller lifts these into the global store and re-saves the cube model
/// (which no longer emits them).
pub fn extract_legacy_automation(text: &str) -> Result<Automation, LoadError> {
    let doc: ModelDoc = toml::from_str(text).map_err(LoadError::Toml)?;
    if doc.format != FORMAT_TAG {
        return Err(LoadError::UnknownFormat(doc.format));
    }
    Ok(automation_from_docs(
        &doc.flows,
        &doc.flow_tests,
        &doc.connections,
        &doc.jobs,
    ))
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
        let doc = build_model_doc(self, Vec::new(), Vec::new(), None, Vec::new(), Vec::new());
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
                suppress_zero_rows: true,
                suppress_zero_columns: false,
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
        let grid = model2.view("Grid").unwrap();
        assert!(grid.suppress_zero_rows);
        assert!(!grid.suppress_zero_columns);
    }

    #[test]
    fn view_suppress_zeros_back_compat() {
        // Old data: the single `suppress_zeros = true` flag must set BOTH split
        // flags on load.
        let old_true: ViewDoc = toml::from_str(
            "name = \"V\"\ncube = \"Sales\"\nvisibility = \"public\"\nsuppress_zeros = true\n",
        )
        .unwrap();
        let v = build_view(&old_true);
        assert!(v.suppress_zero_rows, "old true -> rows true");
        assert!(v.suppress_zero_columns, "old true -> columns true");

        // Old data: `suppress_zeros = false` -> both false. (Absent is the same:
        // serde defaults the legacy field to None, so neither split flag is set.)
        let old_false: ViewDoc = toml::from_str(
            "name = \"V\"\ncube = \"Sales\"\nvisibility = \"public\"\nsuppress_zeros = false\n",
        )
        .unwrap();
        let v = build_view(&old_false);
        assert!(!v.suppress_zero_rows, "old false -> rows false");
        assert!(!v.suppress_zero_columns, "old false -> columns false");

        let absent: ViewDoc =
            toml::from_str("name = \"V\"\ncube = \"Sales\"\nvisibility = \"public\"\n").unwrap();
        let v = build_view(&absent);
        assert!(
            !v.suppress_zero_rows && !v.suppress_zero_columns,
            "absent -> both false"
        );

        // A document that carries BOTH the legacy flag and a split field (only a
        // hand-edited or transitional file would) resolves legacy-wins: the present
        // legacy flag overrides the split fields. Genuine legacy data never carries
        // split fields, so this only pins the documented precedence against a refactor.
        let mixed: ViewDoc = toml::from_str(
            "name = \"V\"\ncube = \"Sales\"\nvisibility = \"public\"\nsuppress_zeros = false\nsuppress_zero_rows = true\n",
        )
        .unwrap();
        let v = build_view(&mixed);
        assert!(
            !v.suppress_zero_rows && !v.suppress_zero_columns,
            "legacy flag wins over split fields when both are present"
        );

        // New data with the two split fields round-trips independently.
        let new = View {
            name: "V".into(),
            cube: "Sales".into(),
            owner: None,
            visibility: Visibility::Public,
            rows: Vec::new(),
            columns: Vec::new(),
            context: Vec::new(),
            suppress_zero_rows: true,
            suppress_zero_columns: false,
        };
        let doc = view_doc(&new);
        // The legacy field is never written back; only the split fields persist.
        assert!(doc.suppress_zeros.is_none());
        let back = build_view(&doc);
        assert!(back.suppress_zero_rows, "new rows:true round-trips");
        assert!(!back.suppress_zero_columns, "new cols:false round-trips");
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
    fn automation_round_trips_flows_and_flow_tests() {
        use crate::{Automation, Flow, FlowTest, TestCell};

        let mut automation = Automation::new();
        automation.flows.insert(
            "load".to_string(),
            Flow {
                name: "load".to_string(),
                source: "export function rows(ctx: FlowContext): void {\n  for (const r of ctx.input()) ctx.writeCells([]);\n}".to_string(),
                owner: Some("ann".to_string()),
                default_cube: Some("Sales".to_string()),
                inputs: Vec::new(),
            },
        );
        let mut params = std::collections::BTreeMap::new();
        params.insert("version".to_string(), "Actual".to_string());
        params.insert("scale".to_string(), "1000".to_string());
        let mut coord = std::collections::BTreeMap::new();
        coord.insert("Region".to_string(), "North".to_string());
        coord.insert("Version".to_string(), "Actual".to_string());
        automation.flow_tests.insert(
            "load_test".to_string(),
            FlowTest {
                name: "load_test".to_string(),
                flow: "load".to_string(),
                input: "Region,Value\nNorth,100\n".to_string(),
                inputs: std::collections::BTreeMap::new(),
                cube: Some("Sales".to_string()),
                params,
                assertions: vec![TestCell {
                    coord,
                    value: "100".to_string(),
                }],
            },
        );

        let text1 = automation.to_model_text().unwrap();
        let a2 = Automation::from_model_text(&text1).unwrap();
        let text2 = a2.to_model_text().unwrap();
        assert_eq!(
            text1, text2,
            "flows and flow tests must round-trip byte-identically"
        );
        assert_eq!(a2.flows.len(), 1);
        assert!(a2.flows["load"].source.contains("writeCells"));
        assert_eq!(a2.flows["load"].owner.as_deref(), Some("ann"));
        assert_eq!(a2.flows["load"].default_cube.as_deref(), Some("Sales"));
        assert_eq!(a2.flow_tests.len(), 1);
        let ft = &a2.flow_tests["load_test"];
        assert_eq!(ft.flow, "load");
        assert_eq!(ft.input, "Region,Value\nNorth,100\n");
        assert_eq!(ft.cube.as_deref(), Some("Sales"));
        assert_eq!(ft.params["version"], "Actual");
        assert_eq!(ft.params["scale"], "1000");
        assert_eq!(ft.params.len(), 2);
        assert_eq!(ft.assertions[0].value, "100");
    }

    #[test]
    fn automation_round_trips_flow_inputs_global_and_local() {
        use crate::{
            Automation, CommandSpec, ConnectionSpec, Flow, FlowInput, FlowInputBinding,
            SourceFormat,
        };

        let mut automation = Automation::new();
        automation.flows.insert(
            "join".to_string(),
            Flow {
                name: "join".to_string(),
                source: "function rows(ctx) {}".to_string(),
                owner: None,
                default_cube: None,
                inputs: vec![
                    FlowInput {
                        name: "sales_db".to_string(),
                        binding: FlowInputBinding::Global,
                    },
                    FlowInput {
                        name: "daily_csv".to_string(),
                        binding: FlowInputBinding::Local(ConnectionSpec::Command(CommandSpec {
                            program: "cat".to_string(),
                            args: vec!["daily.csv".to_string()],
                            format: SourceFormat::Csv,
                            json_path: None,
                            timeout_ms: 5_000,
                            working_dir: None,
                        })),
                    },
                ],
            },
        );

        let text1 = automation.to_model_text().unwrap();
        let a2 = Automation::from_model_text(&text1).unwrap();
        assert_eq!(text1, a2.to_model_text().unwrap());
        let inputs = &a2.flows["join"].inputs;
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].name, "sales_db");
        assert!(matches!(inputs[0].binding, FlowInputBinding::Global));
        assert_eq!(inputs[1].name, "daily_csv");
        let FlowInputBinding::Local(ConnectionSpec::Command(cmd)) = &inputs[1].binding else {
            panic!("expected a local command connection");
        };
        assert_eq!(cmd.program, "cat");
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
    fn automation_round_trips_connections() {
        use crate::{Automation, CommandSpec, Connection, ConnectionSpec, SourceFormat};

        let mut automation = Automation::new();
        automation.connections.insert(
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

        let text1 = automation.to_model_text().unwrap();
        let model2 = Automation::from_model_text(&text1).unwrap();
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
    fn automation_round_trips_http_connections() {
        use crate::{
            Automation, Connection, ConnectionSpec, HttpAuth, HttpAuthKind, HttpSpec, SourceFormat,
        };

        let mut automation = Automation::new();
        automation.connections.insert(
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

        let text1 = automation.to_model_text().unwrap();
        // The model text references the secret by NAME, never the value (RG-13).
        assert!(text1.contains("rates_token"));
        let model2 = Automation::from_model_text(&text1).unwrap();
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
    fn automation_round_trips_sql_connections() {
        use crate::{Automation, Connection, ConnectionSpec, SqlEngine, SqlSpec, SqlSslMode};

        let mut automation = Automation::new();
        automation.connections.insert(
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

        let text1 = automation.to_model_text().unwrap();
        // The model text references the password by secret NAME, never a value.
        assert!(text1.contains("warehouse_pw"));
        let model2 = Automation::from_model_text(&text1).unwrap();
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
    fn automation_round_trips_mysql_engine() {
        use crate::{Automation, Connection, ConnectionSpec, SqlEngine, SqlSpec, SqlSslMode};

        let mut automation = Automation::new();
        automation.connections.insert(
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
        let text1 = automation.to_model_text().unwrap();
        assert!(text1.contains("mysql"), "engine token must serialize");
        let model2 = Automation::from_model_text(&text1).unwrap();
        assert_eq!(text1, model2.to_model_text().unwrap());
        let ConnectionSpec::Sql(sql) = &model2.connections["mariadb"].spec else {
            panic!("expected a sql connection");
        };
        assert_eq!(sql.engine, SqlEngine::MySql);
        assert_eq!(sql.port, 3306);
    }

    #[test]
    fn automation_round_trips_jobs() {
        use crate::{Automation, Job, Trigger};

        let mut automation = Automation::new();
        automation.jobs.insert(
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

        let text1 = automation.to_model_text().unwrap();
        let model2 = Automation::from_model_text(&text1).unwrap();
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
