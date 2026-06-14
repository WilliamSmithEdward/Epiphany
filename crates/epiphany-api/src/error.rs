//! The JSON error envelope shared by every endpoint.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use epiphany_core::QueryError;
use serde::Serialize;

/// An API error rendered as `{"error": {"code", "message", "details"?}}`.
///
/// `message` is safe to show a client; internal causes are logged, never
/// serialized (RG-12). `code` is a stable machine-readable token.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Option<serde_json::Value>,
}

impl ApiError {
    /// A fully specified error.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Attach structured details (for example, a failed batch index).
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// 404 Not Found.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", message)
    }

    /// 400 Bad Request (malformed input).
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BAD_REQUEST", message)
    }

    /// 422 Unprocessable Entity (well-formed but semantically rejected).
    pub fn unprocessable(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    }

    /// 409 Conflict.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "CONFLICT", message)
    }

    /// 401 Unauthorized.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", message)
    }

    /// 500 Internal Server Error (the cause is logged, not serialized).
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            "internal server error",
        )
    }

    /// The HTTP status this error renders as.
    pub fn status_code(&self) -> StatusCode {
        self.status
    }
}

/// Map a core query error to the HTTP envelope with a stable code. A missing
/// named object is 404; everything else is a 422 the client can act on.
impl From<QueryError> for ApiError {
    fn from(e: QueryError) -> Self {
        let message = e.to_string();
        match e {
            QueryError::UnknownSubset { .. } => {
                ApiError::new(StatusCode::NOT_FOUND, "UNKNOWN_SUBSET", message)
            }
            QueryError::UnknownDimension { .. } => {
                ApiError::unprocessable("UNKNOWN_DIMENSION", message)
            }
            QueryError::UnknownMember { .. } => ApiError::unprocessable("UNKNOWN_ELEMENT", message),
            QueryError::DimensionCoverage { .. } => {
                ApiError::unprocessable("DIMENSION_COVERAGE", message)
            }
            QueryError::SubsetDimensionMismatch { .. } => {
                ApiError::unprocessable("SUBSET_DIMENSION_MISMATCH", message)
            }
            QueryError::DynamicUnsupported | QueryError::DynamicEval { .. } => {
                ApiError::unprocessable("MDX_ERROR", message)
            }
            QueryError::Calc { .. } => ApiError::unprocessable("CALC_ERROR", message),
            QueryError::Model(_) => ApiError::unprocessable("MODEL_ERROR", message),
        }
    }
}

#[derive(Serialize)]
struct Envelope {
    error: Body,
}

#[derive(Serialize)]
struct Body {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let envelope = Envelope {
            error: Body {
                code: self.code,
                message: self.message,
                details: self.details,
            },
        };
        (self.status, Json(envelope)).into_response()
    }
}
