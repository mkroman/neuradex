//! The error type shared by the API handlers.
//!
//! Every error is rendered as a JSON body of the shape
//! `{"error": {"type": "...", "message": "..."}}` with an appropriate status code.

use std::collections::BTreeMap;

use axum::Json;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::openapi::{Content, RefOr, ResponseBuilder, ResponsesBuilder};
use utoipa::{IntoResponses, PartialSchema, ToSchema};

/// An error produced while serving an API request.
///
/// The errors the handlers can produce are documented as OpenAPI responses through the
/// [`IntoResponses`] implementation, referenced from every `#[utoipa::path]` as `ApiError`.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// A request parameter is missing, malformed, or out of range.
    #[error("{0}")]
    InvalidParam(String),
    /// The target response has a content type that cannot be returned.
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
#[derive(Debug, Serialize, ToSchema)]
#[schema(examples(json!({
    "error": {
        "type": "invalid_param",
        "message": "the timeout parameter must be between 1 and 30 seconds"
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
                kind: "not_found",
                message: format!("no route for {path}"),
            },
        }
    }

    /// Builds the body of a method-not-allowed response for `method` on `path`.
    pub(crate) fn method_not_allowed(method: &Method, path: &str) -> Self {
        Self {
            error: ErrorDetail {
                kind: "method_not_allowed",
                message: format!("method {method} is not allowed for {path}"),
            },
        }
    }
}

/// The details of an error response.
#[derive(Debug, Serialize, ToSchema)]
struct ErrorDetail {
    /// The machine-readable error type.
    #[serde(rename = "type")]
    #[schema(value_type = String)]
    kind: &'static str,
    /// The human-readable error message.
    message: String,
}

/// Builds the shared JSON error response with the given description.
fn error_response(description: &'static str) -> ResponseBuilder {
    ResponseBuilder::new().description(description).content(
        "application/json",
        Content::builder().schema(Some(ErrorBody::schema())).build(),
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
                error_response("The target response has a content type that cannot be returned."),
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
    const fn classify(&self) -> (StatusCode, &'static str) {
        match self {
            Self::InvalidParam(_) => (StatusCode::BAD_REQUEST, "invalid_param"),
            Self::UnsupportedMediaType(_) => {
                (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type")
            }
            Self::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream_error"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
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
        let message = if status.is_server_error() {
            tracing::error!(kind, message = %self, "request failed");
            generic_message(kind).to_owned()
        } else {
            self.to_string()
        };

        let body = ErrorBody {
            error: ErrorDetail { kind, message },
        };

        (status, Json(body)).into_response()
    }
}

/// Returns the generic, client-safe message rendered for a 5xx error class.
fn generic_message(kind: &str) -> &'static str {
    match kind {
        "upstream_error" => "the upstream request failed",
        _ => "an internal error occurred",
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::Value;
    use serde_json::json;

    use super::*;

    #[test]
    fn classifies_errors() {
        assert_eq!(
            ApiError::invalid_param("nope").classify(),
            (StatusCode::BAD_REQUEST, "invalid_param")
        );
        assert_eq!(
            ApiError::UnsupportedMediaType("binary".into()).classify(),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type")
        );
        assert_eq!(
            ApiError::upstream("timeout").classify(),
            (StatusCode::BAD_GATEWAY, "upstream_error")
        );
        assert_eq!(
            ApiError::internal("panic").classify(),
            (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        );
    }

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

    #[test]
    fn renders_as_json() {
        let response = ApiError::invalid_param("nope").into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn serializes_the_documented_shape() {
        let detail = ErrorDetail {
            kind: "invalid_param",
            message: "nope".to_string(),
        };

        assert_eq!(
            serde_json::to_value(ErrorBody { error: detail }).unwrap(),
            json!({"error": {"type": "invalid_param", "message": "nope"}})
        );
    }
}
