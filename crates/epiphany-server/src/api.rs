//! The routes, their request and response shapes, and the OpenAPI document generated from them.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use epiphany_core::Value;
use epiphany_core::cells::CellError;
use epiphany_core::database::{BatchError, CellRef, CellWrite, Database};
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
        .routes(routes!(get_model))
        .routes(routes!(get_dimension))
        .routes(routes!(read_cells))
        .routes(routes!(write_cells))
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

impl From<CellError> for ApiError {
    fn from(error: CellError) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: error.code(),
            message: error.to_string(),
            index: None,
        }
    }
}

impl From<BatchError> for ApiError {
    fn from(batch: BatchError) -> Self {
        Self {
            index: Some(batch.index),
            ..batch.error.into()
        }
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
            Err(rejection) => Err(ApiError {
                status: rejection.status(),
                code: "invalid_request",
                message: rejection.body_text(),
                index: None,
            }),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
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
    cells: Vec<CellWrite>,
}

/// The model definition.
#[utoipa::path(get, path = "/model", responses((status = OK, body = ModelDef)))]
async fn get_model(State(database): State<Shared>) -> Json<ModelDef> {
    Json(database.read().unwrap().definition().clone())
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
    let dimension = database.model().dimension(&name).ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        code: "unknown_dimension",
        message: format!("unknown dimension '{name}'"),
        index: None,
    })?;
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
    let read = |cell: &CellRef| match database.read(cell) {
        Ok(value) => CellResult::Value(value),
        Err(error) => CellResult::Error(error.into()),
    };
    Json(ReadResponse {
        cells: request.cells.iter().map(read).collect(),
    })
}

/// Writes a batch of leaf cells: all of them, or none if any is invalid.
#[utoipa::path(
    post,
    path = "/cells/write",
    request_body = WriteRequest,
    responses((status = NO_CONTENT), (status = UNPROCESSABLE_ENTITY, body = ApiError))
)]
async fn write_cells(
    State(database): State<Shared>,
    Body(request): Body<WriteRequest>,
) -> Result<StatusCode, ApiError> {
    database.write().unwrap().write(&request.cells)?;
    Ok(StatusCode::NO_CONTENT)
}
