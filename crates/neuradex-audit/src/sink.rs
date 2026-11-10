//! Synchronous audit event sinks.
//!
//! The service wraps a sink in whatever runtime-specific queue or background writer it needs;
//! this crate deliberately keeps [`AuditSink`] synchronous and object-safe so the event schema
//! stays independent of any async runtime.

use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::AuditError;
use crate::event::AuditEvent;

/// A destination for finished audit events.
///
/// Implementations must be safe to call from multiple threads. `emit` should not block for long;
/// production sinks should hand the event to a background writer.
pub trait AuditSink: Send + Sync {
    /// Emits one event.
    ///
    /// # Errors
    ///
    /// Returns an error when the event cannot be serialized or written.
    fn emit(&self, event: &AuditEvent) -> Result<(), AuditError>;
}

impl<T> AuditSink for &T
where
    T: AuditSink + ?Sized,
{
    fn emit(&self, event: &AuditEvent) -> Result<(), AuditError> {
        (**self).emit(event)
    }
}

impl<T> AuditSink for Arc<T>
where
    T: AuditSink + ?Sized,
{
    fn emit(&self, event: &AuditEvent) -> Result<(), AuditError> {
        (**self).emit(event)
    }
}

impl<T> AuditSink for Box<T>
where
    T: AuditSink + ?Sized,
{
    fn emit(&self, event: &AuditEvent) -> Result<(), AuditError> {
        (**self).emit(event)
    }
}

/// A sink that writes one compact JSON object per line.
///
/// Each event is serialized as a single line, followed by `\n`, and the writer is flushed after
/// every event so a batching layer can decide when to flush itself.
#[derive(Debug)]
pub struct JsonlSink<W: Write + Send> {
    writer: Mutex<W>,
}

impl<W: Write + Send> JsonlSink<W> {
    /// Builds a sink around `writer`.
    #[must_use]
    pub const fn new(writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }

    /// Returns the writer after all emitted events have been flushed.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError::Poisoned`] when a previous `emit` panicked while holding the writer
    /// lock.
    pub fn into_inner(self) -> Result<W, AuditError> {
        self.writer.into_inner().map_err(|_| AuditError::Poisoned)
    }
}

impl<W> AuditSink for JsonlSink<W>
where
    W: Write + Send,
{
    fn emit(&self, event: &AuditEvent) -> Result<(), AuditError> {
        let mut writer = self.writer.lock().map_err(|_| AuditError::Poisoned)?;
        serde_json::to_writer(&mut *writer, event)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        drop(writer);
        Ok(())
    }
}

/// A sink that retains emitted events in memory.
///
/// This is intended for tests and local inspection; it grows without bound.
#[derive(Debug, Default)]
pub struct MemorySink {
    events: Mutex<Vec<AuditEvent>>,
}

impl MemorySink {
    /// Builds an empty in-memory sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a snapshot of the emitted events.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError::Poisoned`] when a previous `emit` panicked while holding the event
    /// lock.
    pub fn events(&self) -> Result<Vec<AuditEvent>, AuditError> {
        self.events
            .lock()
            .map(|events| events.clone())
            .map_err(|_| AuditError::Poisoned)
    }

    /// Returns the number of emitted events.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError::Poisoned`] when a previous `emit` panicked while holding the event
    /// lock.
    pub fn len(&self) -> Result<usize, AuditError> {
        self.events
            .lock()
            .map(|events| events.len())
            .map_err(|_| AuditError::Poisoned)
    }

    /// Whether no events have been emitted.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError::Poisoned`] when a previous `emit` panicked while holding the event
    /// lock.
    pub fn is_empty(&self) -> Result<bool, AuditError> {
        self.len().map(|len| len == 0)
    }

    /// Removes every retained event.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError::Poisoned`] when a previous `emit` panicked while holding the event
    /// lock.
    pub fn clear(&self) -> Result<(), AuditError> {
        self.events
            .lock()
            .map(|mut events| events.clear())
            .map_err(|_| AuditError::Poisoned)
    }
}

impl AuditSink for MemorySink {
    fn emit(&self, event: &AuditEvent) -> Result<(), AuditError> {
        self.events
            .lock()
            .map(|mut events| events.push(event.clone()))
            .map_err(|_| AuditError::Poisoned)
    }
}

/// A sink that discards every event.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSink;

impl AuditSink for NullSink {
    fn emit(&self, _event: &AuditEvent) -> Result<(), AuditError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use http::Method;
    use serde_json::Value;

    use super::*;
    use crate::RequestRecord;

    fn event(id: &str, path: &str) -> AuditEvent {
        let mut record = RequestRecord::new(id, Method::GET, path);
        record.set_response(200, std::time::Duration::from_millis(3));
        record.finish()
    }

    #[test]
    fn jsonl_writes_one_compact_line_per_event() {
        let sink = JsonlSink::new(Vec::new());
        sink.emit(&event("req-1", "/v1/fetch")).expect("emits");
        sink.emit(&event("req-2", "/v1/peek")).expect("emits");

        let bytes = sink.into_inner().expect("the lock is intact");
        let text = String::from_utf8(bytes).expect("the sink writes UTF-8");
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines.len(), 2);
        assert!(!lines[0].contains('\n'), "one event is one line");
        let parsed: Value = serde_json::from_str(lines[0]).expect("each line is JSON");
        assert_eq!(parsed["event"], "neuradex.request");
        assert_eq!(parsed["request_id"], "req-1");
        assert_eq!(parsed["http"]["path"], "/v1/fetch");
    }

    #[test]
    fn memory_sink_captures_events() {
        let sink = MemorySink::new();
        assert!(sink.is_empty().expect("not poisoned"));

        sink.emit(&event("req-1", "/v1/fetch")).expect("emits");
        sink.emit(&event("req-2", "/v1/peek")).expect("emits");

        let events = sink.events().expect("not poisoned");
        assert_eq!(sink.len().expect("not poisoned"), 2);
        assert_eq!(events[0].request_id().as_str(), "req-1");
        assert_eq!(events[1].http().path(), "/v1/peek");

        sink.clear().expect("not poisoned");
        assert!(sink.is_empty().expect("not poisoned"));
    }

    #[test]
    fn null_sink_discards_events() {
        NullSink.emit(&event("req-1", "/v1/fetch")).expect("emits");
    }

    #[test]
    fn arc_and_box_delegate() {
        let sink = Arc::new(MemorySink::new());
        sink.emit(&event("req-1", "/v1/fetch")).expect("emits");
        assert_eq!(sink.len().expect("not poisoned"), 1);

        let boxed: Box<dyn AuditSink> = Box::new(MemorySink::new());
        boxed
            .emit(&event("req-2", "/v1/peek"))
            .expect("the trait object emits");
    }

    #[test]
    fn references_delegate() {
        let sink = MemorySink::new();
        let borrowed: &dyn AuditSink = &sink;

        borrowed
            .emit(&event("req-1", "/v1/fetch"))
            .expect("the reference emits");

        assert_eq!(sink.len().expect("not poisoned"), 1);
    }
}
