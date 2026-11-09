//! The error type shared by the API handlers.
//!
//! Every error is rendered as a JSON body of the shape
//! `{"error": {"type": "...", "message": "..."}}` with an appropriate status code.

use std::collections::BTreeMap;

use axum::Json;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::IntoResponses;
use utoipa::openapi::{Content, Ref, RefOr, ResponseBuilder, ResponsesBuilder};

/// An error produced while serving an API request.
///
/// The errors the handlers can produce are documented as OpenAPI responses through the
/// [`IntoResponses`] implementation, referenced from every `#[utoipa::path]` as `ApiError`.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// A request parameter is missing, malformed, or out of range.
    #[error("{0}")]
    InvalidParam(String),
    /// The request body or target response has a content type that cannot be handled.
    #[error("{0}")]
    UnsupportedMediaType(String),
    /// The upstream request failed.
    #[error("upstream request failed: {0}")]
    Upstream(String),
    /// An internal, unexpected failure.
    #[error("{0}")]
    Internal(String),
}

/// The JSON body of an error response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(examples(json!({
    "error": {
        "type": "invalid_param",
        "message": "the timeout must be between 1 and 30 seconds"
    }
})))]
pub(crate) struct ErrorBody {
    /// The error details.
    error: ErrorDetail,
}

impl ErrorBody {
    /// Builds the body of a not-found response for `path`.
    pub(crate) fn not_found(path: &str) -> Self {
        Self {
            error: ErrorDetail {
                kind: ErrorKind::NotFound,
                message: format!("no route for {path}"),
            },
        }
    }

    /// Builds the body of a method-not-allowed response for `method` on `path`.
    pub(crate) fn method_not_allowed(method: &Method, path: &str) -> Self {
        Self {
            error: ErrorDetail {
                kind: ErrorKind::MethodNotAllowed,
                message: format!("method {method} is not allowed for {path}"),
            },
        }
    }
}

/// The machine-readable error type.
///
/// The router-level `not_found` and `method_not_allowed` values are included because
/// the 404 and 405 fallbacks produce them too.
#[derive(Clone, Copy, Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ErrorKind {
    /// A request parameter is missing, malformed, or out of range.
    InvalidParam,
    /// The request body or target response has an unsupported content type.
    UnsupportedMediaType,
    /// The upstream request failed.
    UpstreamError,
    /// An internal, unexpected failure.
    InternalError,
    /// No route matched the request.
    NotFound,
    /// The route exists but does not accept the request method.
    MethodNotAllowed,
}

/// The details of an error response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ErrorDetail {
    /// The machine-readable error type.
    #[serde(rename = "type")]
    kind: ErrorKind,
    /// The human-readable error message.
    message: String,
}

/// Builds the shared JSON error response with the given description.
///
/// The content references the registered `ErrorBody` component instead of inlining it:
/// every error response uses the same schema, and the reference keeps the document (and
/// its generated clients) from repeating it.
fn error_response(description: &'static str) -> ResponseBuilder {
    ResponseBuilder::new().description(description).content(
        "application/json",
        Content::builder()
            .schema(Some(RefOr::Ref(Ref::from_schema_name("ErrorBody"))))
            .build(),
    )
}

impl IntoResponses for ApiError {
    fn responses() -> BTreeMap<String, RefOr<utoipa::openapi::Response>> {
        ResponsesBuilder::new()
            .response(
                "400",
                error_response("A request parameter is missing, malformed, or out of range."),
            )
            .response(
                "415",
                error_response(
                    "The request body is not JSON, or the target response has a content type \
                     that cannot be returned.",
                ),
            )
            .response("500", error_response("An internal, unexpected failure."))
            .response("502", error_response("The upstream request failed."))
            .build()
            .into()
    }
}

impl ApiError {
    /// Constructs an [`ApiError::InvalidParam`].
    pub fn invalid_param(message: impl Into<String>) -> Self {
        Self::InvalidParam(message.into())
    }

    /// Constructs an [`ApiError::UnsupportedMediaType`].
    pub fn unsupported_media_type(message: impl Into<String>) -> Self {
        Self::UnsupportedMediaType(message.into())
    }

    /// Constructs an [`ApiError::Upstream`].
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::Upstream(message.into())
    }

    /// Constructs an [`ApiError::Internal`].
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// Returns the status code and machine-readable type for the error.
    const fn classify(&self) -> (StatusCode, ErrorKind) {
        match self {
            Self::InvalidParam(_) => (StatusCode::BAD_REQUEST, ErrorKind::InvalidParam),
            Self::UnsupportedMediaType(_) => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorKind::UnsupportedMediaType,
            ),
            Self::Upstream(_) => (StatusCode::BAD_GATEWAY, ErrorKind::UpstreamError),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, ErrorKind::InternalError),
        }
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::internal(format!("could not process the response: {error}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, kind) = self.classify();

        // 5xx bodies stay generic per the "Invariants" rule: the details are for the logs,
        // never for the client.
        if status.is_server_error() {
            tracing::error!(kind = ?kind, message = %self, "request failed");
        }

        // 4xx variants already own their client-facing message; only the 5xx classes need a
        // generic replacement.
        let message = match self {
            Self::InvalidParam(message) | Self::UnsupportedMediaType(message) => message,
            Self::Upstream(_) => "the upstream request failed".to_owned(),
            Self::Internal(_) => "an internal error occurred".to_owned(),
        };

        let body = ErrorBody {
            error: ErrorDetail { kind, message },
        };

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::Value;

    use super::*;

    #[tokio::test]
    async fn maps_every_constructor_to_its_status_and_type() {
        // `classify` is an exhaustive match, so a variant added without a mapping fails to
        // compile; this additionally pins each constructor's pairing on the rendered response,
        // where the envelope type is serialized — including the generic 5xx bodies.
        let cases = [
            (
                ApiError::invalid_param("nope"),
                StatusCode::BAD_REQUEST,
                "invalid_param",
                "nope",
            ),
            (
                ApiError::unsupported_media_type("binary"),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "binary",
            ),
            (
                ApiError::upstream("timeout"),
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "the upstream request failed",
            ),
            (
                ApiError::internal("panic"),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "an internal error occurred",
            ),
        ];

        for (error, status, kind, expected) in cases {
            let detail = error.to_string();
            let response = error.into_response();

            assert_eq!(response.status(), status);

            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            let rendered: Value = serde_json::from_slice(&body).expect("valid json");

            assert_eq!(rendered["error"]["type"], kind);
            assert_eq!(rendered["error"]["message"], expected);

            if status.is_server_error() {
                // The detail is logged, never returned to the client.
                assert_ne!(rendered["error"]["message"], detail);
            }
        }
    }
}
