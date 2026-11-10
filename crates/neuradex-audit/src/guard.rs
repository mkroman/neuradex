//! A completion guard that emits exactly one event per request.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::AuditError;
use crate::event::Outcome;
use crate::request_record::RequestRecord;
use crate::sink::AuditSink;

/// Emits a [`RequestRecord`] exactly once, whichever way its scope ends.
///
/// Call [`complete`](Self::complete) on the success path, [`abort`](Self::abort) when the request
/// is cancelled, and [`mark_panic`](Self::mark_panic) when a panic is caught. If none of them is
/// called, dropping the guard emits an aborted event — or a panic event when the thread is
/// unwinding. The first transition wins; later calls and the drop are no-ops.
#[derive(Debug)]
pub struct CompletionGuard<S: AuditSink> {
    record: Option<RequestRecord>,
    sink: S,
    completed: AtomicBool,
}

impl<S> CompletionGuard<S>
where
    S: AuditSink,
{
    /// Wraps `record` and `sink` in a guard.
    #[must_use]
    pub const fn new(record: RequestRecord, sink: S) -> Self {
        Self {
            record: Some(record),
            sink,
            completed: AtomicBool::new(false),
        }
    }

    /// Emits the record with a successful outcome.
    ///
    /// # Errors
    ///
    /// Returns the sink's error when the event cannot be emitted, or
    /// [`AuditError::AlreadyCompleted`] when the guard already emitted.
    pub fn complete(mut self) -> Result<(), AuditError> {
        self.emit(Outcome::Success)
    }

    /// Emits the record with an aborted outcome.
    ///
    /// # Errors
    ///
    /// Returns the sink's error when the event cannot be emitted, or
    /// [`AuditError::AlreadyCompleted`] when the guard already emitted.
    pub fn abort(mut self) -> Result<(), AuditError> {
        self.emit(Outcome::Aborted)
    }

    /// Emits the record with a panic outcome.
    ///
    /// # Errors
    ///
    /// Returns the sink's error when the event cannot be emitted, or
    /// [`AuditError::AlreadyCompleted`] when the guard already emitted.
    pub fn mark_panic(mut self) -> Result<(), AuditError> {
        self.emit(Outcome::Panic)
    }

    /// Whether the guard already emitted its event.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    /// Emits the record with `outcome`, or reports that it was already emitted.
    fn emit(&mut self, outcome: Outcome) -> Result<(), AuditError> {
        if self.completed.swap(true, Ordering::AcqRel) {
            return Err(AuditError::AlreadyCompleted);
        }

        let record = self.record.take().ok_or(AuditError::AlreadyCompleted)?;
        let event = record.finish_with_outcome(outcome);
        self.sink.emit(&event)
    }
}

impl<S: AuditSink> Drop for CompletionGuard<S> {
    fn drop(&mut self) {
        let outcome = if std::thread::panicking() {
            Outcome::Panic
        } else {
            Outcome::Aborted
        };

        // A guard may only emit during unwinding; swallowing a sink error keeps the panic path
        // intact and preserves the event in the sink if it was already written.
        let _ = self.emit(outcome);
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use http::Method;

    use super::*;
    use crate::sink::{MemorySink, NullSink};

    fn record(id: &str) -> RequestRecord {
        RequestRecord::new(id, Method::GET, "/v1/fetch")
    }

    #[test]
    fn complete_emits_success_once() {
        let sink = Arc::new(MemorySink::new());
        let guard = CompletionGuard::new(record("req-1"), Arc::clone(&sink));

        guard.complete().expect("emits");

        let events = sink.events().expect("not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome(), Outcome::Success);
        assert_eq!(events[0].request_id().as_str(), "req-1");
    }

    #[test]
    fn abort_emits_aborted() {
        let sink = Arc::new(MemorySink::new());
        CompletionGuard::new(record("req-2"), Arc::clone(&sink))
            .abort()
            .expect("emits");

        let events = sink.events().expect("not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome(), Outcome::Aborted);
    }

    #[test]
    fn mark_panic_emits_panic() {
        let sink = Arc::new(MemorySink::new());
        CompletionGuard::new(record("req-3"), Arc::clone(&sink))
            .mark_panic()
            .expect("emits");

        let events = sink.events().expect("not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome(), Outcome::Panic);
    }

    #[test]
    fn dropping_without_completion_emits_aborted() {
        let sink = Arc::new(MemorySink::new());
        {
            let _guard = CompletionGuard::new(record("req-4"), Arc::clone(&sink));
        }

        let events = sink.events().expect("not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome(), Outcome::Aborted);
    }

    #[test]
    fn unwinding_emits_panic() {
        let sink = Arc::new(MemorySink::new());
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = CompletionGuard::new(record("req-5"), Arc::clone(&sink));
            panic!("intentional test panic");
        }));

        assert!(result.is_err());
        let events = sink.events().expect("not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome(), Outcome::Panic);
    }

    #[test]
    fn sink_errors_do_not_double_emit_on_drop() {
        let sink = Arc::new(CountingFailSink::default());
        let guard = CompletionGuard::new(record("req-6"), Arc::clone(&sink));

        assert!(guard.complete().is_err());
        assert_eq!(sink.attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn null_sink_accepts_completion() {
        CompletionGuard::new(record("req-7"), NullSink)
            .complete()
            .expect("the null sink accepts events");
    }

    #[derive(Debug, Default)]
    struct CountingFailSink {
        attempts: AtomicUsize,
    }

    impl AuditSink for CountingFailSink {
        fn emit(&self, _event: &crate::AuditEvent) -> Result<(), AuditError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(AuditError::Io(io::Error::other("sink failure")))
        }
    }
}
