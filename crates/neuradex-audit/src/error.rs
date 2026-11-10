//! Typed errors for audit event construction and sinks.

use std::io;

/// An error produced while recording or emitting an audit event.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuditError {
    /// An event could not be serialized to JSON.
    #[error("failed to serialize an audit event: {0}")]
    Serialization(#[from] serde_json::Error),
    /// An event could not be written to a sink.
    #[error("failed to write an audit event: {0}")]
    Io(#[from] io::Error),
    /// A value supplied to an audit event constructor was invalid.
    #[error("invalid {field}: {reason}")]
    InvalidValue {
        /// The name of the invalid field.
        field: &'static str,
        /// A human-readable description of the problem.
        reason: String,
    },
    /// A shared sink or capture buffer was poisoned by a panic.
    #[error("an audit sink lock was poisoned")]
    Poisoned,
    /// A completion guard was completed more than once.
    #[error("the audit completion guard has already been completed")]
    AlreadyCompleted,
}

impl AuditError {
    /// Builds an [`AuditError::InvalidValue`] for `field`.
    #[must_use]
    pub(crate) fn invalid_value(field: &'static str, reason: impl std::fmt::Display) -> Self {
        Self::InvalidValue {
            field,
            reason: reason.to_string(),
        }
    }

    /// Returns the field named by an [`AuditError::InvalidValue`].
    #[must_use]
    pub const fn field(&self) -> Option<&'static str> {
        match self {
            Self::InvalidValue { field, .. } => Some(field),
            _ => None,
        }
    }
}
