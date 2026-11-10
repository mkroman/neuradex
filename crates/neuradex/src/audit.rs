//! Audit event wiring for the HTTP service.
//!
//! The service records exactly one [`neuradex_audit::AuditEvent`] per request through its
//! request middleware. The event schema, header policy, and sinks belong to `neuradex-audit`;
//! this module only connects them to axum, the request lifecycle, and the handler extensions
//! that enrich a request record.

mod guard;
mod request_layer;

use std::sync::Arc;

use neuradex_audit::{AuditSink, HeaderPolicy, NullSink};

use crate::config::DEFAULT_CONTENT_MAX_BODY_BYTES;

pub(crate) use guard::RequestAuditHandle;
pub(crate) use request_layer::request_audit;

/// The audit configuration carried by [`AppState`](crate::state::AppState).
///
/// The default configuration discards events and captures headers with the audit crate's
/// default policy. Tests and the binary replace it with a real sink through
/// [`AppState::try_new_with_audit`](crate::state::AppState::try_new_with_audit).
#[derive(Clone)]
pub struct AuditConfig {
    sink: Arc<dyn AuditSink>,
    header_policy: HeaderPolicy,
    max_body_bytes: usize,
}

impl AuditConfig {
    /// Builds an audit configuration from its sink, header policy, and captured-body limit.
    #[must_use]
    pub fn new(
        sink: Arc<dyn AuditSink>,
        header_policy: HeaderPolicy,
        max_body_bytes: usize,
    ) -> Self {
        Self {
            sink,
            header_policy,
            max_body_bytes,
        }
    }

    /// Returns the sink every finished request event is emitted to.
    pub(crate) fn sink(&self) -> &Arc<dyn AuditSink> {
        &self.sink
    }

    /// Returns the policy used to capture request and response headers.
    pub(crate) const fn header_policy(&self) -> &HeaderPolicy {
        &self.header_policy
    }

    /// Returns the maximum number of request body bytes the service captures.
    pub(crate) const fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            sink: Arc::new(NullSink),
            header_policy: HeaderPolicy::default(),
            max_body_bytes: DEFAULT_CONTENT_MAX_BODY_BYTES,
        }
    }
}
