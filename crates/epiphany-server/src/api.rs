//! The routes, their request and response shapes, and the OpenAPI document generated from them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use epiphany_core::Value;
use epiphany_core::database::{
    CellRef, CellWrite, Database, Error, LockMode, ModelUpdate, TransactionId,
};
use epiphany_core::mdx::QueryResult;
use epiphany_core::model::{AttributeType, ChildDef, Dimension, ElementId, ElementType, ModelDef};
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

type Shared = Arc<RwLock<Database>>;

#[derive(OpenApi)]
#[openapi(info(title = "Epiphany", description = "An in-memory OLAP server."))]
struct Contract;

fn router() -> OpenApiRouter<Shared> {
    OpenApiRouter::with_openapi(Contract::openapi())
        .routes(routes!(get_model, put_model))
        .routes(routes!(get_dimension))
        .routes(routes!(read_cells))
        .routes(routes!(write_cells))
        .routes(routes!(query))
        .routes(routes!(begin_transaction))
        .routes(routes!(commit_transaction))
        .routes(routes!(rollback_transaction))
        .routes(routes!(lock))
}

/// The OpenAPI document describing every route.
pub fn openapi() -> utoipa::openapi::OpenApi {
    router().into_openapi()
}

/// The HTTP application serving a database.
pub fn app(database: Database) -> Router {
    let (router, contract) = router().split_for_parts();
    router
        .route("/openapi.json", get(async move || Json(contract)))
        .with_state(Arc::new(RwLock::new(database)))
}

/// The application plus a test page at `/lab`, read from disk on every request so that it can
/// be edited while the server runs.
pub fn lab(database: Database, page: PathBuf) -> Router {
    let serve = move || {
        let page = std::fs::read_to_string(&page);
        let missing = |error: std::io::Error| (StatusCode::NOT_FOUND, error.to_string());
        async move { page.map(Html).map_err(missing) }
    };
    app(database).route("/lab", get(serve))
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
            Error::Cell(_) | Error::Write { .. } | Error::Model(_) | Error::Mdx(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Error::UnknownTransaction(_) => StatusCode::NOT_FOUND,
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
    attributes: BTreeMap<String, Value>,
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
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    /// Read within this transaction, seeing its pending writes.
    transaction: Option<TransactionId>,
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
    Value(Value),
    Error(ApiError),
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct WriteRequest {
    /// Hold the writes in this transaction until it commits. Without it they are committed
    /// at once.
    transaction: Option<TransactionId>,
    cells: Vec<CellWrite>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct QueryRequest {
    /// Query within this transaction, seeing its pending writes.
    transaction: Option<TransactionId>,
    /// The query text, for example `SELECT {[Entity].[US]} ON COLUMNS FROM [General Ledger]`.
    mdx: String,
}

#[derive(Serialize, ToSchema)]
struct TransactionView {
    transaction: TransactionId,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct LockRequest {
    /// The cube to lock. Without it the whole server is locked.
    cube: Option<String>,
    mode: LockMode,
}

/// The model definition and its version.
#[utoipa::path(get, path = "/model", responses((status = OK, body = ModelView)))]
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
        (status = OK, body = ModelUpdate),
        (status = CONFLICT, body = ApiError, description = "Stale version, or a transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The definition is invalid")
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
    responses((status = OK, body = DimensionView), (status = NOT_FOUND, body = ApiError))
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

/// Reads a batch of cells. A cell that cannot be read reports its own error.
#[utoipa::path(
    post,
    path = "/cells/read",
    request_body = ReadRequest,
    responses((status = OK, body = ReadResponse))
)]
async fn read_cells(
    State(database): State<Shared>,
    Body(request): Body<ReadRequest>,
) -> Json<ReadResponse> {
    let database = database.read().unwrap();
    let read = |cell: &CellRef| match database.read(request.transaction, cell) {
        Ok(value) => CellResult::Value(value),
        Err(error) => CellResult::Error(error.into()),
    };
    Json(ReadResponse {
        cells: request.cells.iter().map(read).collect(),
    })
}

/// Writes a batch of leaf cells: all of them, or none if any is refused.
#[utoipa::path(
    post,
    path = "/cells/write",
    request_body = WriteRequest,
    responses(
        (status = NO_CONTENT),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction"),
        (status = CONFLICT, body = ApiError, description = "Another transaction holds a lock"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "A write is invalid")
    )
)]
async fn write_cells(
    State(database): State<Shared>,
    Body(request): Body<WriteRequest>,
) -> Result<StatusCode, ApiError> {
    let mut database = database.write().unwrap();
    database.write(request.transaction, &request.cells)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Answers an MDX query.
#[utoipa::path(
    post,
    path = "/mdx",
    request_body = QueryRequest,
    responses(
        (status = OK, body = QueryResult),
        (status = NOT_FOUND, body = ApiError, description = "Unknown transaction"),
        (status = UNPROCESSABLE_ENTITY, body = ApiError, description = "The query is invalid")
    )
)]
async fn query(
    State(database): State<Shared>,
    Body(request): Body<QueryRequest>,
) -> Result<Json<QueryResult>, ApiError> {
    let database = database.read().unwrap();
    Ok(Json(database.query(request.transaction, &request.mdx)?))
}

/// Begins a transaction. It is rolled back if left idle.
#[utoipa::path(post, path = "/transactions", responses((status = CREATED, body = TransactionView)))]
async fn begin_transaction(State(database): State<Shared>) -> (StatusCode, Json<TransactionView>) {
    let transaction = database.write().unwrap().begin();
    (StatusCode::CREATED, Json(TransactionView { transaction }))
}

/// Stores a transaction's pending writes in one step and ends it.
#[utoipa::path(
    post,
    path = "/transactions/{id}/commit",
    params(("id" = TransactionId, Path)),
    responses((status = NO_CONTENT), (status = NOT_FOUND, body = ApiError))
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
    params(("id" = TransactionId, Path)),
    responses((status = NO_CONTENT), (status = NOT_FOUND, body = ApiError))
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
    params(("id" = TransactionId, Path)),
    request_body = LockRequest,
    responses(
        (status = NO_CONTENT),
        (status = NOT_FOUND, body = ApiError),
        (status = CONFLICT, body = ApiError, description = "Another transaction holds a conflicting lock")
    )
)]
async fn lock(
    State(database): State<Shared>,
    Path(id): Path<TransactionId>,
    Body(request): Body<LockRequest>,
) -> Result<StatusCode, ApiError> {
    let mut database = database.write().unwrap();
    database.lock(id, request.cube.as_deref(), request.mode)?;
    Ok(StatusCode::NO_CONTENT)
}
