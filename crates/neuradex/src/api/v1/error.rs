//! The error type shared by the API handlers.
//!
//! Every error is rendered as a JSON body of the shape
//! `{"error": {"type": "...", "message": "..."}}` with an appropriate status code.

use std::collections::BTreeMap;
use std::error::Error;

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
#[non_exhaustive]
pub enum ApiError {
    /// A request parameter is missing, malformed, or out of range.
    #[error("{0}")]
    InvalidParam(String),
    /// The request body or target response has a content type that cannot be handled.
    #[error("{0}")]
    UnsupportedMediaType(String),
    /// The upstream request failed.
    #[error("the upstream request failed")]
    Upstream(#[source] Box<dyn Error + Send + Sync>),
    /// An internal, unexpected failure.
    #[error("an internal error occurred")]
    Internal(#[source] Box<dyn Error + Send + Sync>),
}

/// A message-only error for failures that carry no underlying source.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct MessageError(String);

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
pub(crate) enum ErrorKind {
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

impl ErrorKind {
    /// Returns the wire name of this kind.
    ///
    /// This must stay in step with the `#[serde(rename_all = "snake_case")]` output; the
    /// `serialized_kinds_match_as_str` test pins the two together.
    const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidParam => "invalid_param",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::UpstreamError => "upstream_error",
            Self::InternalError => "internal_error",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
        }
    }
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

    /// Constructs an [`ApiError::Upstream`] from the error that caused it.
    pub fn upstream(error: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self::Upstream(error.into())
    }

    /// Constructs an [`ApiError::Upstream`] from a message alone.
    pub fn upstream_message(message: impl Into<String>) -> Self {
        Self::upstream(MessageError(message.into()))
    }

    /// Constructs an [`ApiError::Internal`] from the error that caused it.
    pub fn internal(error: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self::Internal(error.into())
    }

    /// Constructs an [`ApiError::Internal`] from a message alone.
    pub fn internal_message(message: impl Into<String>) -> Self {
        Self::internal(MessageError(message.into()))
    }

    /// Returns the kind of this error.
    #[must_use]
    pub(crate) const fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidParam(_) => ErrorKind::InvalidParam,
            Self::UnsupportedMediaType(_) => ErrorKind::UnsupportedMediaType,
            Self::Upstream(_) => ErrorKind::UpstreamError,
            Self::Internal(_) => ErrorKind::InternalError,
        }
    }

    /// Returns the machine-readable error type used by the API envelope and as `error.type`
    /// on spans and log events.
    #[must_use]
    pub(crate) const fn error_type(&self) -> &'static str {
        self.kind().as_str()
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::internal(error)
    }
}

/// Logs a 5xx failure with its source chain.
///
/// The source is logged here and never rendered into the response body: 5xx bodies stay
/// generic per the API invariants. Callers must redact URLs before boxing an upstream error
/// (`wreq::Error::without_uri` at the fetch boundary); the `Debug` rendering is used so the
/// cause chain is visible to operators.
fn log_failure(kind: ErrorKind, source: &(dyn Error + Send + Sync)) {
    tracing::error!(
        "error.type" = kind.as_str(),
        "exception.message" = ?source,
        "request failed"
    );
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let kind = self.kind();

        let (status, message) = match self {
            Self::InvalidParam(message) => (StatusCode::BAD_REQUEST, message),
            Self::UnsupportedMediaType(message) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, message),
            Self::Upstream(source) => {
                log_failure(kind, source.as_ref());
                (
                    StatusCode::BAD_GATEWAY,
                    "the upstream request failed".to_owned(),
                )
            }
            Self::Internal(source) => {
                log_failure(kind, source.as_ref());
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "an internal error occurred".to_owned(),
                )
            }
        };

        let body = ErrorBody {
            error: ErrorDetail { kind, message },
        };

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::Value;
    use utoipa::IntoResponses;

    use super::*;

    #[tokio::test]
    async fn maps_every_constructor_to_its_status_and_type() {
        // The `IntoResponse` match is exhaustive, so a variant added without a mapping fails to
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
                ApiError::upstream(io::Error::other("timeout")),
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "the upstream request failed",
            ),
            (
                ApiError::internal(io::Error::other("panic")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "an internal error occurred",
            ),
        ];
        let documented = ApiError::responses();

        for (error, status, kind, expected) in cases {
            // The hand-written response map must document every status this match renders.
            assert!(
                documented.contains_key(status.as_str()),
                "the {status} response is missing from ApiError::responses()"
            );

            // Span and log telemetry must classify the failure the same way the envelope does.
            assert_eq!(error.error_type(), kind);

            let response = error.into_response();

            assert_eq!(response.status(), status);

            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            let rendered: Value = serde_json::from_slice(&body).expect("valid json");

            assert_eq!(rendered["error"]["type"], kind);
            assert_eq!(rendered["error"]["message"], expected);
        }
    }

    #[test]
    fn serialized_kinds_match_as_str() {
        let kinds = [
            ErrorKind::InvalidParam,
            ErrorKind::UnsupportedMediaType,
            ErrorKind::UpstreamError,
            ErrorKind::InternalError,
            ErrorKind::NotFound,
            ErrorKind::MethodNotAllowed,
        ];

        for kind in kinds {
            let value = serde_json::to_value(kind).expect("the kind serializes");

            assert_eq!(value, Value::from(kind.as_str()));
        }
    }

    #[test]
    fn exposes_the_underlying_error_as_the_source() {
        let upstream = ApiError::upstream(io::Error::other("upstream detail"));
        let internal = ApiError::internal(io::Error::other("internal detail"));

        assert_eq!(
            upstream.source().map(ToString::to_string).as_deref(),
            Some("upstream detail")
        );
        assert_eq!(
            internal.source().map(ToString::to_string).as_deref(),
            Some("internal detail")
        );
    }
}
