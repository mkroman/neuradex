//! The service-side completion guard that emits exactly one audit event per request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use neuradex_audit::{
    AuditError, AuditSink, CompletionGuard, HeaderCapture, Outcome, RequestRecord,
};

/// A clonable handle that handlers use to enrich the request's audit record.
///
/// The middleware inserts one of these into the request extensions. Handlers only ever hold the
/// lock for the duration of a single update, so the shared record can be finished by the guard
/// after the handler returns without losing enrichment.
#[derive(Clone)]
pub struct RequestAuditHandle {
    record: Arc<Mutex<RequestRecord>>,
}

impl RequestAuditHandle {
    /// Applies `update` to the shared request record.
    pub(crate) fn update(&self, update: impl FnOnce(&mut RequestRecord)) {
        match self.record.lock() {
            Ok(mut record) => update(&mut record),
            // A panic while enriching a record must not prevent the guard from emitting it.
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }
}

/// Emits the shared request record exactly once, whichever way the request ends.
///
/// The normal path calls [`complete`](Self::complete) after the response is produced. When the
/// middleware future is dropped — a cancelled request — the guard emits an aborted event; when
/// it is dropped during unwinding, it emits a panic event. The completion flag means a handler
/// that stored its [`RequestAuditHandle`] past the response cannot suppress or duplicate the
/// event.
pub struct RequestAudit {
    record: Arc<Mutex<RequestRecord>>,
    sink: Arc<dyn AuditSink>,
    emitted: AtomicBool,
}

impl RequestAudit {
    /// Wraps `record` and `sink` in a guard.
    pub(crate) fn new(record: RequestRecord, sink: Arc<dyn AuditSink>) -> Self {
        Self {
            record: Arc::new(Mutex::new(record)),
            sink,
            emitted: AtomicBool::new(false),
        }
    }

    /// Returns a handle for handler enrichment.
    pub(crate) fn handle(&self) -> RequestAuditHandle {
        RequestAuditHandle {
            record: Arc::clone(&self.record),
        }
    }

    /// Completes the request with its response status, duration, and captured headers.
    ///
    /// # Errors
    ///
    /// Returns the sink's error when the event cannot be emitted, or
    /// [`AuditError::AlreadyCompleted`] when the guard already emitted.
    pub(crate) fn complete(
        &self,
        status: StatusCode,
        duration: Duration,
        response_headers: HeaderCapture,
    ) -> Result<(), AuditError> {
        if self.emitted.swap(true, Ordering::AcqRel) {
            return Err(AuditError::AlreadyCompleted);
        }

        let event = {
            let mut record = self.lock();
            record.set_response(status.as_u16(), duration);
            record.set_response_headers(response_headers);
            if status.is_client_error() || status.is_server_error() {
                record.set_error_type(status.as_u16().to_string());
            }
            record.clone().finish()
        };

        self.sink.emit(&event)
    }

    /// Emits an aborted or panic event, or reports that the guard already emitted.
    fn emit_forced(&self, outcome: Outcome) -> Result<(), AuditError> {
        if self.emitted.swap(true, Ordering::AcqRel) {
            return Err(AuditError::AlreadyCompleted);
        }

        let record = self.lock().clone();
        let guard = CompletionGuard::new(record, Arc::clone(&self.sink));

        match outcome {
            Outcome::Panic => guard.mark_panic(),
            _ => guard.abort(),
        }
    }

    /// Locks the shared record, recovering from a handler panic that poisoned it.
    fn lock(&self) -> std::sync::MutexGuard<'_, RequestRecord> {
        match self.record.lock() {
            Ok(record) => record,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Drop for RequestAudit {
    fn drop(&mut self) {
        if self.emitted.load(Ordering::Acquire) {
            return;
        }

        let outcome = if std::thread::panicking() {
            Outcome::Panic
        } else {
            Outcome::Aborted
        };

        // The guard may emit during unwinding; a sink error cannot be propagated from a `Drop`
        // implementation, and the event is not retried so it is attempted at most once.
        let _ = self.emit_forced(outcome);
    }
}
