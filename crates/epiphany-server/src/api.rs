//! The routes, their request and response shapes, and the OpenAPI document generated from them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::Extension;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRef, FromRequest, Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use epiphany_core::Scalar;
use epiphany_core::database::{
    CellRef, CellWrite, Database, Error, Explanation, LockMode, ModelUpdate, Spread, TransactionId,
};
use epiphany_core::mdx::QueryResult;
use epiphany_core::model::{
    AttributeType, ChildDef, CubeDef, Dimension, DimensionDef, ElementId, ElementType, ModelDef,
};
use serde::{Deserialize, Serialize};

use crate::auth::{Auth, Grants, Refusal, Role};
use utoipa::openapi::RefOr;
use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityRequirement, SecurityScheme};
use utoipa::{IntoParams, OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

type Shared = Arc<RwLock<Database>>;

/// What every request can reach: the database, and the users if the server has any.
#[derive(Clone)]
struct AppState {
    database: Shared,
    auth: Option<Arc<Auth>>,
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

fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(Contract::openapi())
        .routes(routes!(sign_in, sign_out))
        .routes(routes!(get_model, put_model))
        .routes(routes!(get_dimension, put_dimension, delete_dimension))
        .routes(routes!(get_cube, put_cube, delete_cube))
        .routes(routes!(read_cells))
        .routes(routes!(write_cells))
        .routes(routes!(explain_cells))
        .routes(routes!(spread_cells))
        .routes(routes!(query))
        .routes(routes!(stored_cells))
        .routes(routes!(run_view))
        .routes(routes!(check_feeders))
        .routes(routes!(begin_transaction))
        .routes(routes!(commit_transaction))
        .routes(routes!(rollback_transaction))
        .routes(routes!(lock))
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
    database.set_transaction_ids(|| {
        getrandom::u64().expect("the operating system provides randomness") >> 11
    });
    let state = AppState {
        database: Arc::new(RwLock::new(database)),
        auth: auth.map(Arc::new),
    };
    let (router, contract) = router().split_for_parts();
    let contract = secured(contract);
    router
        .layer(from_fn_with_state(state.clone(), require_session))
        .layer(from_fn_with_state(state.clone(), stamp))
        .route("/openapi.json", get(async move || Json(contract)))
        .with_state(state)
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
    let version = state.database.read().unwrap().model_version();
    response
        .headers_mut()
        .insert(MODEL_VERSION, HeaderValue::from(version));
    response
}

/// What a user must be allowed to do to call a route.
fn needed(method: &Method, path: &str) -> Role {
    let reads = ["/cells/read", "/cells/explain", "/mdx"];
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
        return next.run(request).await;
    };
    let grants = bearer(request.headers()).and_then(|token| auth.admits(token));
    let needed = needed(request.method(), request.uri().path());
    if let Some(grants) = grants {
        // The model belongs to no cube, so changing it takes the user's own role. Anything
        // else is let in by a role on any cube; the route then asks about the cube it names.
        let held = match needed {
            Role::Admin => grants.role(),
            _ => grants.most(),
        };
        if held < needed {
            let message = format!("this needs a user with the role '{needed}'");
            return ApiError::new(StatusCode::FORBIDDEN, "forbidden", message).into_response();
        }
        request.extensions_mut().insert(grants);
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
        (status = TOO_MANY_REQUESTS, body = ApiError, description = "The name has been given too many wrong passwords; the Retry-After header says how many seconds to wait"),
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
            | Error::Rule(_) => StatusCode::UNPROCESSABLE_ENTITY,
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
    attributes: BTreeMap<String, AttributeType>,
    /// Elements in declaration order.
    elements: Vec<ElementView>,
    /// The element names each subset selects, in the subset's order.
    subsets: BTreeMap<String, Vec<String>>,
}

#[derive(Serialize, ToSchema)]
struct ElementView {
    name: String,
    #[serde(rename = "type")]
    kind: ElementType,
    /// Zero for a leaf, otherwise one more than the highest child level.
    level: u32,
    parents: Vec<String>,
    children: Vec<ChildDef>,
    attributes: BTreeMap<String, Scalar>,
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
            }
        });
        Self {
            name: dimension.name.clone(),
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
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    /// Read within this transaction, seeing its pending writes.
    transaction: Option<u64>,
    cells: Vec<CellRef>,
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
    cells: Vec<CellWrite>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct QueryRequest {
    /// Query within this transaction, seeing its pending writes.
    transaction: Option<u64>,
    /// The query text, for example `SELECT {[Entity].[US]} ON COLUMNS FROM [General Ledger]`.
    mdx: String,
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
    let mut database = database.write().unwrap();
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
    Ok(Json(dimension.into()))
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
struct Based {
    /// The model version the removal was decided from.
    based_on: u64,
}

fn unknown(what: &'static str, name: &str) -> ApiError {
    let code = match what {
        "cube" => "unknown_cube",
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
    let mut database = database.write().unwrap();
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
    let result = |(cell, read): (&CellRef, Result<Scalar, Error>)| match allow(
        &grants,
        &cell.cube,
        Role::Read,
    )
    .and_then(|()| Ok(read?))
    {
        Ok(value) => CellResult::Value(value),
        Err(error) => CellResult::Error(error),
    };
    let cells = database.read_many(request.transaction, &request.cells);
    Json(ReadResponse {
        cells: request.cells.iter().zip(cells).map(result).collect(),
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
    Body(request): Body<WriteRequest>,
) -> Result<StatusCode, ApiError> {
    for (index, cell) in request.cells.iter().enumerate() {
        allow(&grants, &cell.cube, Role::Write).map_err(|refusal| ApiError {
            index: Some(index),
            ..refusal
        })?;
    }
    let mut database = database.write().unwrap();
    database.write(request.transaction, &request.cells)?;
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
    Body(request): Body<SpreadRequest>,
) -> Result<Json<SpreadResult>, ApiError> {
    allow(&grants, &request.spread.cube, Role::Write)?;
    let mut database = database.write().unwrap();
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
    State(database): State<Shared>,
    Extension(grants): Held,
    Body(request): Body<QueryRequest>,
) -> Result<Json<QueryResult>, ApiError> {
    let database = database.read().unwrap();
    let may_read = |cube: &str| grants.on(cube) >= Role::Read;
    Ok(Json(database.query_as(
        request.transaction,
        &request.mdx,
        &may_read,
    )?))
}

/// How many cells a page of stored cells holds unless the request says.
const PAGE: usize = 10_000;

#[derive(Deserialize, IntoParams)]
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

/// Begins a transaction. It is rolled back if left idle.
#[utoipa::path(post, path = "/transactions", responses((status = CREATED, body = TransactionView, description = "The new transaction")))]
async fn begin_transaction(State(database): State<Shared>) -> (StatusCode, Json<TransactionView>) {
    let transaction = database.write().unwrap().begin();
    (StatusCode::CREATED, Json(TransactionView { transaction }))
}

/// Stores a transaction's pending writes in one step and ends it.
#[utoipa::path(
    post,
    path = "/transactions/{id}/commit",
    params(("id" = u64, Path)),
    responses(
        (status = NO_CONTENT, description = "Done"),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction"),
        (status = INTERNAL_SERVER_ERROR, body = ApiError, description = "The change could not be stored")
    )
)]
async fn commit_transaction(
    State(database): State<Shared>,
    Path(id): Path<TransactionId>,
) -> Result<StatusCode, ApiError> {
    database.write().unwrap().commit(id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Discards a transaction's pending writes and ends it.
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
    database.write().unwrap().rollback(id)?;
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
    let mut database = database.write().unwrap();
    database.lock(id, request.cube.as_deref(), request.mode)?;
    Ok(StatusCode::NO_CONTENT)
}
