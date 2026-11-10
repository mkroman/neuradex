//! The builder that turns request facts into one [`AuditEvent`].

use std::time::Duration;

use http::{Method, Uri};

use crate::event::{
    AuditEvent, ContentReference, EventId, FetchOperation, HttpInfo, InfrastructureKind,
    InfrastructureOperation, Operation, Outcome, RequestId, SCHEMA_VERSION, SearchOperation,
    ServiceInfo, Timestamp,
};
use crate::header_policy::HeaderCapture;

/// Accumulates the facts of one request and finishes them into an [`AuditEvent`].
///
/// The record captures its timestamp and event identifier when it is created, so a long request
/// is still ordered by its start time. Setters are chainable and overwrite the previous value.
#[derive(Clone, Debug)]
pub struct RequestRecord {
    event_id: EventId,
    timestamp: Timestamp,
    observed_at: Option<Timestamp>,
    service: ServiceInfo,
    trace_id: Option<String>,
    span_id: Option<String>,
    request_id: RequestId,
    method: Method,
    route: Option<String>,
    path: String,
    status: Option<u16>,
    request_body_bytes: Option<u64>,
    response_body_bytes: Option<u64>,
    duration: Duration,
    outcome: Option<Outcome>,
    outcome_explicit: bool,
    error_type: Option<String>,
    operation: Option<Operation>,
    content: Vec<ContentReference>,
    request_headers: Option<HeaderCapture>,
    response_headers: Option<HeaderCapture>,
}

impl RequestRecord {
    /// Starts a record for a request identified by `request_id`, `method`, and `path`.
    #[must_use]
    pub fn new(request_id: impl Into<RequestId>, method: Method, path: impl Into<String>) -> Self {
        Self {
            event_id: EventId::generate(),
            timestamp: Timestamp::now(),
            observed_at: None,
            service: ServiceInfo::default(),
            trace_id: None,
            span_id: None,
            request_id: request_id.into(),
            method,
            route: None,
            path: path.into(),
            status: None,
            request_body_bytes: None,
            response_body_bytes: None,
            duration: Duration::ZERO,
            outcome: None,
            outcome_explicit: false,
            error_type: None,
            operation: None,
            content: Vec::new(),
            request_headers: None,
            response_headers: None,
        }
    }

    /// Starts a `/v1/search` record, including the query digest.
    #[must_use]
    pub fn search(
        request_id: impl Into<RequestId>,
        method: Method,
        path: impl Into<String>,
        query: &str,
    ) -> Self {
        let mut record = Self::new(request_id, method, path);
        record.set_search(SearchOperation::new(query));
        record
    }

    /// Starts a `/v1/openwebui_search` record, including the query digest.
    #[must_use]
    pub fn openwebui_search(
        request_id: impl Into<RequestId>,
        method: Method,
        path: impl Into<String>,
        query: &str,
    ) -> Self {
        let mut record = Self::new(request_id, method, path);
        record.set_openwebui_search(SearchOperation::new(query));
        record
    }

    /// Starts a `/v1/fetch` record for `target`.
    #[must_use]
    pub fn fetch(
        request_id: impl Into<RequestId>,
        method: Method,
        path: impl Into<String>,
        target: &Uri,
    ) -> Self {
        let mut record = Self::new(request_id, method, path);
        record.set_fetch(FetchOperation::from_uri(target));
        record
    }

    /// Starts a `/v1/peek` record for `target`.
    #[must_use]
    pub fn peek(
        request_id: impl Into<RequestId>,
        method: Method,
        path: impl Into<String>,
        target: &Uri,
    ) -> Self {
        let mut record = Self::new(request_id, method, path);
        record.set_peek(FetchOperation::from_uri(target));
        record
    }

    /// Starts an infrastructure record.
    #[must_use]
    pub fn infrastructure(
        request_id: impl Into<RequestId>,
        method: Method,
        path: impl Into<String>,
        kind: InfrastructureKind,
    ) -> Self {
        let mut record = Self::new(request_id, method, path);
        record.set_operation(Operation::Infrastructure(InfrastructureOperation::new(
            kind,
        )));
        record
    }

    /// Sets the matched route.
    pub fn set_route(&mut self, route: impl Into<String>) -> &mut Self {
        self.route = Some(route.into());
        self
    }

    /// Sets the response status and duration, and derives the outcome when none was set
    /// explicitly.
    pub const fn set_response(&mut self, status: u16, duration: Duration) -> &mut Self {
        self.status = Some(status);
        self.duration = duration;
        if !self.outcome_explicit {
            self.outcome = Some(Outcome::from_status(status));
        }
        self
    }

    /// Sets the response status without changing the duration or outcome.
    pub const fn set_status(&mut self, status: u16) -> &mut Self {
        self.status = Some(status);
        self
    }

    /// Sets the request duration.
    pub const fn set_duration(&mut self, duration: Duration) -> &mut Self {
        self.duration = duration;
        self
    }

    /// Overrides the outcome.
    pub const fn set_outcome(&mut self, outcome: Outcome) -> &mut Self {
        self.outcome = Some(outcome);
        self.outcome_explicit = true;
        self
    }

    /// Sets the machine-readable error type.
    pub fn set_error_type(&mut self, error_type: impl Into<String>) -> &mut Self {
        self.error_type = Some(error_type.into());
        self
    }

    /// Sets the operation-specific fields.
    pub fn set_operation(&mut self, operation: Operation) -> &mut Self {
        self.operation = Some(operation);
        self
    }

    /// Sets a `/v1/search` operation.
    pub fn set_search(&mut self, operation: SearchOperation) -> &mut Self {
        self.set_operation(Operation::Search(operation))
    }

    /// Sets a `/v1/openwebui_search` operation.
    pub fn set_openwebui_search(&mut self, operation: SearchOperation) -> &mut Self {
        self.set_operation(Operation::OpenWebUiSearch(operation))
    }

    /// Sets a `/v1/fetch` operation.
    pub fn set_fetch(&mut self, operation: FetchOperation) -> &mut Self {
        self.set_operation(Operation::Fetch(operation))
    }

    /// Sets a `/v1/peek` operation.
    pub fn set_peek(&mut self, operation: FetchOperation) -> &mut Self {
        self.set_operation(Operation::Peek(operation))
    }

    /// Sets an infrastructure operation.
    pub fn set_infrastructure(&mut self, operation: InfrastructureOperation) -> &mut Self {
        self.set_operation(Operation::Infrastructure(operation))
    }

    /// Returns the operation fields for in-place updates.
    #[must_use]
    pub const fn operation_mut(&mut self) -> Option<&mut Operation> {
        self.operation.as_mut()
    }

    /// Sets the content references.
    pub fn set_content_references(&mut self, references: Vec<ContentReference>) -> &mut Self {
        self.content = references;
        self
    }

    /// Appends a content reference.
    pub fn add_content_reference(&mut self, reference: ContentReference) -> &mut Self {
        self.content.push(reference);
        self
    }

    /// Sets the captured request headers.
    pub fn set_request_headers(&mut self, headers: HeaderCapture) -> &mut Self {
        self.request_headers = Some(headers);
        self
    }

    /// Sets the captured response headers.
    pub fn set_response_headers(&mut self, headers: HeaderCapture) -> &mut Self {
        self.response_headers = Some(headers);
        self
    }

    /// Sets the number of request body bytes.
    pub const fn set_request_body_bytes(&mut self, bytes: u64) -> &mut Self {
        self.request_body_bytes = Some(bytes);
        self
    }

    /// Sets the number of response body bytes.
    pub const fn set_response_body_bytes(&mut self, bytes: u64) -> &mut Self {
        self.response_body_bytes = Some(bytes);
        self
    }

    /// Sets the trace identifier.
    pub fn set_trace_id(&mut self, trace_id: impl Into<String>) -> &mut Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Sets the span identifier.
    pub fn set_span_id(&mut self, span_id: impl Into<String>) -> &mut Self {
        self.span_id = Some(span_id.into());
        self
    }

    /// Sets the event timestamp.
    pub const fn set_timestamp(&mut self, timestamp: Timestamp) -> &mut Self {
        self.timestamp = timestamp;
        self
    }

    /// Sets the time the upstream observation happened, when distinct from the event timestamp.
    pub const fn set_observed_at(&mut self, observed_at: Timestamp) -> &mut Self {
        self.observed_at = Some(observed_at);
        self
    }

    /// Sets the service information.
    pub fn set_service(&mut self, service: ServiceInfo) -> &mut Self {
        self.service = service;
        self
    }

    /// Overrides the generated event identifier.
    pub fn set_event_id(&mut self, event_id: impl Into<EventId>) -> &mut Self {
        self.event_id = event_id.into();
        self
    }

    /// Returns the request identifier.
    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// Returns the operation fields, when set.
    #[must_use]
    pub const fn operation(&self) -> Option<&Operation> {
        self.operation.as_ref()
    }

    /// Finishes the record into an [`AuditEvent`].
    #[must_use]
    pub fn finish(self) -> AuditEvent {
        let outcome = self
            .outcome
            .unwrap_or_else(|| self.status.map_or(Outcome::Success, Outcome::from_status));
        self.finish_with_outcome(outcome)
    }

    /// Finishes the record with a forced outcome.
    #[must_use]
    pub(crate) fn finish_with_outcome(self, outcome: Outcome) -> AuditEvent {
        AuditEvent::new(
            self.event_id,
            self.timestamp,
            self.observed_at,
            self.service,
            self.trace_id,
            self.span_id,
            self.request_id,
            HttpInfo {
                method: self.method.as_str().to_owned(),
                route: self.route,
                path: self.path,
                status: self.status,
                request_body_bytes: self.request_body_bytes,
                response_body_bytes: self.response_body_bytes,
                duration_ms: duration_ms(self.duration),
            },
            outcome,
            self.error_type,
            self.operation,
            self.content,
            self.request_headers,
            self.response_headers,
        )
    }
}

/// Converts a duration to whole milliseconds, saturating at [`u64::MAX`].
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The schema version exported by this crate, re-exported for convenience.
#[must_use]
pub const fn schema_version() -> &'static str {
    SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use time::macros::datetime;

    use super::*;
    use crate::event::{AuthMode, ContentStorage, InfrastructureKind, Outcome, Timestamp};

    fn record() -> RequestRecord {
        let mut record = RequestRecord::new("req-1", Method::GET, "/v1/fetch");
        record.set_timestamp(Timestamp::from_datetime(datetime!(2026-11-10 12:34:56 UTC)));
        record.set_event_id("evt-1");
        record
    }

    #[test]
    fn defaults_are_complete_and_serializable() {
        let event = record().finish();
        let encoded = serde_json::to_value(&event).expect("serializes");

        assert_eq!(encoded["event"], "neuradex.request");
        assert_eq!(encoded["schema_version"], SCHEMA_VERSION);
        assert_eq!(encoded["event_id"], "evt-1");
        assert_eq!(encoded["timestamp"], "2026-11-10T12:34:56Z");
        assert_eq!(encoded["service"]["name"], "neuradex");
        assert_eq!(encoded["request_id"], "req-1");
        assert_eq!(encoded["http"]["method"], "GET");
        assert_eq!(encoded["http"]["path"], "/v1/fetch");
        assert_eq!(encoded["http"]["duration_ms"], 0);
        assert_eq!(encoded["outcome"], "success");
        assert!(encoded.get("observed_at").is_none());
        assert!(encoded.get("route").is_none());
        assert!(encoded["http"].get("status").is_none());
        assert!(encoded.get("operation").is_none());
        assert!(encoded.get("content_references").is_none());
        assert!(encoded.get("request_headers").is_none());
        assert_eq!(event.event_name().as_str(), "neuradex.request");
    }

    #[test]
    fn response_status_derives_the_outcome_unless_explicitly_set() {
        let mut client_error = record();
        client_error.set_response(404, Duration::from_millis(7));
        let event = client_error.finish();

        assert_eq!(event.outcome(), Outcome::ClientError);
        assert_eq!(event.http().status(), Some(404));
        assert_eq!(event.http().duration_ms(), 7);

        let mut server_error = record();
        server_error.set_response(503, Duration::from_millis(9));
        server_error.set_outcome(Outcome::Aborted);
        assert_eq!(server_error.finish().outcome(), Outcome::Aborted);

        let mut replaced = record();
        replaced.set_response(200, Duration::from_millis(1));
        replaced.set_response(500, Duration::from_millis(2));
        assert_eq!(replaced.finish().outcome(), Outcome::ServerError);
    }

    #[test]
    fn search_records_carry_query_and_result_fields() {
        let mut record =
            RequestRecord::search("req-2", Method::POST, "/v1/search", "rust web servers");
        record
            .set_route("/v1/search")
            .set_response(200, Duration::from_millis(40))
            .set_request_body_bytes(35)
            .set_response_body_bytes(512)
            .set_trace_id("trace-1")
            .set_span_id("span-1");
        let operation = record.operation_mut().expect("a search operation");
        let search = operation.as_search_mut().expect("search fields");
        *search = search
            .clone()
            .with_limit(10)
            .with_auth_mode(AuthMode::Session)
            .with_queue_ms(3)
            .with_upstream_ms(30)
            .with_result_counts(12, 2)
            .with_results(["https://example.com/a", "https://docs.example.com/b"]);

        let event = record.finish();
        let encoded: Value = serde_json::to_value(&event).expect("serializes");

        assert_eq!(encoded["operation"]["type"], "search");
        assert_eq!(encoded["operation"]["query_length"], 16);
        assert_eq!(encoded["operation"]["query_terms"], 3);
        assert_eq!(encoded["operation"]["limit"], 10);
        assert_eq!(encoded["operation"]["auth_mode"], "session");
        assert_eq!(encoded["operation"]["queue_ms"], 3);
        assert_eq!(encoded["operation"]["upstream_ms"], 30);
        assert_eq!(encoded["operation"]["upstream_result_count"], 12);
        assert_eq!(encoded["operation"]["returned_result_count"], 2);
        assert_eq!(encoded["operation"]["zero_results"], false);
        assert_eq!(
            encoded["operation"]["top_hosts"][0]["host"],
            "docs.example.com"
        );
        assert_eq!(encoded["operation"]["top_hosts"][0]["count"], 1);
        assert_eq!(encoded["trace_id"], "trace-1");
        assert_eq!(encoded["span_id"], "span-1");
    }

    #[test]
    fn openwebui_search_records_use_their_own_operation_tag() {
        let mut record =
            RequestRecord::openwebui_search("req-3", Method::POST, "/v1/openwebui_search", "q");
        record.set_response(200, Duration::from_millis(1));

        let encoded = serde_json::to_value(record.finish()).expect("serializes");

        assert_eq!(encoded["operation"]["type"], "openwebui_search");
        assert_eq!(encoded["operation"]["query_length"], 1);
    }

    #[test]
    fn fetch_and_peek_records_capture_the_target() {
        let target: Uri = "https://example.com:8443/path?token=secret"
            .parse()
            .expect("valid URI");
        let mut fetch = RequestRecord::fetch("req-4", Method::GET, "/v1/fetch", &target);
        fetch.set_response(200, Duration::from_millis(20));
        if let Some(fetch_fields) = fetch
            .operation_mut()
            .expect("a fetch operation")
            .as_fetch_mut()
        {
            *fetch_fields = fetch_fields
                .clone()
                .with_redirects(2)
                .with_final_host("example.com")
                .with_upstream_status(200)
                .with_content_type("text/html")
                .with_content_length(1_024)
                .with_bytes_read(1_024)
                .with_ttfb_ms(10)
                .with_total_ms(20);
        }

        let encoded = serde_json::to_value(fetch.finish()).expect("serializes");
        assert_eq!(encoded["operation"]["type"], "fetch");
        assert_eq!(encoded["operation"]["target_scheme"], "https");
        assert_eq!(encoded["operation"]["target_host"], "example.com");
        assert_eq!(encoded["operation"]["target_port"], 8443);
        assert_eq!(encoded["operation"]["target_path"], "/path");
        assert_eq!(encoded["operation"]["redirects"], 2);
        assert_eq!(encoded["operation"]["stopped_at_head"], false);

        let mut peek = RequestRecord::peek("req-5", Method::GET, "/v1/peek", &target);
        peek.set_response(200, Duration::from_millis(5));
        if let Some(peek_fields) = peek
            .operation_mut()
            .expect("a peek operation")
            .as_fetch_mut()
        {
            *peek_fields = peek_fields.clone().with_stopped_at_head(true);
        }

        let encoded = serde_json::to_value(peek.finish()).expect("serializes");
        assert_eq!(encoded["operation"]["type"], "peek");
        assert_eq!(encoded["operation"]["stopped_at_head"], true);
    }

    #[test]
    fn infrastructure_and_content_are_recorded() {
        let mut record = RequestRecord::infrastructure(
            "req-6",
            Method::GET,
            "/openapi.json",
            InfrastructureKind::OpenApi,
        );
        record.set_response(200, Duration::from_millis(2));
        record.add_content_reference(ContentReference::new(
            ContentStorage::Object,
            "objects/page.html",
            "blake3-hex",
            "sha256-hex",
            42,
        ));

        let encoded = serde_json::to_value(record.finish()).expect("serializes");

        assert_eq!(encoded["operation"]["type"], "infrastructure");
        assert_eq!(encoded["operation"]["kind"], "openapi");
        assert_eq!(encoded["content_references"][0]["storage"], "object");
        assert_eq!(encoded["content_references"][0]["size"], 42);
    }

    #[test]
    fn result_count_shapes_zero_results() {
        let search = SearchOperation::new("query")
            .with_result_counts(0, 0)
            .with_results(Vec::<String>::new());

        assert!(search.zero_results());
        assert_eq!(search.returned_result_count(), Some(0));
        assert_eq!(search.top_hosts(), []);
    }

    #[test]
    fn events_round_trip_through_json() {
        let mut record = RequestRecord::search("req-8", Method::POST, "/v1/search", "rust");
        record.set_response(200, Duration::from_millis(12));
        record.set_search(
            SearchOperation::new("rust")
                .with_limit(5)
                .with_result_counts(5, 5)
                .with_results(["https://example.com/a"]),
        );
        let event = record.finish();

        let encoded = serde_json::to_string(&event).expect("serializes");
        let decoded: AuditEvent = serde_json::from_str(&encoded).expect("deserializes");

        assert_eq!(decoded, event);
    }

    #[test]
    fn schema_version_is_re_exported() {
        assert_eq!(schema_version(), SCHEMA_VERSION);
    }
}
