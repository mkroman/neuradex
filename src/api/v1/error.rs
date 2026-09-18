//! The error type shared by the API handlers.
//!
//! Every error is rendered as a JSON body of the shape
//! `{"error": {"type": "...", "message": "..."}}` with an appropriate status code.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// An error produced while serving an API request.
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
#[derive(Debug, Serialize)]
struct ErrorBody {
    /// The error details.
    error: ErrorDetail,
}

/// The details of an error response.
#[derive(Debug, Serialize)]
struct ErrorDetail {
    /// The machine-readable error type.
    #[serde(rename = "type")]
    kind: &'static str,
    /// The human-readable error message.
    message: String,
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
        let body = ErrorBody {
            error: ErrorDetail {
                kind,
                message: self.to_string(),
            },
        };

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
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
