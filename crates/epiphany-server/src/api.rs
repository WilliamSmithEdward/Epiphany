//! The routes, their request and response shapes, and the OpenAPI document generated from them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{
    Arc, LockResult, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Extension;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, FromRef, FromRequest, Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use epiphany_core::Scalar;
use epiphany_core::database::{
    Asked, CellChange, CellRef, CellRefs, CellWrite, CellWrites, Comment, Database, EarlierModel,
    Error, Explanation, HistoryRead, Line, LockMode, ModelUpdate, Sandbox, Spread, TransactionId,
};
use epiphany_core::mdx::QueryResult;
use epiphany_core::model::{
    AttributeType, ChildDef, CubeDef, DashboardDef, Dimension, DimensionDef, DimensionType,
    ElementId, ElementType, ModelDef, ViewDef, key,
};
use serde::{Deserialize, Serialize};

use crate::auth::{Admitted, AuditEntry, Auth, Grants, Refusal, Role, UserView};
use crate::private::Refused;
use utoipa::openapi::RefOr;
use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityRequirement, SecurityScheme};
use utoipa::{IntoParams, OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

type Shared = Arc<Guarded>;

/// The database, and a turn that whoever writes to it takes first.
///
/// A snapshot holds the turn while it is written, reading the database. A writer that
/// waited for the database itself meanwhile would hold up every reader behind it, for the
/// lock lets nobody new in once a writer is waiting; waiting for the turn, it holds up nobody.
struct Guarded {
    database: RwLock<Database>,
    turn: Mutex<()>,
}

/// The database held for writing, and the turn taken for it, given back in that order.
struct Writing<'a> {
    database: RwLockWriteGuard<'a, Database>,
    _turn: MutexGuard<'a, ()>,
}

impl Guarded {
    fn new(database: Database) -> Shared {
        Arc::new(Guarded {
            database: RwLock::new(database),
            turn: Mutex::new(()),
        })
    }

    fn read(&self) -> LockResult<RwLockReadGuard<'_, Database>> {
        self.database.read()
    }

    /// Takes the turn, then the database. Fails as a poisoned lock does, after a write that
    /// stopped part of the way through.
    fn write(&self) -> Writing<'_> {
        let turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        Writing {
            database: self.database.write().unwrap(),
            _turn: turn,
        }
    }

    /// Takes the turn and the database for a user: what they begin is signed with their
    /// name, for the history of cells.
    fn write_by(&self, user: &str) -> Writing<'_> {
        let mut writing = self.write();
        writing.database.sign(Some(user));
        writing
    }

    /// Writes a snapshot if a commit has called for one, in the turn and holding the
    /// database for reading, so that others go on reading meanwhile.
    fn snapshot_if_wanted(&self) {
        let _turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        if let Ok(database) = self.database.read() {
            database.snapshot_if_wanted();
        }
    }
}

/// The database given back unsigned, so that whoever writes next is not taken for the last.
impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.database.sign(None);
    }
}

impl std::ops::Deref for Writing<'_> {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.database
    }
}

impl std::ops::DerefMut for Writing<'_> {
    fn deref_mut(&mut self) -> &mut Database {
        &mut self.database
    }
}

/// What every request can reach: the database, and the users if the server has any.
#[derive(Clone)]
struct AppState {
    database: Shared,
    auth: Option<Arc<Auth>>,
    /// The model's version, kept beside the database so that it can be read without waiting
    /// for whoever is writing.
    version: Arc<AtomicU64>,
    /// When the server started.
    started: Instant,
}

impl FromRef<AppState> for Shared {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.database)
    }
}

#[derive(OpenApi)]
#[openapi(info(
    title = "Epiphany",
    description = "An in-memory OLAP server. Every response carries the header Epiphany-Model-Version, the version of the model it was answered from, so that a client holding a copy of a dimension knows when to fetch it again."
))]
struct Contract;

/// The name of the security scheme in the contract.
const SESSION: &str = "session";

/// The most bytes a request that writes cells or changes the model may carry: room for a
/// model of large dimensions, or some hundreds of thousands of cells in one write. A larger
/// body is refused before it is read.
const MOST_BODY: usize = 64 * 1024 * 1024;
/// The most a request that only asks for something may carry: room for a query of any
/// length a person would write, or tens of thousands of cells to read. Bodies wait in
/// memory, and those who may only read are not given the room that writing needs.
const MOST_ASKED: usize = 4 * 1024 * 1024;
/// The most a request to sign in may carry. It is the one request that anybody may send, so
/// it is given room for a name and a password and no more.
const MOST_SIGN_IN: usize = 16 * 1024;

fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(Contract::openapi())
        .routes(routes!(sign_in, sign_out))
        .routes(routes!(get_model, put_model))
        .routes(routes!(get_outline))
        .routes(routes!(get_dimension, put_dimension, delete_dimension))
        .routes(routes!(get_cube, put_cube, delete_cube))
        .routes(routes!(read_cells))
        .routes(routes!(write_cells))
        .routes(routes!(write_detail))
        .routes(routes!(explain_cells))
        .routes(routes!(cell_history))
        .routes(routes!(spread_cells))
        .routes(routes!(query))
        .routes(routes!(stored_cells))
        .routes(routes!(run_view))
        .routes(routes!(check_feeders))
        .routes(routes!(begin_transaction))
        .routes(routes!(commit_transaction))
        .routes(routes!(rollback_transaction))
        .routes(routes!(transaction_point))
        .routes(routes!(undo_transaction))
        .routes(routes!(lock))
        .routes(routes!(list_sandboxes, create_sandbox))
        .routes(routes!(delete_sandbox))
        .routes(routes!(list_private_views))
        .routes(routes!(put_private_view, delete_private_view))
        .routes(routes!(list_private_subsets))
        .routes(routes!(put_private_subset, delete_private_subset))
        .routes(routes!(put_dashboard, delete_dashboard))
        .routes(routes!(server_status))
        .routes(routes!(list_users))
        .routes(routes!(put_user, delete_user))
        .routes(routes!(end_sessions))
        .routes(routes!(view_as))
        .routes(routes!(read_audit))
        .routes(routes!(list_transactions))
        .routes(routes!(list_earlier_models))
        .routes(routes!(list_changes))
        .routes(routes!(get_earlier_model))
        .routes(routes!(list_comments, add_comment))
        .routes(routes!(remove_comment))
}

/// The OpenAPI document describing every route.
pub fn openapi() -> utoipa::openapi::OpenApi {
    secured(router().into_openapi())
}

/// Adds to the contract what holds for every route but signing in: a session token is sent,
/// and without a live one the answer is 401.
fn secured(mut contract: utoipa::openapi::OpenApi) -> utoipa::openapi::OpenApi {
    let bearer = Http::new(HttpAuthScheme::Bearer);
    let components = contract.components.get_or_insert_default();
    components.add_security_scheme(SESSION, SecurityScheme::Http(bearer));
    contract.security = Some(vec![SecurityRequirement::new(SESSION, [""; 0])]);

    // Every refusal has the same shape, so the 401 is modelled on one already described.
    let read = contract.paths.paths["/cells/read"]
        .post
        .as_ref()
        .expect("the route exists");
    let mut refusal = read.responses.responses["422"].clone();
    if let RefOr::T(response) = &mut refusal {
        response.description = "Not signed in, or the session has ended".to_owned();
    }
    let mut forbidden = refusal.clone();
    if let RefOr::T(response) = &mut forbidden {
        response.description = "The user may not do this".to_owned();
    }
    for (path, item) in &mut contract.paths.paths {
        let operations = [
            (Method::GET, &mut item.get),
            (Method::PUT, &mut item.put),
            (Method::DELETE, &mut item.delete),
            (Method::POST, &mut item.post),
        ];
        for (method, operation) in operations {
            let signing_in = method == Method::POST && path == "/sessions";
            let Some(operation) = operation.as_mut().filter(|_| !signing_in) else {
                continue;
            };
            let responses = &mut operation.responses.responses;
            responses.insert("401".to_owned(), refusal.clone());
            responses.insert("403".to_owned(), forbidden.clone());
        }
    }
    contract
}

/// The HTTP application serving a database to anyone who can reach it.
pub fn app(database: Database) -> Router {
    app_with(database, None)
}

/// The HTTP application serving a database, to signed-in users only if it is given any.
pub fn app_with(mut database: Database, auth: Option<Auth>) -> Router {
    // A transaction id is all that is needed to use a transaction, so it must not be
    // guessable. Fifty-three random bits survive a trip through JSON and JavaScript.
    // A snapshot is taken after the request whose commit filled the log, holding the
    // database for reading only, rather than inside the commit, holding it for writing.
    database.defer_snapshots();
    database.set_transaction_ids(|| {
        getrandom::u64().expect("the operating system provides randomness") >> 11
    });
    let state = AppState {
        version: Arc::new(AtomicU64::new(database.model_version())),
        database: Guarded::new(database),
        auth: auth.map(Arc::new),
        started: Instant::now(),
    };
    let (router, contract) = router().split_for_parts();
    let contract = secured(contract);
    // Nearest the routes comes the handing over, so that a request is given a thread only
    // once it has been let in and has arrived whole.
    router
        .layer(from_fn_with_state(state.clone(), survive))
        .layer(from_fn_with_state(state.clone(), require_session))
        .layer(from_fn_with_state(state.clone(), stamp))
        .route("/openapi.json", get(async move || Json(contract)))
        .layer(DefaultBodyLimit::max(MOST_BODY))
        .with_state(state)
}

/// Answers a request whose handling went wrong in a way nobody foresaw, which would
/// otherwise close the connection without a word, and keeps serving the others.
///
/// One thing cannot be carried on from: a write that stopped part of the way through. What
/// is in memory is then in no known state and every later request would fail on it, so the
/// server stops, to be started again from what is on disk, where only whole commits are.
async fn survive(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let database = &state.database;
    // The body is taken in first, here where waiting for it holds up nothing: a request
    // handed over while its body was still on its way would keep a thread for as long as
    // the sender cared to take, and a few hundred senders in no hurry would have them all.

    let most = match (request.method(), request.uri().path()) {
        (_, "/sessions") => MOST_SIGN_IN,
        // A user's own view or subset, which a reader may keep, is given the room of a question.
        (&Method::PUT, path) if path.contains("/private-") => MOST_ASKED,
        (&Method::PUT, _) | (_, "/cells/write") => MOST_BODY,
        _ => MOST_ASKED,
    };
    let (head, body) = request.into_parts();
    let Ok(body) = axum::body::to_bytes(body, most).await else {
        let message = format!("the request body is longer than {most} bytes, or did not arrive");
        return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "invalid_request", message)
            .into_response();
    };
    let request = Request::from_parts(head, axum::body::Body::from(body));
    // Run apart, so that its failure is this request's and nobody else's, and on a thread
    // of its own kind: a query may work for seconds, and the few threads that take in
    // requests would otherwise all be busy with slow ones while a quick one waited. The
    // hand-over costs a small request about 25 microseconds (2026-11-21). Keeping the
    // request where it is and telling the runtime so (`block_in_place`) cost 7, and left
    // some of the adapter's requests unanswered.
    let runtime = tokio::runtime::Handle::current();
    // Only these change the model. Its version is noted afresh after one of them, there on
    // the thread that may wait; never set back, should two of them finish out of turn.
    let changes = matches!(*request.method(), Method::PUT | Method::DELETE);
    let (shared, version) = (Arc::clone(database), Arc::clone(&state.version));
    let answer = tokio::task::spawn_blocking(move || {
        let response = runtime.block_on(next.run(request));
        let wanted = shared.read().is_ok_and(|database| {
            if changes {
                version.fetch_max(database.model_version(), Ordering::Relaxed);
            }
            database.wants_snapshot()
        });
        // A snapshot a commit has called for is written in a task of its own: others go on
        // reading, only writers wait, and no request's answer waits for it. Taken on the way
        // out of a request, it held up that request, whichever it was, by the seconds a
        // snapshot of millions of cells takes.
        if wanted {
            runtime.spawn_blocking(move || shared.snapshot_if_wanted());
        }
        response
    });
    if let Ok(response) = answer.await {
        return response;
    }
    if database.database.is_poisoned() {
        eprintln!("A write failed part of the way through. Stopping; start the server again.");
        std::process::exit(70);
    }
    let message = "the server failed to answer this request".to_owned();
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message).into_response()
}

/// Adds a test page at `/lab` to an application, read from disk on every request so that it
/// can be edited while the server runs.
pub fn with_lab(application: Router, page: PathBuf) -> Router {
    let serve = move || {
        let page = std::fs::read_to_string(&page);
        let missing = |error: std::io::Error| (StatusCode::NOT_FOUND, error.to_string());
        async move { page.map(Html).map_err(missing) }
    };
    application.route("/lab", get(serve))
}

/// The session token a request carries, if it carries one.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// The header every response carries: the version of the model it was answered from.
const MODEL_VERSION: &str = "epiphany-model-version";

/// Puts the model's version on a response, refusals included.
async fn stamp(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    // Read from beside the database: this runs on a thread that takes requests in, which
    // must not be kept waiting for a write to finish.
    let version = state.version.load(Ordering::Relaxed);
    response
        .headers_mut()
        .insert(MODEL_VERSION, HeaderValue::from(version));
    response
}

/// What a user must be allowed to do to call a route.
fn needed(method: &Method, path: &str) -> Role {
    if path.starts_with("/admin/") {
        return Role::Admin;
    }
    // Commenting on a cube's cells takes writing to it; taking a comment away, its author
    // or an administrator, as the route says.
    let segments: Vec<&str> = path.split('/').collect();
    if let ["", "cubes", _, "comments", ..] = segments.as_slice()
        && method != Method::GET
    {
        return Role::Write;
    }
    // A sandbox is its owner's own, to write values into: listing them takes reading, and
    // making or deleting one writing.
    if segments.get(1) == Some(&"sandboxes") {
        return match method == Method::GET {
            true => Role::Read,
            false => Role::Write,
        };
    }
    // A user's own views and subsets are theirs to keep and take away, with no more than
    // reading.
    if let ["", "cubes", _, "private-views", ..] | ["", "dimensions", _, "private-subsets", ..] =
        segments.as_slice()
    {
        return Role::Read;
    }
    let reads = ["/cells/read", "/cells/explain", "/cells/history", "/mdx"];
    if method == Method::GET || path == "/sessions" || reads.contains(&path) {
        Role::Read
    } else if method == Method::PUT || method == Method::DELETE {
        // Every other PUT or DELETE changes the model.
        Role::Admin
    } else {
        Role::Write
    }
}

/// Lets a request through if the server has no users, if it is a sign-in, or if it carries
/// the token of a live session whose user may do what the route does.
async fn require_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let signing_in = request.method() == Method::POST && request.uri().path() == "/sessions";
    let Some(auth) = state.auth.as_ref().filter(|_| !signing_in) else {
        // A server without users lets everyone do everything.
        request.extensions_mut().insert(Arc::new(Grants::default()));
        request
            .extensions_mut()
            .insert(Signed(OPEN_USER.to_owned()));
        return next.run(request).await;
    };
    let admitted = bearer(request.headers()).and_then(|token| auth.admits(token));
    let needed = needed(request.method(), request.uri().path());
    if let Some(Admitted {
        name,
        grants,
        viewer,
    }) = admitted
    {
        // The model belongs to no cube, so changing it takes the user's own role. Anything
        // else is let in by a role on any cube; the route then asks about the cube it names.
        let held = match needed {
            Role::Admin => grants.role(),
            _ => grants.most(),
        };
        if held < needed {
            let message = match viewer {
                Some(_) => format!("a view as {name} only reads; this needs the role '{needed}'"),
                None => format!("this needs a user with the role '{needed}'"),
            };
            return ApiError::new(StatusCode::FORBIDDEN, "forbidden", message).into_response();
        }
        request.extensions_mut().insert(grants);
        request.extensions_mut().insert(Signed(name));
        if viewer.is_some() {
            request.extensions_mut().insert(Viewing);
        }
        return next.run(request).await;
    }
    let refusal = ApiError::new(
        StatusCode::UNAUTHORIZED,
        "not_signed_in",
        "sign in at POST /sessions and send the token as 'Authorization: Bearer <token>'"
            .to_owned(),
    );
    ([(WWW_AUTHENTICATE, "Bearer")], refusal).into_response()
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct SignIn {
    name: String,
    password: String,
}

#[derive(Serialize, ToSchema)]
struct Session {
    /// Send this with every request, as `Authorization: Bearer <token>`.
    token: String,
    /// The session ends when it has not been used for this many seconds.
    idle_seconds: u64,
    /// What the user may do.
    role: Role,
    /// The cubes on which the user has another role in place of that one.
    cubes: BTreeMap<String, Role>,
    /// For a view as another user, the administrator viewing.
    #[serde(skip_serializing_if = "Option::is_none")]
    viewer: Option<String>,
}

/// Signs in, starting a session.
#[utoipa::path(
    post,
    path = "/sessions",
    request_body = SignIn,
    security(()),
    responses(
        (status = CREATED, body = Session, description = "Signed in"),
        (status = UNAUTHORIZED, body = ApiError, description = "Wrong name or password"),
        (status = TOO_MANY_REQUESTS, body = ApiError, description = "The name has been given too many wrong passwords, or too many sign-ins are waiting to be checked; the Retry-After header says how many seconds to wait"),
        (status = NOT_FOUND, body = ApiError, description = "The server has no users, so nothing needs signing in to"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The request body is malformed")
    )
)]
async fn sign_in(State(state): State<AppState>, Body(request): Body<SignIn>) -> Response {
    let Some(auth) = state.auth else {
        let message = "this server has no users, so nothing needs signing in to".to_owned();
        return ApiError::new(StatusCode::NOT_FOUND, "not_found", message).into_response();
    };
    let idle_seconds = auth.idle().as_secs();
    // A place in the queue, or none if it is full; then a turn at being checked.
    let Ok(_place) = auth.waiting.try_acquire() else {
        let message = "too many sign-ins are waiting; try again in a moment".to_owned();
        let refusal = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "busy", message);
        return ([(RETRY_AFTER, "1")], refusal).into_response();
    };
    let _turn = auth.checking.acquire().await.expect("never closed");
    let auth = Arc::clone(&auth);
    // Checking a password is deliberately slow, so it is kept off the threads that serve
    // other requests.
    let checked =
        tokio::task::spawn_blocking(move || auth.sign_in(&request.name, &request.password));
    match checked.await.unwrap_or(Err(Refusal::Wrong)) {
        Ok((token, grants)) => {
            let session = Session {
                token,
                idle_seconds,
                role: grants.role(),
                cubes: grants.cubes().clone(),
                viewer: None,
            };
            (StatusCode::CREATED, Json(session)).into_response()
        }
        Err(Refusal::Wrong) => ApiError::new(
            StatusCode::UNAUTHORIZED,
            "wrong_credentials",
            "wrong name or password".to_owned(),
        )
        .into_response(),
        Err(Refusal::Paused(pause)) => {
            let seconds = pause.as_secs() + 1;
            let refusal = ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_attempts",
                format!("too many wrong passwords for this name; try again in {seconds} seconds"),
            );
            ([(RETRY_AFTER, seconds.to_string())], refusal).into_response()
        }
    }
}

/// Signs out, ending the session whose token is sent.
#[utoipa::path(
    delete,
    path = "/sessions",
    responses((status = NO_CONTENT, description = "Signed out"))
)]
async fn sign_out(State(state): State<AppState>, headers: HeaderMap) -> StatusCode {
    if let (Some(auth), Some(token)) = (&state.auth, bearer(&headers)) {
        auth.sign_out(token);
    }
    StatusCode::NO_CONTENT
}

/// The one error shape used by every route.
#[derive(Debug, Serialize, ToSchema)]
struct ApiError {
    #[serde(skip)]
    status: StatusCode,
    /// A stable identifier for the kind of error.
    code: &'static str,
    message: String,
    /// The position of the offending cell in a batch.
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<usize>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: String) -> Self {
        Self {
            status,
            code,
            message,
            index: None,
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let status = match error {
            Error::Cell(_)
            | Error::Write { .. }
            | Error::Model(_)
            | Error::Mdx(_)
            | Error::Rule(_)
            | Error::Comment(_)
            | Error::Sandbox(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Error::UnknownTransaction(_) | Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Locked(_) | Error::StaleVersion { .. } => StatusCode::CONFLICT,
            Error::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let index = match error {
            Error::Write { index, .. } => Some(index),
            _ => None,
        };
        Self {
            index,
            ..Self::new(status, error.code(), error.to_string())
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

/// A JSON request body whose rejection uses the common error shape.
struct Body<T>(T);

impl<S: Send + Sync, T> FromRequest<S> for Body<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        match Json::from_request(request, state).await {
            Ok(Json(body)) => Ok(Self(body)),
            Err(rejection) => Err(ApiError::new(
                rejection.status(),
                "invalid_request",
                rejection.body_text(),
            )),
        }
    }
}

/// The model definition and its version.
#[derive(Serialize, ToSchema)]
struct ModelView {
    /// Counts up by one with every update.
    version: u64,
    model: ModelDef,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ModelPut {
    /// The version the new definition was edited from. The update is refused if it is no
    /// longer the current version.
    based_on: u64,
    model: ModelDef,
}

/// A dimension with the hierarchy facts derived from its definition.
#[derive(Serialize, ToSchema)]
struct DimensionView {
    name: String,
    #[serde(rename = "type")]
    kind: DimensionType,
    /// The names of the levels, from the leaves up, where the model gives them.
    levels: Vec<String>,
    attributes: BTreeMap<String, AttributeType>,
    /// Elements in declaration order.
    elements: Vec<ElementView>,
    /// The element names each subset selects, in the subset's order; for a subset that reads
    /// cells, as the stored cells stand now.
    subsets: BTreeMap<String, Vec<String>>,
    /// The subsets that read cells, which select what the cells say when they are used.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    subsets_reading_cells: Vec<String>,
}

/// An element and its place in the hierarchy. Its parents, children and attributes are left
/// out where it has none, as a leaf has no children: in a large dimension, most of the
/// elements are leaves, and the empty lists were half of what was sent.
#[derive(Serialize, ToSchema)]
struct ElementView {
    name: String,
    #[serde(rename = "type")]
    kind: ElementType,
    /// Zero for a leaf, otherwise one more than the highest child level.
    level: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parents: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<ChildDef>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    attributes: BTreeMap<String, Scalar>,
    /// For a string element with a picklist, the values its text cells may be given.
    #[serde(skip_serializing_if = "Option::is_none")]
    picklist: Option<Vec<String>>,
}

impl From<&Dimension> for DimensionView {
    fn from(dimension: &Dimension) -> Self {
        let name = |id: ElementId| dimension.elements[id as usize].name.clone();
        let child = |&(id, weight): &(ElementId, f64)| ChildDef {
            name: name(id),
            weight,
        };
        let elements = dimension.elements.iter().map(|element| {
            let values = dimension.attributes.iter().zip(&element.attributes);
            ElementView {
                name: element.name.clone(),
                kind: element.kind,
                level: element.level,
                parents: element.parents.iter().map(|&id| name(id)).collect(),
                children: element.children.iter().map(child).collect(),
                attributes: values
                    .filter_map(|(attribute, value)| Some((attribute.name.clone(), value.clone()?)))
                    .collect(),
                picklist: element.picklist.clone(),
            }
        });
        Self {
            name: dimension.name.clone(),
            kind: dimension.kind,
            levels: dimension.levels.clone(),
            attributes: dimension
                .attributes
                .iter()
                .map(|attribute| (attribute.name.clone(), attribute.kind))
                .collect(),
            elements: elements.collect(),
            subsets: dimension
                .subsets
                .iter()
                .map(|subset| {
                    let names = subset.elements.iter().map(|&id| name(id));
                    (subset.name.clone(), names.collect())
                })
                .collect(),
            subsets_reading_cells: dimension
                .subsets
                .iter()
                .filter(|subset| subset.reads_cells())
                .map(|subset| subset.name.clone())
                .collect(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    /// Read within this transaction, seeing its pending writes.
    transaction: Option<u64>,
    #[schema(value_type = Vec<CellRef>)]
    cells: CellRefs,
}

#[derive(Serialize, ToSchema)]
struct ReadResponse {
    /// One result per requested cell, in request order.
    cells: Vec<CellResult>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum CellResult {
    Value(Scalar),
    Error(ApiError),
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct WriteRequest {
    /// Hold the writes in this transaction until it commits. Without it they are committed
    /// at once.
    transaction: Option<u64>,
    #[schema(value_type = Vec<CellWrite>)]
    cells: CellWrites,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct QueryRequest {
    /// Query within this transaction, seeing its pending writes.
    transaction: Option<u64>,
    /// The query text, for example `SELECT {[Entity].[US]} ON COLUMNS FROM [General Ledger]`.
    mdx: String,
    /// Members to stand at, each in place of the member the query's WHERE names for its
    /// dimension, or beside them. An axis that places one of their dimensions decides it.
    /// A page's selectors use it to show one saved query at other members.
    #[serde(default)]
    context: Vec<ContextMember>,
}

/// A member to stand at, by its dimension and its name or alias.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ContextMember {
    dimension: String,
    member: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ExplainRequest {
    /// Explain within this transaction, seeing its pending writes.
    transaction: Option<u64>,
    cells: Vec<CellRef>,
    /// Also say which feeders feed each cell and from where. This looks at every stored and
    /// fed cell for each cell asked about, so ask it of few cells at a time.
    #[serde(default)]
    feeders: bool,
}

#[derive(Serialize, ToSchema)]
struct ExplainResponse {
    /// One result per requested cell, in request order.
    cells: Vec<ExplainResult>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum ExplainResult {
    Cell(Explanation),
    Error(ApiError),
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct HistoryRequest {
    cube: String,
    /// One element name or alias for each dimension of the cube.
    at: Vec<String>,
    /// At most this many changes, the newest: 100 unless given, and never more than 1,000.
    most: Option<usize>,
}

#[derive(Serialize, ToSchema)]
struct HistoryResponse {
    /// The newest first.
    changes: Vec<CellChange>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct SpreadRequest {
    /// Hold the writes in this transaction until it commits. Without it they are committed
    /// at once.
    transaction: Option<u64>,
    spread: Spread,
}

#[derive(Serialize, ToSchema)]
struct SpreadResult {
    /// How many leaf cells were written.
    written: usize,
}

#[derive(Serialize, ToSchema)]
struct FeederCheck {
    /// Calculated leaf cells that hold a value but are not fed.
    unfed: Vec<CellRef>,
}

#[derive(Serialize, ToSchema)]
struct TransactionView {
    transaction: u64,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct LockRequest {
    /// The cube to lock. Without it the whole server is locked.
    cube: Option<String>,
    mode: LockMode,
}

/// A dimension as the outline of the model gives it: by its name, type and size.
#[derive(Serialize, ToSchema)]
struct DimensionOutline {
    name: String,
    #[serde(rename = "type")]
    kind: DimensionType,
    /// How many elements it has.
    element_count: usize,
}

/// The model in outline: its cubes and dashboards whole, and its dimensions without their
/// elements, so that it stays small however large the dimensions grow.
#[derive(Serialize, ToSchema)]
struct ModelOutline {
    /// Counts up by one with every update.
    version: u64,
    dimensions: Vec<DimensionOutline>,
    cubes: Vec<CubeDef>,
    dashboards: Vec<DashboardDef>,
}

/// The model in outline: every cube, with its rules, views and checks, and every dashboard,
/// but each dimension only by its name, type and number of elements. What lists the cubes
/// and views asks for this; `GET /dimensions/{name}` gives a dimension whole.
#[utoipa::path(get, path = "/model/outline", responses((status = OK, body = ModelOutline, description = "The model, without its dimensions' elements")))]
async fn get_outline(State(database): State<Shared>) -> Json<ModelOutline> {
    let database = database.read().unwrap();
    let definition = database.definition();
    let dimensions = definition
        .dimensions
        .iter()
        .map(|dimension| DimensionOutline {
            name: dimension.name.clone(),
            kind: dimension.kind,
            element_count: dimension.elements.len(),
        });
    Json(ModelOutline {
        version: database.model_version(),
        dimensions: dimensions.collect(),
        cubes: definition.cubes.clone(),
        dashboards: definition.dashboards.clone(),
    })
}

/// The model definition and its version.
#[utoipa::path(get, path = "/model", responses((status = OK, body = ModelView, description = "The model")))]
async fn get_model(State(database): State<Shared>) -> Json<ModelView> {
    let database = database.read().unwrap();
    Json(ModelView {
        version: database.model_version(),
        model: database.definition().clone(),
    })
}

/// Replaces the model in one step. Stored cells that still fit the new model are kept.
#[utoipa::path(
    put,
    path = "/model",
    request_body = ModelPut,
    responses(
        (status = OK, body = ModelUpdate, description = "The model was replaced"),
        (status = CONFLICT, body = ApiError, description = "Stale version, or a transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The definition is invalid, or the request body is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn put_model(
    State(database): State<Shared>,
    Body(request): Body<ModelPut>,
) -> Result<Json<ModelUpdate>, ApiError> {
    let mut database = database.write();
    Ok(Json(
        database.update_model(request.based_on, request.model)?,
    ))
}

/// One dimension, with each element's level, parents, children and attributes.
#[utoipa::path(
    get,
    path = "/dimensions/{name}",
    params(("name" = String, Path, description = "Dimension name, ignoring case and spaces")),
    responses(
        (status = OK, body = DimensionView, description = "The dimension"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown dimension")
    )
)]
async fn get_dimension(
    State(database): State<Shared>,
    Path(name): Path<String>,
) -> Result<Json<DimensionView>, ApiError> {
    let database = database.read().unwrap();
    let unknown = || {
        let message = format!("unknown dimension '{name}'");
        ApiError::new(StatusCode::NOT_FOUND, "unknown_dimension", message)
    };
    let dimension = database.model().dimension(&name).ok_or_else(unknown)?;
    let mut view = DimensionView::from(dimension);
    // A subset that reads cells selects what the stored cells say now.
    for subset in dimension
        .subsets
        .iter()
        .filter(|subset| subset.reads_cells())
    {
        let elements = database.subset_elements(&name, &subset.name)?;
        let names = elements
            .iter()
            .map(|&id| dimension.elements[id as usize].name.clone());
        view.subsets.insert(subset.name.clone(), names.collect());
    }
    Ok(Json(view))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct DimensionPut {
    /// The model version the dimension was edited from.
    based_on: u64,
    dimension: DimensionDef,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CubePut {
    /// The model version the cube was edited from.
    based_on: u64,
    cube: CubeDef,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct Based {
    /// The model version the removal was decided from.
    based_on: u64,
}

fn unknown(what: &'static str, name: &str) -> ApiError {
    let code = match what {
        "cube" => "unknown_cube",
        "user" => "unknown_user",
        _ => "unknown_dimension",
    };
    ApiError::new(
        StatusCode::NOT_FOUND,
        code,
        format!("unknown {what} '{name}'"),
    )
}

/// Changes one part of the model definition and puts the whole in place in one step, so that
/// a change to a part is as atomic and as versioned as a change of everything.
fn change(
    database: &Shared,
    based_on: u64,
    edit: impl FnOnce(&Database, &mut ModelDef) -> Result<(), ApiError>,
) -> Result<Json<ModelUpdate>, ApiError> {
    let mut database = database.write();
    let mut definition = database.definition().clone();
    edit(&database, &mut definition)?;
    Ok(Json(database.update_model(based_on, definition)?))
}

/// Where a dimension is in the definition, by its name ignoring case and spaces.
fn dimension_at(database: &Database, definition: &ModelDef, name: &str) -> Option<usize> {
    let found = &database.model().dimension(name)?.name;
    definition.dimensions.iter().position(|d| &d.name == found)
}

/// Where a cube is in the definition, by its name ignoring case and spaces.
fn cube_at(database: &Database, definition: &ModelDef, name: &str) -> Option<usize> {
    let found = &database.model().cubes()[database.model().cube_id(name)?].name;
    definition.cubes.iter().position(|c| &c.name == found)
}

/// Replaces one dimension, or adds it if the model has none of that name. Stored cells that
/// still fit are kept.
#[utoipa::path(
    put,
    path = "/dimensions/{name}",
    params(("name" = String, Path, description = "The dimension to replace, ignoring case and spaces")),
    request_body = DimensionPut,
    responses(
        (status = OK, body = ModelUpdate, description = "The dimension was replaced or added"),
        (status = CONFLICT, body = ApiError, description = "Stale version, or a transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The model would be invalid, or the request body is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn put_dimension(
    State(database): State<Shared>,
    Path(name): Path<String>,
    Body(request): Body<DimensionPut>,
) -> Result<Json<ModelUpdate>, ApiError> {
    change(&database, request.based_on, |database, definition| {
        match dimension_at(database, definition, &name) {
            Some(at) => definition.dimensions[at] = request.dimension,
            None => definition.dimensions.push(request.dimension),
        }
        Ok(())
    })
}

/// Removes one dimension. It is refused while a cube uses the dimension.
#[utoipa::path(
    delete,
    path = "/dimensions/{name}",
    params(("name" = String, Path, description = "Dimension name, ignoring case and spaces"), Based),
    responses(
        (status = OK, body = ModelUpdate, description = "The dimension was removed"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown dimension"),
        (status = CONFLICT, body = ApiError, description = "Stale version, or a transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "A cube uses the dimension"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn delete_dimension(
    State(database): State<Shared>,
    Path(name): Path<String>,
    Query(based): Query<Based>,
) -> Result<Json<ModelUpdate>, ApiError> {
    change(&database, based.based_on, |database, definition| {
        let at =
            dimension_at(database, definition, &name).ok_or_else(|| unknown("dimension", &name))?;
        definition.dimensions.remove(at);
        Ok(())
    })
}

/// One cube as the model defines it: its dimensions, views, rules and feeders.
#[utoipa::path(
    get,
    path = "/cubes/{name}",
    params(("name" = String, Path, description = "Cube name, ignoring case and spaces")),
    responses(
        (status = OK, body = CubeDef, description = "The cube"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown cube")
    )
)]
async fn get_cube(
    State(database): State<Shared>,
    Path(name): Path<String>,
) -> Result<Json<CubeDef>, ApiError> {
    let database = database.read().unwrap();
    let definition = database.definition();
    let at = cube_at(&database, definition, &name).ok_or_else(|| unknown("cube", &name))?;
    Ok(Json(definition.cubes[at].clone()))
}

/// Replaces one cube, or adds it if the model has none of that name. This is how a view, a
/// rule or a feeder is saved without sending the whole model. Stored cells that still fit are
/// kept.
#[utoipa::path(
    put,
    path = "/cubes/{name}",
    params(("name" = String, Path, description = "The cube to replace, ignoring case and spaces")),
    request_body = CubePut,
    responses(
        (status = OK, body = ModelUpdate, description = "The cube was replaced or added"),
        (status = CONFLICT, body = ApiError, description = "Stale version, or a transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The model would be invalid, or the request body is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn put_cube(
    State(database): State<Shared>,
    Path(name): Path<String>,
    Body(request): Body<CubePut>,
) -> Result<Json<ModelUpdate>, ApiError> {
    change(&database, request.based_on, |database, definition| {
        match cube_at(database, definition, &name) {
            Some(at) => definition.cubes[at] = request.cube,
            None => definition.cubes.push(request.cube),
        }
        Ok(())
    })
}

/// Removes one cube and the cells stored in it.
#[utoipa::path(
    delete,
    path = "/cubes/{name}",
    params(("name" = String, Path, description = "Cube name, ignoring case and spaces"), Based),
    responses(
        (status = OK, body = ModelUpdate, description = "The cube was removed"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown cube"),
        (status = CONFLICT, body = ApiError, description = "Stale version, or a transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "A rule or feeder of another cube reads the cube"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn delete_cube(
    State(database): State<Shared>,
    Path(name): Path<String>,
    Query(based): Query<Based>,
) -> Result<Json<ModelUpdate>, ApiError> {
    change(&database, based.based_on, |database, definition| {
        let at = cube_at(database, definition, &name).ok_or_else(|| unknown("cube", &name))?;
        definition.cubes.remove(at);
        Ok(())
    })
}

/// The grants the session layer put with the request.
type Held = Extension<Arc<Grants>>;

/// The name of the user a request is from, as the session layer found it.
#[derive(Clone)]
struct Signed(String);

/// Marks a request made in a view as another user.
#[derive(Clone)]
struct Viewing;

/// Who a server without users takes a request to be from.
const OPEN_USER: &str = "this machine";

/// Refuses unless the user has at least a role on a cube.
fn allow(grants: &Grants, cube: &str, needed: Role) -> Result<(), ApiError> {
    match grants.on(cube) >= needed {
        true => Ok(()),
        false => {
            let message = format!("this needs the role '{needed}' on cube '{cube}'");
            Err(ApiError::new(StatusCode::FORBIDDEN, "forbidden", message))
        }
    }
}

/// Reads a batch of cells. A cell that cannot be read reports its own error.
#[utoipa::path(
    post,
    path = "/cells/read",
    request_body = ReadRequest,
    responses(
        (status = OK, body = ReadResponse, description = "A value or an error for each cell"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The request body is malformed")
    )
)]
async fn read_cells(
    State(database): State<Shared>,
    Extension(grants): Held,
    Body(request): Body<ReadRequest>,
) -> Json<ReadResponse> {
    let database = database.read().unwrap();
    let result = |(cell, read): (usize, Result<Scalar, Error>)| {
        let allowed = allow(&grants, request.cells.cube(cell), Role::Read);
        match allowed.and_then(|()| Ok(read?)) {
            Ok(value) => CellResult::Value(value),
            Err(error) => CellResult::Error(error),
        }
    };
    let cells = database.read_batch(request.transaction, &request.cells);
    Json(ReadResponse {
        cells: cells.into_iter().enumerate().map(result).collect(),
    })
}

/// Says how each cell comes by its value: stored, calculated by a rule or added up, whether
/// it can be written, and whether it is fed. A cell that cannot be explained reports its own
/// error.
#[utoipa::path(
    post,
    path = "/cells/explain",
    request_body = ExplainRequest,
    responses(
        (status = OK, body = ExplainResponse, description = "An explanation or an error for each cell"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The request body is malformed")
    )
)]
async fn explain_cells(
    State(database): State<Shared>,
    Extension(grants): Held,
    Body(request): Body<ExplainRequest>,
) -> Json<ExplainResponse> {
    let database = database.read().unwrap();
    let explain = |cell: &CellRef| {
        let explained = allow(&grants, &cell.cube, Role::Read)
            .and_then(|()| Ok(database.explain(request.transaction, cell, request.feeders)?));
        match explained {
            Ok(cell) => ExplainResult::Cell(cell),
            Err(error) => ExplainResult::Error(error),
        }
    };
    Json(ExplainResponse {
        cells: request.cells.iter().map(explain).collect(),
    })
}

/// The changes committed to a cell, the newest first: when, by whom, and from what to what.
/// The history is read after the database is let go, so that a long one holds up nobody.
#[utoipa::path(
    post,
    path = "/cells/history",
    request_body = HistoryRequest,
    responses(
        (status = OK, body = HistoryResponse, description = "The cell's changes, the newest first"),
        (status = FORBIDDEN, body = ApiError, description = "The cube may not be read"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "No such cell, or the request body is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The history could not be read")
    )
)]
async fn cell_history(
    State(database): State<Shared>,
    Extension(grants): Held,
    Body(request): Body<HistoryRequest>,
) -> Result<Json<HistoryResponse>, ApiError> {
    allow(&grants, &request.cube, Role::Read)?;
    let asked = Asked {
        cube: Some(request.cube),
        at: Some(request.at),
        ..Asked::default()
    };
    let history = database.read().unwrap().history(asked)?;
    Ok(Json(HistoryResponse {
        changes: read_history(history, request.most).await?,
    }))
}

/// Reads the history of cells as asked, after the database is let go and on a thread of its
/// own: at most `most` changes, the newest, 100 unless given, and never more than 1,000.
async fn read_history(
    history: HistoryRead,
    most: Option<usize>,
) -> Result<Vec<CellChange>, ApiError> {
    let most = most.unwrap_or(100).clamp(1, 1000);
    let read = tokio::task::spawn_blocking(move || history.read(most))
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                error.to_string(),
            )
        })?;
    Ok(read.map_err(Error::from)?)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct ChangesQuery {
    /// Only changes to this cube.
    cube: Option<String>,
    /// Only changes made by this user, in any case.
    user: Option<String>,
    /// Only changes made from this time on, in seconds since 1970.
    since: Option<u64>,
    /// Only changes made until this time, in seconds since 1970.
    until: Option<u64>,
    /// At most this many, the newest: 100 unless given, and never more than 1,000.
    most: Option<usize>,
}

/// The changes saved to the cells of every cube, or of one, the newest first: who made each,
/// when, to which cell, from what to what. Narrowed by cube, by user, and by time.
#[utoipa::path(
    get,
    path = "/admin/changes",
    params(ChangesQuery),
    responses(
        (status = OK, body = HistoryResponse, description = "The changes, the newest first"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "Unknown cube, or the query is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The history could not be read")
    )
)]
async fn list_changes(
    State(database): State<Shared>,
    Query(query): Query<ChangesQuery>,
) -> Result<Json<HistoryResponse>, ApiError> {
    let asked = Asked {
        cube: query.cube,
        author: query.user,
        since: query.since,
        until: query.until,
        ..Asked::default()
    };
    let history = database.read().unwrap().history(asked)?;
    Ok(Json(HistoryResponse {
        changes: read_history(history, query.most).await?,
    }))
}

/// Writes a batch of leaf cells: all of them, or none if any is refused.
#[utoipa::path(
    post,
    path = "/cells/write",
    request_body = WriteRequest,
    responses(
        (status = NO_CONTENT, description = "Done"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction"),
        (status = CONFLICT, body = ApiError, description = "Another transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "A write is invalid, or the request body is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn write_cells(
    State(database): State<Shared>,
    Extension(grants): Held,
    Extension(Signed(user)): Extension<Signed>,
    Body(request): Body<WriteRequest>,
) -> Result<StatusCode, ApiError> {
    for index in 0..request.cells.len() {
        allow(&grants, request.cells.cube(index), Role::Write).map_err(|refusal| ApiError {
            index: Some(index),
            ..refusal
        })?;
    }
    let mut database = database.write_by(&user);
    database.write_batch(request.transaction, &request.cells)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct DetailRequest {
    /// Hold the detail and its value in this transaction, or this sandbox, until it commits.
    /// Without it they are committed at once.
    transaction: Option<u64>,
    cube: String,
    /// One element name or alias for each dimension of the cube.
    at: Vec<String>,
    /// The lines the cell's value is the sum of. None take the detail away and leave the
    /// value as it is.
    lines: Vec<Line>,
}

/// Writes a leaf cell's supporting detail, the lines its value is the sum of, and the sum to
/// the cell, as one write. A value written to the cell plainly later takes the detail away,
/// for its lines no longer add up to it; `POST /cells/explain` gives a cell's detail.
#[utoipa::path(
    post,
    path = "/cells/detail",
    request_body = DetailRequest,
    responses(
        (status = NO_CONTENT, description = "Done"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction"),
        (status = CONFLICT, body = ApiError, description = "Another transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The cell cannot be written, the lines are not a detail it can have, or the request body is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn write_detail(
    State(database): State<Shared>,
    Extension(grants): Held,
    Extension(Signed(user)): Extension<Signed>,
    Body(request): Body<DetailRequest>,
) -> Result<StatusCode, ApiError> {
    allow(&grants, &request.cube, Role::Write)?;
    let cell = CellRef {
        cube: request.cube,
        at: request.at,
    };
    let mut database = database.write_by(&user);
    database.write_detail(request.transaction, &cell, request.lines)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Writes a value to a consolidated cell by sharing it out among its leaves, as one batch:
/// every leaf is written or none.
#[utoipa::path(
    post,
    path = "/cells/spread",
    request_body = SpreadRequest,
    responses(
        (status = OK, body = SpreadResult, description = "The value was spread"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction"),
        (status = CONFLICT, body = ApiError, description = "Another transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The spread is invalid, or the request body is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn spread_cells(
    State(database): State<Shared>,
    Extension(grants): Held,
    Extension(Signed(user)): Extension<Signed>,
    Body(request): Body<SpreadRequest>,
) -> Result<Json<SpreadResult>, ApiError> {
    allow(&grants, &request.spread.cube, Role::Write)?;
    let mut database = database.write_by(&user);
    let written = database.spread(request.transaction, &request.spread)?;
    Ok(Json(SpreadResult { written }))
}

/// Answers an MDX query.
#[utoipa::path(
    post,
    path = "/mdx",
    request_body = QueryRequest,
    responses(
        (status = OK, body = QueryResult, description = "The answer"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The query is invalid, or the request body is malformed")
    )
)]
async fn query(
    State(state): State<AppState>,
    Extension(grants): Held,
    Extension(Signed(user)): Extension<Signed>,
    viewing: Option<Extension<Viewing>>,
    Body(request): Body<QueryRequest>,
) -> Result<Json<QueryResult>, ApiError> {
    let database = state.database.read().unwrap();
    let may_read = |cube: &str| grants.on(cube) >= Role::Read;
    let standing: Vec<(&str, &str)> = request
        .context
        .iter()
        .map(|at| (at.dimension.as_str(), at.member.as_str()))
        .collect();
    Ok(Json(database.query_as(
        request.transaction,
        &request.mdx,
        &may_read,
        &standing,
        &own_subsets(&state, &user, viewing.is_some()),
    )?))
}

/// The subsets the one asking keeps for themself, which `TM1SubsetToSet` with "private"
/// finds: none on a server without users, or in a view as another user, which sees none of
/// theirs.
fn own_subsets<'a>(
    state: &'a AppState,
    user: &'a str,
    viewing: bool,
) -> impl Fn(&str, &str) -> Option<Vec<String>> + 'a {
    move |dimension, subset| {
        let auth = state.auth.as_deref().filter(|_| !viewing)?;
        auth.subsets.one(user, dimension, subset)
    }
}

/// How many cells a page of stored cells holds unless the request says.
const PAGE: usize = 10_000;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct Page {
    /// How many stored cells to pass over first. Zero unless given.
    offset: Option<usize>,
    /// The most cells to return. Ten thousand unless given.
    limit: Option<usize>,
}

/// Some of the stored cells of a cube.
#[derive(Serialize, ToSchema)]
struct StoredCells {
    /// How many cells the cube stores in all.
    total: usize,
    /// The cells of this page, in a form `POST /cells/write` takes back.
    cells: Vec<CellWrite>,
}

/// Lists the cells a cube stores: its leaf cells that hold a value, with neither totals nor
/// cells a rule calculates. They come in a fixed order, so the whole can be taken a page at
/// a time, and what comes out can be written back with `POST /cells/write`, to this server or
/// another.
#[utoipa::path(
    get,
    path = "/cubes/{cube}/cells",
    params(("cube" = String, Path, description = "Cube name, ignoring case and spaces"), Page),
    responses(
        (status = OK, body = StoredCells, description = "The cells of the page"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "Unknown cube")
    )
)]
async fn stored_cells(
    State(database): State<Shared>,
    Extension(grants): Held,
    Path(cube): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<StoredCells>, ApiError> {
    allow(&grants, &cube, Role::Read)?;
    let database = database.read().unwrap();
    let (offset, limit) = (page.offset.unwrap_or(0), page.limit.unwrap_or(PAGE));
    let (total, cells) = database.stored(&cube, offset, limit)?;
    Ok(Json(StoredCells { total, cells }))
}

/// Runs a view saved with a cube.
#[utoipa::path(
    get,
    path = "/cubes/{cube}/views/{view}",
    params(("cube" = String, Path), ("view" = String, Path)),
    responses(
        (status = OK, body = QueryResult, description = "The answer"),
        (status = NOT_FOUND, body = ApiError, description = "The cube has no such view"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "Unknown cube")
    )
)]
async fn run_view(
    State(database): State<Shared>,
    Extension(grants): Held,
    Path((cube, view)): Path<(String, String)>,
) -> Result<Json<QueryResult>, ApiError> {
    // A view is part of the model, so like a rule it may read what its cube's readers may not.
    allow(&grants, &cube, Role::Read)?;
    Ok(Json(database.read().unwrap().view(None, &cube, &view)?))
}

/// Looks for leaf cells that a rule gives a value but no feeder feeds, which a fed cube's
/// consolidations therefore leave out. It works every calculated leaf out, so it is slow on
/// a large cube. At most a hundred cells are returned; none means the feeders are complete
/// for the data held, or the cube is not fed.
#[utoipa::path(
    get,
    path = "/cubes/{cube}/feeders/check",
    params(("cube" = String, Path)),
    responses(
        (status = OK, body = FeederCheck, description = "The cells found"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "Unknown cube, or a rule failed")
    )
)]
async fn check_feeders(
    State(database): State<Shared>,
    Extension(grants): Held,
    Path(cube): Path<String>,
) -> Result<Json<FeederCheck>, ApiError> {
    allow(&grants, &cube, Role::Read)?;
    let unfed = database.read().unwrap().unfed(&cube, 100)?;
    Ok(Json(FeederCheck { unfed }))
}

/// Begins a transaction, signed with the user who begins it: the history of cells records
/// its commit as theirs. It is rolled back if left idle.
#[utoipa::path(post, path = "/transactions", responses((status = CREATED, body = TransactionView, description = "The new transaction")))]
async fn begin_transaction(
    State(database): State<Shared>,
    Extension(Signed(user)): Extension<Signed>,
) -> (StatusCode, Json<TransactionView>) {
    let transaction = database.write_by(&user).begin();
    (StatusCode::CREATED, Json(TransactionView { transaction }))
}

/// Stores a transaction's pending writes in one step and ends it. A sandbox's are published,
/// checked and locked as any commit's, and the sandbox stays open, empty. Values go only
/// into cubes the user may write to now, however long ago they were written.
#[utoipa::path(
    post,
    path = "/transactions/{id}/commit",
    params(("id" = u64, Path)),
    responses(
        (status = NO_CONTENT, description = "Done"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction"),
        (status = CONFLICT, body = ApiError, description = "Publishing a sandbox: another transaction holds a lock, or has one of its cells pending"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "Publishing a sandbox: one of its values can no longer be written where it is"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn commit_transaction(
    State(database): State<Shared>,
    Extension(grants): Held,
    Path(id): Path<TransactionId>,
) -> Result<StatusCode, ApiError> {
    let mut database = database.write();
    for cube in database.pending_cubes(id)? {
        allow(&grants, &cube, Role::Write)?;
    }
    database.commit(id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Discards a transaction's pending writes and ends it. A sandbox is emptied, and stays.
#[utoipa::path(
    post,
    path = "/transactions/{id}/rollback",
    params(("id" = u64, Path)),
    responses(
        (status = NO_CONTENT, description = "Done"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction")
    )
)]
async fn rollback_transaction(
    State(database): State<Shared>,
    Path(id): Path<TransactionId>,
) -> Result<StatusCode, ApiError> {
    database.write().rollback(id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// How far a transaction has come.
#[derive(Serialize, ToSchema)]
struct TransactionPoint {
    /// How many writes the transaction has been given. Its writes since this point can be
    /// taken back with `POST /transactions/{id}/undo`.
    point: usize,
}

/// Says how far a transaction has come: a point to take it back to later.
#[utoipa::path(
    get,
    path = "/transactions/{id}/point",
    params(("id" = u64, Path)),
    responses(
        (status = OK, body = TransactionPoint, description = "The point the transaction has come to"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction")
    )
)]
async fn transaction_point(
    State(database): State<Shared>,
    Path(id): Path<TransactionId>,
) -> Result<Json<TransactionPoint>, ApiError> {
    let point = database.read().unwrap().savepoint(id)?;
    Ok(Json(TransactionPoint { point }))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UndoRequest {
    /// The point to go back to, as `GET /transactions/{id}/point` gave it.
    to: usize,
}

/// Takes back a transaction's writes since a point it came to, the last first, so that each
/// cell is pending as it was then. Spreads and batches go back whole. The transaction stays
/// open.
#[utoipa::path(
    post,
    path = "/transactions/{id}/undo",
    params(("id" = u64, Path)),
    request_body = UndoRequest,
    responses(
        (status = NO_CONTENT, description = "Done"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction, or a point it has not come to"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The request body is malformed")
    )
)]
async fn undo_transaction(
    State(database): State<Shared>,
    Path(id): Path<TransactionId>,
    Body(request): Body<UndoRequest>,
) -> Result<StatusCode, ApiError> {
    database.write().undo_to(id, request.to)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Locks a cube or the whole server for a transaction until it ends.
#[utoipa::path(
    post,
    path = "/transactions/{id}/locks",
    params(("id" = u64, Path)),
    request_body = LockRequest,
    responses(
        (status = NO_CONTENT, description = "Done"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction"),
        (status = CONFLICT, body = ApiError, description = "Another transaction holds a conflicting lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "Unknown cube, or the request body is malformed")
    )
)]
async fn lock(
    State(database): State<Shared>,
    Extension(grants): Held,
    Path(id): Path<TransactionId>,
    Body(request): Body<LockRequest>,
) -> Result<StatusCode, ApiError> {
    match &request.cube {
        Some(cube) => allow(&grants, cube, Role::Write)?,
        // Locking the server holds up every cube, so it takes the user's own role.
        None if grants.role() >= Role::Write => {}
        None => {
            let message = "locking the server needs a user with the role 'write'".to_owned();
            return Err(ApiError::new(StatusCode::FORBIDDEN, "forbidden", message));
        }
    }
    let mut database = database.write();
    database.lock(id, request.cube.as_deref(), request.mode)?;
    Ok(StatusCode::NO_CONTENT)
}

/* ---- Sandboxes: transactions kept under a name, for values to be tried out in ---- */

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct SandboxRequest {
    /// A name none of the user's other sandboxes has, case and spaces aside.
    name: String,
}

/// The signed-in user's sandboxes, in the order of their names. Values are read, written,
/// spread and queried in one with its transaction; `POST /transactions/{id}/commit`
/// publishes them, and `POST /transactions/{id}/rollback` empties it.
#[utoipa::path(get, path = "/sandboxes", responses((status = OK, body = Vec<Sandbox>, description = "The user's sandboxes")))]
async fn list_sandboxes(
    State(database): State<Shared>,
    Extension(Signed(user)): Extension<Signed>,
    viewing: Option<Extension<Viewing>>,
) -> Json<Vec<Sandbox>> {
    // A user's sandboxes are their drafts: a view as them does not see into them.
    if viewing.is_some() {
        return Json(Vec::new());
    }
    Json(database.read().unwrap().sandboxes(&user))
}

/// Makes a sandbox for the signed-in user: a transaction kept under a name, which holds no
/// locks, never expires and outlives the server, for values to wait in until they are
/// published.
#[utoipa::path(
    post,
    path = "/sandboxes",
    request_body = SandboxRequest,
    responses(
        (status = CREATED, body = Sandbox, description = "The new sandbox, empty"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The user has a sandbox of that name, or a sandbox cannot be called that"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The sandbox could not be kept")
    )
)]
async fn create_sandbox(
    State(database): State<Shared>,
    Extension(Signed(user)): Extension<Signed>,
    Body(request): Body<SandboxRequest>,
) -> Result<(StatusCode, Json<Sandbox>), ApiError> {
    let sandbox = database.write().create_sandbox(&user, &request.name)?;
    Ok((StatusCode::CREATED, Json(sandbox)))
}

/// Deletes one of the signed-in user's sandboxes, with the values it holds.
#[utoipa::path(
    delete,
    path = "/sandboxes/{name}",
    params(("name" = String, Path)),
    responses(
        (status = NO_CONTENT, description = "Done"),
        (status = NOT_FOUND, body = ApiError, description = "The user has no sandbox of that name")
    )
)]
async fn delete_sandbox(
    State(database): State<Shared>,
    Extension(Signed(user)): Extension<Signed>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    database.write().delete_sandbox(&user, &name)?;
    Ok(StatusCode::NO_CONTENT)
}

/* ---- What each user keeps for themself: views and subsets beside the model's ---- */

impl From<Refused> for ApiError {
    fn from(refused: Refused) -> Self {
        let (status, code) = match refused {
            Refused::Invalid(code, _) => (StatusCode::UNPROCESSABLE_ENTITY, code),
            Refused::Missing(code, _) => (StatusCode::NOT_FOUND, code),
            Refused::Unkept(_) => (StatusCode::INTERNAL_SERVER_ERROR, "storage_error"),
        };
        ApiError::new(status, code, refused.to_string())
    }
}

/// The users, who keep views and subsets for themselves; a server without users has none
/// of anybody's own, its views and subsets being everybody's.
fn keepers(state: &AppState) -> Result<&Auth, ApiError> {
    let message =
        "this server has no users, so its views and subsets are everybody's, kept with the model";
    let auth = state.auth.as_deref();
    auth.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no_users", message.to_owned()))
}

/// Refuses in a view as another user, which may keep and take away nothing of theirs.
fn not_viewing(viewing: Option<Extension<Viewing>>, user: &str) -> Result<(), ApiError> {
    match viewing {
        Some(_) => {
            let message =
                format!("a view as {user} only reads; what they keep is theirs to change");
            Err(ApiError::new(StatusCode::FORBIDDEN, "forbidden", message))
        }
        None => Ok(()),
    }
}

/// The signed-in user's own views of a cube, by name: queries saved for them alone, beside
/// the cube's views, which everybody sees. A view as the user sees none of them.
#[utoipa::path(
    get,
    path = "/cubes/{cube}/private-views",
    params(("cube" = String, Path, description = "Cube name, ignoring case and spaces")),
    responses(
        (status = OK, body = BTreeMap<String, ViewDef>, description = "The user's views of the cube"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown cube, or a server without users, which has no views of anybody's own")
    )
)]
async fn list_private_views(
    State(state): State<AppState>,
    Extension(grants): Held,
    Extension(Signed(user)): Extension<Signed>,
    viewing: Option<Extension<Viewing>>,
    Path(cube): Path<String>,
) -> Result<Json<BTreeMap<String, ViewDef>>, ApiError> {
    let views = &keepers(&state)?.views;
    allow(&grants, &cube, Role::Read)?;
    let named = state
        .database
        .read()
        .unwrap()
        .model()
        .cube(&cube)
        .map(|found| found.name.clone());
    let named = named.ok_or_else(|| unknown("cube", &cube))?;
    match viewing {
        Some(_) => Ok(Json(BTreeMap::new())),
        None => Ok(Json(views.of(&user, &named))),
    }
}

/// Saves a view of a cube for the signed-in user alone, in place of one of theirs of the
/// same name, case and spaces aside. Its query is run first, as theirs: it must select from
/// the cube, and they must be able to read what it reads. Each user's views may take 4 MiB.
#[utoipa::path(
    put,
    path = "/cubes/{cube}/private-views/{name}",
    params(("cube" = String, Path), ("name" = String, Path, description = "The view's name, up to 120 characters")),
    request_body = ViewDef,
    responses(
        (status = NO_CONTENT, description = "Saved"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown cube, or a server without users"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The query fails or selects from another cube, the name will not do, or the user's views would take too much room"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The view could not be kept")
    )
)]
async fn put_private_view(
    State(state): State<AppState>,
    Extension(grants): Held,
    Extension(Signed(user)): Extension<Signed>,
    viewing: Option<Extension<Viewing>>,
    Path((cube, name)): Path<(String, String)>,
    Body(view): Body<ViewDef>,
) -> Result<StatusCode, ApiError> {
    let views = &keepers(&state)?.views;
    not_viewing(viewing, &user)?;
    allow(&grants, &cube, Role::Read)?;
    let named = {
        let database = state.database.read().unwrap();
        let named = database.model().cube(&cube).map(|found| found.name.clone());
        let named = named.ok_or_else(|| unknown("cube", &cube))?;
        let may_read = |other: &str| grants.on(other) >= Role::Read;
        let own = own_subsets(&state, &user, false);
        let answered = database.query_as(None, &view.mdx, &may_read, &[], &own)?;
        if key(&answered.cube) != key(&named) {
            let message = format!(
                "the view's query selects from {}, not from {named}",
                answered.cube
            );
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_view",
                message,
            ));
        }
        named
    };
    views.keep(&user, &named, &name, view)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Takes one of the signed-in user's own views away; of a cube that is gone, too.
#[utoipa::path(
    delete,
    path = "/cubes/{cube}/private-views/{name}",
    params(("cube" = String, Path), ("name" = String, Path)),
    responses(
        (status = NO_CONTENT, description = "Taken away"),
        (status = NOT_FOUND, body = ApiError, description = "The user has no such view, or the server has no users"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The views could not be kept")
    )
)]
async fn delete_private_view(
    State(state): State<AppState>,
    Extension(Signed(user)): Extension<Signed>,
    viewing: Option<Extension<Viewing>>,
    Path((cube, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let views = &keepers(&state)?.views;
    not_viewing(viewing, &user)?;
    views.forget(&user, &cube, &name)?;
    Ok(StatusCode::NO_CONTENT)
}

/// A subset a user keeps for themself: the members it holds, in order.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct OwnSubset {
    /// The members, by name or alias; each is kept by its name.
    elements: Vec<String>,
}

/// The signed-in user's own subsets of a dimension, by name: lists of members kept for them
/// alone, beside the dimension's subsets, which everybody sees. A member since taken out of
/// the dimension is left out. A view as the user sees none of them.
#[utoipa::path(
    get,
    path = "/dimensions/{dimension}/private-subsets",
    params(("dimension" = String, Path, description = "Dimension name, ignoring case and spaces")),
    responses(
        (status = OK, body = BTreeMap<String, Vec<String>>, description = "The user's subsets of the dimension, each its members' names in order"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown dimension, or a server without users, which has no subsets of anybody's own")
    )
)]
async fn list_private_subsets(
    State(state): State<AppState>,
    Extension(Signed(user)): Extension<Signed>,
    viewing: Option<Extension<Viewing>>,
    Path(dimension): Path<String>,
) -> Result<Json<BTreeMap<String, Vec<String>>>, ApiError> {
    let subsets = &keepers(&state)?.subsets;
    let database = state.database.read().unwrap();
    let found = database.model().dimension(&dimension);
    let found = found.ok_or_else(|| unknown("dimension", &dimension))?;
    if viewing.is_some() {
        return Ok(Json(BTreeMap::new()));
    }
    let present = |members: Vec<String>| -> Vec<String> {
        let found = members.iter().filter_map(|member| found.element(member));
        found.map(|element| element.name.clone()).collect()
    };
    let theirs = subsets.of(&user, &found.name).into_iter();
    Ok(Json(
        theirs
            .map(|(name, members)| (name, present(members)))
            .collect(),
    ))
}

/// Saves a subset of a dimension for the signed-in user alone, in place of one of theirs of
/// the same name, case and spaces aside. Each member must be the dimension's, by name or
/// alias. Each user's subsets may take 4 MiB.
#[utoipa::path(
    put,
    path = "/dimensions/{dimension}/private-subsets/{name}",
    params(("dimension" = String, Path), ("name" = String, Path, description = "The subset's name, up to 120 characters")),
    request_body = OwnSubset,
    responses(
        (status = NO_CONTENT, description = "Saved"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown dimension, or a server without users"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "A member is not the dimension's, there are none, the name will not do, or the user's subsets would take too much room"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The subset could not be kept")
    )
)]
async fn put_private_subset(
    State(state): State<AppState>,
    Extension(Signed(user)): Extension<Signed>,
    viewing: Option<Extension<Viewing>>,
    Path((dimension, name)): Path<(String, String)>,
    Body(subset): Body<OwnSubset>,
) -> Result<StatusCode, ApiError> {
    let subsets = &keepers(&state)?.subsets;
    not_viewing(viewing, &user)?;
    let invalid = |message: String| {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_subset", message)
    };
    let (named, members) = {
        let database = state.database.read().unwrap();
        let found = database.model().dimension(&dimension);
        let found = found.ok_or_else(|| unknown("dimension", &dimension))?;
        let members = subset.elements.iter().map(|member| {
            let element = found.element(member).map(|element| element.name.clone());
            element.ok_or_else(|| invalid(format!("{} has no element '{member}'", found.name)))
        });
        (found.name.clone(), members.collect::<Result<Vec<_>, _>>()?)
    };
    if members.is_empty() {
        return Err(invalid("a subset needs a member".to_owned()));
    }
    subsets.keep(&user, &named, &name, members)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Takes one of the signed-in user's own subsets away; of a dimension that is gone, too.
#[utoipa::path(
    delete,
    path = "/dimensions/{dimension}/private-subsets/{name}",
    params(("dimension" = String, Path), ("name" = String, Path)),
    responses(
        (status = NO_CONTENT, description = "Taken away"),
        (status = NOT_FOUND, body = ApiError, description = "The user has no such subset, or the server has no users"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The subsets could not be kept")
    )
)]
async fn delete_private_subset(
    State(state): State<AppState>,
    Extension(Signed(user)): Extension<Signed>,
    viewing: Option<Extension<Viewing>>,
    Path((dimension, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let subsets = &keepers(&state)?.subsets;
    not_viewing(viewing, &user)?;
    subsets.forget(&user, &dimension, &name)?;
    Ok(StatusCode::NO_CONTENT)
}

/* ---- Dashboards: pages of saved views, kept with the model ---- */

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct DashboardPut {
    /// The model version the dashboard was edited from.
    based_on: u64,
    dashboard: DashboardDef,
}

/// Where a dashboard is in the definition, by its name ignoring case and spaces.
fn dashboard_at(definition: &ModelDef, name: &str) -> Option<usize> {
    definition
        .dashboards
        .iter()
        .position(|dashboard| key(&dashboard.name) == key(name))
}

/// Replaces one dashboard, or adds it if the model has none of that name. Every tile must
/// show a view the cube has. Dashboards are kept with the model, under its version.
#[utoipa::path(
    put,
    path = "/dashboards/{name}",
    params(("name" = String, Path, description = "The dashboard to replace, ignoring case and spaces")),
    request_body = DashboardPut,
    responses(
        (status = OK, body = ModelUpdate, description = "The dashboard was replaced or added"),
        (status = CONFLICT, body = ApiError, description = "Stale version"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "A tile shows no view the cube has, or the request body is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn put_dashboard(
    State(database): State<Shared>,
    Path(name): Path<String>,
    Body(request): Body<DashboardPut>,
) -> Result<Json<ModelUpdate>, ApiError> {
    change(&database, request.based_on, |_, definition| {
        match dashboard_at(definition, &name) {
            Some(at) => definition.dashboards[at] = request.dashboard,
            None => definition.dashboards.push(request.dashboard),
        }
        Ok(())
    })
}

/// Removes one dashboard.
#[utoipa::path(
    delete,
    path = "/dashboards/{name}",
    params(("name" = String, Path, description = "Dashboard name, ignoring case and spaces"), Based),
    responses(
        (status = OK, body = ModelUpdate, description = "The dashboard was removed"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown dashboard"),
        (status = CONFLICT, body = ApiError, description = "Stale version"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn delete_dashboard(
    State(database): State<Shared>,
    Path(name): Path<String>,
    Query(based): Query<Based>,
) -> Result<Json<ModelUpdate>, ApiError> {
    change(&database, based.based_on, |_, definition| {
        let at = dashboard_at(definition, &name).ok_or_else(|| unknown("dashboard", &name))?;
        definition.dashboards.remove(at);
        Ok(())
    })
}

/* ---- Administration: the server's state, its users, sessions and transactions ---- */

/// The state of the server.
#[derive(Serialize, ToSchema)]
struct ServerStatus {
    /// The version of the server program.
    server: String,
    model_version: u64,
    /// How long the server has been running, in seconds.
    uptime_seconds: u64,
    /// The cells each cube stores.
    cubes: Vec<CubeStatus>,
    /// How many transactions are open.
    transactions: usize,
    /// How many sessions are open; none on a server that asks no one to sign in.
    sessions: Option<usize>,
    /// The bytes of recorded commits since the last snapshot.
    log_bytes: Option<u64>,
    /// The bytes of the last snapshot.
    snapshot_bytes: Option<u64>,
    /// The memory the server holds, where the operating system says.
    memory_bytes: Option<u64>,
}

#[derive(Serialize, ToSchema)]
struct CubeStatus {
    name: String,
    /// How many leaf cells it stores.
    cells: usize,
}

/// The users of the server.
#[derive(Serialize, ToSchema)]
struct UserList {
    /// Whether the server asks users to sign in. Without users it does not, and none can be
    /// added while it runs.
    signing_in: bool,
    users: Vec<UserView>,
}

/// A user to add or change.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UserPut {
    /// A new password. A new user needs one; without it a user's password is kept.
    password: Option<String>,
    role: Role,
    /// Cubes on which the user has another role in place of that one.
    #[serde(default)]
    cubes: BTreeMap<String, Role>,
}

#[derive(Serialize, ToSchema)]
struct Ended {
    /// How many sessions were ended.
    ended: usize,
}

/// An open transaction.
#[derive(Serialize, ToSchema)]
struct TransactionInfo {
    transaction: u64,
    /// How long since it was last used, in seconds.
    idle_seconds: u64,
    /// How many writes it holds, waiting to be committed.
    pending: usize,
}

/// The memory the process holds, where the operating system says: on Linux, its resident
/// set.
fn memory() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kilobytes: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kilobytes * 1024)
}

/// The users, or a refusal on a server that has none.
fn users_of(state: &AppState) -> Result<&Arc<Auth>, ApiError> {
    state.auth.as_ref().ok_or_else(|| {
        let message = "this server asks no one to sign in; add its first user with \
                       'epiphany-server user', then restart it";
        ApiError::new(StatusCode::CONFLICT, "no_users", message.to_owned())
    })
}

/// The state of the server: the cells each cube stores, what it keeps on disk, its open
/// sessions and transactions, and how long it has run. For administrators.
#[utoipa::path(
    get,
    path = "/admin/status",
    responses((status = OK, body = ServerStatus, description = "The state of the server"))
)]
async fn server_status(State(state): State<AppState>) -> Json<ServerStatus> {
    let database = state.database.read().unwrap();
    let journal = database.journal_bytes();
    let cubes = database.cell_counts().into_iter();
    Json(ServerStatus {
        server: env!("CARGO_PKG_VERSION").to_owned(),
        model_version: database.model_version(),
        uptime_seconds: state.started.elapsed().as_secs(),
        cubes: cubes
            .map(|(name, cells)| CubeStatus { name, cells })
            .collect(),
        transactions: database.open_transactions().len(),
        sessions: state.auth.as_ref().map(|auth| auth.open_sessions()),
        log_bytes: journal.map(|(log, _)| log),
        snapshot_bytes: journal.map(|(_, snapshot)| snapshot),
        memory_bytes: memory(),
    })
}

/// The users, with their roles and how many sessions each has open. For administrators.
#[utoipa::path(
    get,
    path = "/admin/users",
    responses((status = OK, body = UserList, description = "The users"))
)]
async fn list_users(State(state): State<AppState>) -> Json<UserList> {
    Json(UserList {
        signing_in: state.auth.is_some(),
        users: state
            .auth
            .as_ref()
            .map(|auth| auth.list())
            .unwrap_or_default(),
    })
}

/// Adds a user, or changes one's role, cube roles or password. It takes effect at once:
/// the user's open sessions hold the new roles from their next request. For administrators.
#[utoipa::path(
    put,
    path = "/admin/users/{name}",
    params(("name" = String, Path, description = "The user, ignoring case")),
    request_body = UserPut,
    responses(
        (status = NO_CONTENT, description = "The user was saved"),
        (status = CONFLICT, body = ApiError, description = "The server has no users, so none can be added while it runs"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "A new user has no password, the change would leave no administrator, or the request body is malformed")
    )
)]
async fn put_user(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Body(user): Body<UserPut>,
) -> Result<StatusCode, ApiError> {
    let grants = Grants::new(user.role, user.cubes);
    let saved = users_of(&state)?.set(&name, user.password.as_deref(), grants);
    saved.map_err(|message| {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_user", message)
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a user and ends their sessions. The last administrator cannot be removed. For
/// administrators.
#[utoipa::path(
    delete,
    path = "/admin/users/{name}",
    params(("name" = String, Path, description = "The user, ignoring case")),
    responses(
        (status = NO_CONTENT, description = "The user was removed"),
        (status = NOT_FOUND, body = ApiError, description = "There is no such user"),
        (status = CONFLICT, body = ApiError, description = "The server has no users"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "It would leave no administrator")
    )
)]
async fn delete_user(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let auth = users_of(&state)?;
    if !auth.has(&name) {
        return Err(unknown("user", &name));
    }
    let removed = auth.remove(&name);
    removed.map_err(|message| {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_user", message)
    })?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ViewAsRequest {
    /// The user to view the server as, ignoring case.
    user: String,
}

/// Starts a view of the server as another user has it: a session of theirs that may only
/// read what they may read, and none of their sandboxes, naming the administrator viewing.
/// It is recorded in the audit first. Sign out of it as of any session. For administrators.
#[utoipa::path(
    post,
    path = "/admin/view-as",
    request_body = ViewAsRequest,
    responses(
        (status = CREATED, body = Session, description = "The view's session"),
        (status = NOT_FOUND, body = ApiError, description = "There is no such user"),
        (status = CONFLICT, body = ApiError, description = "The server has no users"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The view could not be recorded, so it is not given")
    )
)]
async fn view_as(
    State(state): State<AppState>,
    Extension(Signed(viewer)): Extension<Signed>,
    Body(request): Body<ViewAsRequest>,
) -> Result<(StatusCode, Json<Session>), ApiError> {
    let auth = users_of(&state)?;
    if !auth.has(&request.user) {
        return Err(unknown("user", &request.user));
    }
    let (token, grants, _) = auth.view_as(&viewer, &request.user).map_err(|message| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "storage_failed", message)
    })?;
    let session = Session {
        token,
        idle_seconds: auth.idle().as_secs(),
        role: grants.role(),
        cubes: grants.cubes().clone(),
        viewer: Some(viewer),
    };
    Ok((StatusCode::CREATED, Json(session)))
}

#[derive(Deserialize, IntoParams)]
struct AuditQuery {
    /// At most this many entries, the newest; 100 if not given, and never more than 1,000.
    most: Option<usize>,
}

/// What the audit records, the newest first: each view of the server as another user, who
/// viewed and when. For administrators.
#[utoipa::path(
    get,
    path = "/admin/audit",
    params(AuditQuery),
    responses(
        (status = OK, body = Vec<AuditEntry>, description = "What the audit records"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The audit could not be read")
    )
)]
async fn read_audit(
    State(state): State<AppState>,
    Query(query): Query<AuditQuery>,
) -> Result<Json<Vec<AuditEntry>>, ApiError> {
    let Some(auth) = state.auth.as_ref() else {
        return Ok(Json(Vec::new()));
    };
    let most = query.most.unwrap_or(100).min(1000);
    let entries = auth.audit(most).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            error.to_string(),
        )
    })?;
    Ok(Json(entries))
}

/// Ends every session of a user, who must sign in again. For administrators.
#[utoipa::path(
    delete,
    path = "/admin/users/{name}/sessions",
    params(("name" = String, Path, description = "The user, ignoring case")),
    responses(
        (status = OK, body = Ended, description = "The sessions were ended"),
        (status = NOT_FOUND, body = ApiError, description = "There is no such user"),
        (status = CONFLICT, body = ApiError, description = "The server has no users")
    )
)]
async fn end_sessions(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Ended>, ApiError> {
    let auth = users_of(&state)?;
    if !auth.has(&name) {
        return Err(unknown("user", &name));
    }
    Ok(Json(Ended {
        ended: auth.end_sessions(&name),
    }))
}

/// The open transactions, the longest idle first. One is ended with
/// `POST /transactions/{id}/rollback`. For administrators.
#[utoipa::path(
    get,
    path = "/admin/transactions",
    responses((status = OK, body = Vec<TransactionInfo>, description = "The open transactions"))
)]
async fn list_transactions(State(state): State<AppState>) -> Json<Vec<TransactionInfo>> {
    let database = state.database.read().unwrap();
    let open = database.open_transactions().into_iter();
    Json(
        open.map(|transaction| TransactionInfo {
            transaction: transaction.id,
            idle_seconds: transaction.idle.as_secs(),
            pending: transaction.pending,
        })
        .collect(),
    )
}

/* ---- Comments: notes pinned to cells ---- */

#[derive(Serialize, ToSchema)]
struct Comments {
    /// The oldest first.
    comments: Vec<Comment>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CommentPost {
    /// The cell: one element name or alias for each dimension of the cube.
    at: Vec<String>,
    text: String,
}

/// The comments on a cube's cells.
#[utoipa::path(
    get,
    path = "/cubes/{cube}/comments",
    params(("cube" = String, Path)),
    responses(
        (status = OK, body = Comments, description = "The comments, the oldest first"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown cube")
    )
)]
async fn list_comments(
    State(database): State<Shared>,
    Extension(grants): Held,
    Path(cube): Path<String>,
) -> Result<Json<Comments>, ApiError> {
    allow(&grants, &cube, Role::Read)?;
    let comments = database.read().unwrap().comments(&cube)?;
    Ok(Json(Comments { comments }))
}

/// Pins a comment to a cell, signed by its author. Takes a user who may write to the cube.
#[utoipa::path(
    post,
    path = "/cubes/{cube}/comments",
    params(("cube" = String, Path)),
    request_body = CommentPost,
    responses(
        (status = CREATED, body = Comment, description = "The comment as kept"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown cube"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "Unknown cell, a comment empty or too long, or the request body is malformed"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The comment could not be stored")
    )
)]
async fn add_comment(
    State(database): State<Shared>,
    Extension(grants): Held,
    Extension(Signed(author)): Extension<Signed>,
    Path(cube): Path<String>,
    Body(post): Body<CommentPost>,
) -> Result<(StatusCode, Json<Comment>), ApiError> {
    allow(&grants, &cube, Role::Write)?;
    let written = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let comment = database
        .write()
        .comment(&cube, &post.at, &author, written, &post.text)?;
    Ok((StatusCode::CREATED, Json(comment)))
}

/// Takes a comment away: its author may, and an administrator.
#[utoipa::path(
    delete,
    path = "/cubes/{cube}/comments/{id}",
    params(("cube" = String, Path), ("id" = u64, Path)),
    responses(
        (status = NO_CONTENT, description = "Taken away"),
        (status = FORBIDDEN, body = ApiError, description = "Someone else's comment, and the user is no administrator"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown cube or comment"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn remove_comment(
    State(database): State<Shared>,
    Extension(grants): Held,
    Extension(Signed(user)): Extension<Signed>,
    Path((cube, id)): Path<(String, u64)>,
) -> Result<StatusCode, ApiError> {
    allow(&grants, &cube, Role::Write)?;
    let mut database = database.write();
    let comment = database
        .comments(&cube)?
        .into_iter()
        .find(|comment| comment.id == id)
        .ok_or_else(|| Error::NotFound(format!("cube '{cube}' has no comment {id}")))?;
    if !comment.author.eq_ignore_ascii_case(&user) && grants.role() < Role::Admin {
        let message = format!("the comment is {}'s to take away", comment.author);
        return Err(ApiError::new(StatusCode::FORBIDDEN, "forbidden", message));
    }
    database.uncomment(id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The models kept from before the current one, newest first. One is restored by sending it
/// back with `PUT /model`, which makes it the newest version. For administrators.
#[utoipa::path(
    get,
    path = "/admin/models",
    responses((status = OK, body = Vec<EarlierModel>, description = "The earlier models kept"))
)]
async fn list_earlier_models(State(database): State<Shared>) -> Json<Vec<EarlierModel>> {
    Json(database.read().unwrap().earlier_models())
}

/// A model kept from before the current one, with the version it was. For administrators.
#[utoipa::path(
    get,
    path = "/admin/models/{name}",
    params(("name" = String, Path, description = "The name it is kept under, as the list of earlier models gives it")),
    responses(
        (status = OK, body = ModelView, description = "The earlier model"),
        (status = NOT_FOUND, body = ApiError, description = "No earlier model is kept under that name"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The kept model could not be read")
    )
)]
async fn get_earlier_model(
    State(database): State<Shared>,
    Path(name): Path<String>,
) -> Result<Json<ModelView>, ApiError> {
    let database = database.read().unwrap();
    let model = database.earlier_model(&name)?;
    let kept = database
        .earlier_models()
        .into_iter()
        .find(|earlier| earlier.name == name);
    Ok(Json(ModelView {
        version: kept.map_or(0, |earlier| earlier.version),
        model,
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn a_request_that_goes_wrong_is_answered_and_so_is_the_next() {
        let model = serde_json::json!({ "dimensions": [], "cubes": [] });
        let database = Database::new(serde_json::from_value(model).unwrap()).unwrap();
        let state = AppState {
            version: Arc::new(AtomicU64::new(database.model_version())),
            database: Guarded::new(database),
            auth: None,
            started: Instant::now(),
        };
        let fail = get(async || -> &'static str { panic!("as nobody foresaw") });
        let application = Router::new()
            .route("/fail", fail)
            .route("/fine", get(async || "fine"))
            .layer(from_fn_with_state(state, survive));
        let ask = |path: &'static str| {
            let request = Request::builder().uri(path).body(Body::empty()).unwrap();
            application.clone().oneshot(request)
        };
        let failed = ask("/fail").await.unwrap();
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(failed.into_body(), 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "internal");
        assert_eq!(ask("/fine").await.unwrap().status(), StatusCode::OK);
    }
}
