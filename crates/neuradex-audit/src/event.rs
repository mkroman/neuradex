//! The canonical audit event schema.
//!
//! The schema is deliberately self-contained: it has no dependency on the HTTP service, the
//! content plane, or Kagi. Producers build a [`RequestRecord`](crate::RequestRecord) and finish
//! it into an [`AuditEvent`]; sinks only see the finished event.
//!
//! JSON field names are stable and snake_case. Optional fields are omitted from the
//! representation while they are unset, so a small event stays small; the non-optional fields
//! are always present.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use http::Uri;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;

use crate::digest::sha256_hex;

/// The schema version emitted in every [`AuditEvent`].
pub const SCHEMA_VERSION: &str = "1.0.0";

/// The maximum number of result hosts recorded by [`SearchOperation`].
const TOP_HOSTS_LIMIT: usize = 10;

/// The counter that keeps generated identifiers unique within the process.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A stable event name, e.g. `neuradex.request`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventName(String);

impl EventName {
    /// The event name of every request audit event.
    pub const REQUEST: &'static str = "neuradex.request";

    /// Returns the request audit event name.
    #[must_use]
    pub fn request() -> Self {
        Self(Self::REQUEST.to_owned())
    }

    /// Returns the event name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for EventName {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for EventName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for EventName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A globally unique audit event identifier.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(String);

impl EventId {
    /// Generates an event identifier from the current time and a process-local counter.
    #[must_use]
    pub fn generate() -> Self {
        Self(generate_id("evt"))
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for EventId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for EventId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for EventId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A request identifier shared by every record of one inbound request.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    /// Generates a request identifier from the current time and a process-local counter.
    #[must_use]
    pub fn generate() -> Self {
        Self(generate_id("req"))
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for RequestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Generates an identifier that is unique within the process and extremely likely unique across
/// processes: the process id, the current Unix time in nanoseconds, and a counter are mixed in
/// hex.
fn generate_id(prefix: &str) -> String {
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let pid = std::process::id();

    format!("{prefix}-{pid:x}-{nanos:x}-{counter:x}")
}

/// A UTC timestamp rendered as RFC 3339.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    /// Returns the current time in UTC.
    #[must_use]
    pub fn now() -> Self {
        Self(OffsetDateTime::now_utc())
    }

    /// Normalizes `datetime` to UTC and wraps it.
    #[must_use]
    pub const fn from_datetime(datetime: OffsetDateTime) -> Self {
        Self(datetime.to_offset(UtcOffset::UTC))
    }

    /// Parses an RFC 3339 timestamp and normalizes it to UTC.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is not a valid RFC 3339 timestamp.
    pub fn parse_rfc3339(value: &str) -> Result<Self, time::error::Parse> {
        Ok(Self::from_datetime(OffsetDateTime::parse(value, &Rfc3339)?))
    }

    /// Returns the wrapped timestamp.
    #[must_use]
    pub const fn as_datetime(self) -> OffsetDateTime {
        self.0
    }

    /// Returns the number of seconds since the Unix epoch.
    #[must_use]
    pub const fn unix_timestamp(self) -> i64 {
        self.0.unix_timestamp()
    }
}

impl Default for Timestamp {
    fn default() -> Self {
        Self(OffsetDateTime::UNIX_EPOCH)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.format(&Rfc3339) {
            Ok(text) => formatter.write_str(&text),
            // An `OffsetDateTime` always formats successfully; keep the fallback total so a
            // display call can never panic in a Drop handler.
            Err(_) => formatter.write_str(&self.0.unix_timestamp().to_string()),
        }
    }
}

impl From<OffsetDateTime> for Timestamp {
    fn from(value: OffsetDateTime) -> Self {
        Self::from_datetime(value)
    }
}

impl Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse_rfc3339(&value).map_err(de::Error::custom)
    }
}

/// The service that emitted an audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServiceInfo {
    name: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<String>,
}

impl ServiceInfo {
    /// Builds service information for `name` and `version`.
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            environment: None,
        }
    }

    /// Sets the deployment environment, e.g. `production` or `staging`.
    #[must_use]
    pub fn with_environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = Some(environment.into());
        self
    }

    /// Returns the service name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the service version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the deployment environment, when configured.
    #[must_use]
    pub fn environment(&self) -> Option<&str> {
        self.environment.as_deref()
    }
}

impl Default for ServiceInfo {
    fn default() -> Self {
        Self {
            name: "neuradex".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            environment: None,
        }
    }
}

/// The HTTP facts of the audited request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HttpInfo {
    pub(crate) method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) route: Option<String>,
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) request_body_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_body_bytes: Option<u64>,
    pub(crate) duration_ms: u64,
}

impl HttpInfo {
    /// Returns the request method.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Returns the matched route, when the request matched one.
    #[must_use]
    pub fn route(&self) -> Option<&str> {
        self.route.as_deref()
    }

    /// Returns the request path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the response status, when a response was produced.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    /// Returns the number of request body bytes, when known.
    #[must_use]
    pub const fn request_body_bytes(&self) -> Option<u64> {
        self.request_body_bytes
    }

    /// Returns the number of response body bytes, when known.
    #[must_use]
    pub const fn response_body_bytes(&self) -> Option<u64> {
        self.response_body_bytes
    }

    /// Returns the request duration in whole milliseconds.
    #[must_use]
    pub const fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
}

/// The final outcome of an audited request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Outcome {
    /// The request completed successfully.
    Success,
    /// The client caused the failure, typically a 4xx response.
    ClientError,
    /// The service or an upstream caused the failure, typically a 5xx response.
    ServerError,
    /// The request was aborted before it completed.
    Aborted,
    /// The request panicked.
    Panic,
}

impl Outcome {
    /// Maps an HTTP status to an outcome.
    ///
    /// Statuses below 400 are successful, 4xx statuses are client errors, and 5xx statuses are
    /// server errors.
    #[must_use]
    pub const fn from_status(status: u16) -> Self {
        match status {
            400..=499 => Self::ClientError,
            500..=599 => Self::ServerError,
            _ => Self::Success,
        }
    }

    /// Whether this outcome represents an unsuccessful request.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        !matches!(self, Self::Success)
    }
}

/// The operation-specific fields of an audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Operation {
    /// A `/v1/search` request.
    Search(SearchOperation),
    /// A `/v1/openwebui_search` request.
    #[serde(rename = "openwebui_search")]
    OpenWebUiSearch(SearchOperation),
    /// A `/v1/fetch` request.
    Fetch(FetchOperation),
    /// A `/v1/peek` request.
    Peek(FetchOperation),
    /// An infrastructure endpoint or a router fallback.
    Infrastructure(InfrastructureOperation),
}

impl Operation {
    /// Returns the stable operation name used in the JSON representation.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Search(_) => "search",
            Self::OpenWebUiSearch(_) => "openwebui_search",
            Self::Fetch(_) => "fetch",
            Self::Peek(_) => "peek",
            Self::Infrastructure(_) => "infrastructure",
        }
    }

    /// Returns the search fields for a search operation.
    #[must_use]
    pub const fn as_search(&self) -> Option<&SearchOperation> {
        match self {
            Self::Search(operation) | Self::OpenWebUiSearch(operation) => Some(operation),
            _ => None,
        }
    }

    /// Returns the search fields mutably for a search operation.
    pub const fn as_search_mut(&mut self) -> Option<&mut SearchOperation> {
        match self {
            Self::Search(operation) | Self::OpenWebUiSearch(operation) => Some(operation),
            _ => None,
        }
    }

    /// Returns the fetch fields for a fetch or peek operation.
    #[must_use]
    pub const fn as_fetch(&self) -> Option<&FetchOperation> {
        match self {
            Self::Fetch(operation) | Self::Peek(operation) => Some(operation),
            _ => None,
        }
    }

    /// Returns the fetch fields mutably for a fetch or peek operation.
    pub const fn as_fetch_mut(&mut self) -> Option<&mut FetchOperation> {
        match self {
            Self::Fetch(operation) | Self::Peek(operation) => Some(operation),
            _ => None,
        }
    }

    /// Returns the infrastructure fields for an infrastructure operation.
    #[must_use]
    pub const fn as_infrastructure(&self) -> Option<&InfrastructureOperation> {
        match self {
            Self::Infrastructure(operation) => Some(operation),
            _ => None,
        }
    }
}

/// The authentication mode used for a search.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthMode {
    /// Kagi's Privacy Pass tokens.
    PrivacyPass,
    /// A Kagi session cookie.
    Session,
}

/// One host and the number of search results it contributed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HostCount {
    host: String,
    count: u64,
}

impl HostCount {
    /// Builds a host count.
    #[must_use]
    pub fn new(host: impl Into<String>, count: u64) -> Self {
        Self {
            host: host.into(),
            count,
        }
    }

    /// Returns the host.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the number of results from the host.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }
}

/// The search-specific fields of an audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SearchOperation {
    query_length: usize,
    query_terms: usize,
    query_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_mode: Option<AuthMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_result_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    returned_result_count: Option<usize>,
    zero_results: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    top_hosts: Vec<HostCount>,
}

impl SearchOperation {
    /// Builds search fields from the query, hashing the query with SHA-256.
    ///
    /// `query_length` counts Unicode scalar values, not bytes; `query_terms` counts
    /// whitespace-separated terms. The query itself is never stored.
    #[must_use]
    pub fn new(query: &str) -> Self {
        Self {
            query_length: query.chars().count(),
            query_terms: query.split_whitespace().count(),
            query_sha256: sha256_hex(query.as_bytes()),
            limit: None,
            count: None,
            auth_mode: None,
            queue_ms: None,
            upstream_ms: None,
            upstream_result_count: None,
            returned_result_count: None,
            zero_results: false,
            result_digest: None,
            top_hosts: Vec::new(),
        }
    }

    /// Sets the requested result limit of `/v1/search`.
    #[must_use]
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Sets the requested result count of `/v1/openwebui_search`.
    #[must_use]
    pub const fn with_count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// Sets the search authentication mode.
    #[must_use]
    pub const fn with_auth_mode(mut self, auth_mode: AuthMode) -> Self {
        self.auth_mode = Some(auth_mode);
        self
    }

    /// Sets the time spent waiting for a search slot.
    #[must_use]
    pub const fn with_queue_ms(mut self, queue_ms: u64) -> Self {
        self.queue_ms = Some(queue_ms);
        self
    }

    /// Sets the time spent in the upstream search.
    #[must_use]
    pub const fn with_upstream_ms(mut self, upstream_ms: u64) -> Self {
        self.upstream_ms = Some(upstream_ms);
        self
    }

    /// Sets the number of results returned by the upstream and the number returned to the
    /// client, and derives [`zero_results`](Self::zero_results) from the latter.
    #[must_use]
    pub const fn with_result_counts(mut self, upstream: usize, returned: usize) -> Self {
        self.upstream_result_count = Some(upstream);
        self.returned_result_count = Some(returned);
        self.zero_results = returned == 0;
        self
    }

    /// Records the returned result URLs: their SHA-256 digest, their count, and the most common
    /// hosts. At most ten hosts are kept, ordered by count and then host name.
    #[must_use]
    pub fn with_results<I, S>(mut self, urls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut digest = Sha256::new();
        let mut hosts: BTreeMap<String, u64> = BTreeMap::new();
        let mut returned = 0_usize;

        for (index, url) in urls.into_iter().enumerate() {
            let url = url.as_ref();
            if index > 0 {
                digest.update(b"\n");
            }
            digest.update(url.as_bytes());
            returned = returned.saturating_add(1);

            if let Ok(uri) = url.parse::<Uri>()
                && let Some(host) = uri.host()
            {
                *hosts.entry(host.to_ascii_lowercase()).or_default() += 1;
            }
        }

        let mut ranked: Vec<HostCount> = hosts
            .into_iter()
            .map(|(host, count)| HostCount::new(host, count))
            .collect();
        ranked.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.host.cmp(&right.host))
        });
        ranked.truncate(TOP_HOSTS_LIMIT);

        self.returned_result_count = Some(returned);
        self.zero_results = returned == 0;
        self.result_digest = Some(const_hex::encode(digest.finalize()));
        self.top_hosts = ranked;
        self
    }

    /// Sets a precomputed digest of the returned result URLs.
    #[must_use]
    pub fn with_result_digest(mut self, digest: impl Into<String>) -> Self {
        self.result_digest = Some(digest.into());
        self
    }

    /// Sets the number of results returned by the upstream.
    #[must_use]
    pub const fn with_upstream_result_count(mut self, count: usize) -> Self {
        self.upstream_result_count = Some(count);
        self
    }

    /// Sets the number of results returned to the client.
    #[must_use]
    pub const fn with_returned_result_count(mut self, count: usize) -> Self {
        self.returned_result_count = Some(count);
        self.zero_results = count == 0;
        self
    }

    /// Returns the query length in Unicode scalar values.
    #[must_use]
    pub const fn query_length(&self) -> usize {
        self.query_length
    }

    /// Returns the number of whitespace-separated query terms.
    #[must_use]
    pub const fn query_terms(&self) -> usize {
        self.query_terms
    }

    /// Returns the lowercase hex SHA-256 digest of the query.
    #[must_use]
    pub fn query_sha256(&self) -> &str {
        &self.query_sha256
    }

    /// Returns the requested `/v1/search` result limit.
    #[must_use]
    pub const fn limit(&self) -> Option<usize> {
        self.limit
    }

    /// Returns the requested `/v1/openwebui_search` result count.
    #[must_use]
    pub const fn count(&self) -> Option<usize> {
        self.count
    }

    /// Returns the search authentication mode.
    #[must_use]
    pub const fn auth_mode(&self) -> Option<AuthMode> {
        self.auth_mode
    }

    /// Returns the time spent waiting for a search slot, in milliseconds.
    #[must_use]
    pub const fn queue_ms(&self) -> Option<u64> {
        self.queue_ms
    }

    /// Returns the time spent in the upstream search, in milliseconds.
    #[must_use]
    pub const fn upstream_ms(&self) -> Option<u64> {
        self.upstream_ms
    }

    /// Returns the number of results returned by the upstream.
    #[must_use]
    pub const fn upstream_result_count(&self) -> Option<usize> {
        self.upstream_result_count
    }

    /// Returns the number of results returned to the client.
    #[must_use]
    pub const fn returned_result_count(&self) -> Option<usize> {
        self.returned_result_count
    }

    /// Whether the search returned no results.
    #[must_use]
    pub const fn zero_results(&self) -> bool {
        self.zero_results
    }

    /// Returns the digest of the returned result URLs, when recorded.
    #[must_use]
    pub fn result_digest(&self) -> Option<&str> {
        self.result_digest.as_deref()
    }

    /// Returns the most common result hosts.
    #[must_use]
    pub fn top_hosts(&self) -> &[HostCount] {
        &self.top_hosts
    }
}

/// The fetch- and peek-specific fields of an audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FetchOperation {
    #[serde(skip_serializing_if = "Option::is_none")]
    target_scheme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_port: Option<u16>,
    target_path: String,
    target_url_sha256: String,
    redirects: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttfb_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_ms: Option<u64>,
    truncated: bool,
    stopped_at_head: bool,
}

impl FetchOperation {
    /// Captures the target of a fetch or peek from an absolute URI.
    ///
    /// Only the scheme, host, port, and path are stored; the full URL is hashed with SHA-256 so
    /// query strings and userinfo never reach the event.
    #[must_use]
    pub fn from_uri(uri: &Uri) -> Self {
        Self {
            target_scheme: uri.scheme_str().map(str::to_owned),
            target_host: uri.host().map(str::to_owned),
            target_port: uri.port_u16(),
            target_path: uri.path().to_owned(),
            target_url_sha256: sha256_hex(uri.to_string().as_bytes()),
            redirects: 0,
            final_host: None,
            upstream_status: None,
            content_type: None,
            content_length: None,
            bytes_read: None,
            ttfb_ms: None,
            total_ms: None,
            truncated: false,
            stopped_at_head: false,
        }
    }

    /// Captures the target of a fetch or peek from a URL string.
    ///
    /// # Errors
    ///
    /// Returns an error when `url` is not a valid absolute URI.
    pub fn from_url_str(url: &str) -> Result<Self, crate::AuditError> {
        let uri: Uri = url
            .parse()
            .map_err(|error| crate::AuditError::invalid_value("target_url", error))?;
        Ok(Self::from_uri(&uri))
    }

    /// Sets the number of redirects followed.
    #[must_use]
    pub const fn with_redirects(mut self, redirects: u32) -> Self {
        self.redirects = redirects;
        self
    }

    /// Sets the host of the final response.
    #[must_use]
    pub fn with_final_host(mut self, host: impl Into<String>) -> Self {
        self.final_host = Some(host.into());
        self
    }

    /// Sets the status of the final upstream response.
    #[must_use]
    pub const fn with_upstream_status(mut self, status: u16) -> Self {
        self.upstream_status = Some(status);
        self
    }

    /// Sets the media type of the final response.
    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Sets the announced content length of the final response.
    #[must_use]
    pub const fn with_content_length(mut self, content_length: u64) -> Self {
        self.content_length = Some(content_length);
        self
    }

    /// Sets the number of decompressed bytes read from the final response.
    #[must_use]
    pub const fn with_bytes_read(mut self, bytes_read: u64) -> Self {
        self.bytes_read = Some(bytes_read);
        self
    }

    /// Sets the time to first byte of the final response, in milliseconds.
    #[must_use]
    pub const fn with_ttfb_ms(mut self, ttfb_ms: u64) -> Self {
        self.ttfb_ms = Some(ttfb_ms);
        self
    }

    /// Sets the total fetch or peek time, in milliseconds.
    #[must_use]
    pub const fn with_total_ms(mut self, total_ms: u64) -> Self {
        self.total_ms = Some(total_ms);
        self
    }

    /// Sets whether the returned body was truncated by a size limit.
    #[must_use]
    pub const fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// Sets whether the read stopped at the end of the document head.
    #[must_use]
    pub const fn with_stopped_at_head(mut self, stopped_at_head: bool) -> Self {
        self.stopped_at_head = stopped_at_head;
        self
    }

    /// Returns the target scheme.
    #[must_use]
    pub fn target_scheme(&self) -> Option<&str> {
        self.target_scheme.as_deref()
    }

    /// Returns the target host.
    #[must_use]
    pub fn target_host(&self) -> Option<&str> {
        self.target_host.as_deref()
    }

    /// Returns the target port, when explicitly present.
    #[must_use]
    pub const fn target_port(&self) -> Option<u16> {
        self.target_port
    }

    /// Returns the target path.
    #[must_use]
    pub fn target_path(&self) -> &str {
        &self.target_path
    }

    /// Returns the lowercase hex SHA-256 digest of the full target URL.
    #[must_use]
    pub fn target_url_sha256(&self) -> &str {
        &self.target_url_sha256
    }

    /// Returns the number of redirects followed.
    #[must_use]
    pub const fn redirects(&self) -> u32 {
        self.redirects
    }

    /// Returns the host of the final response.
    #[must_use]
    pub fn final_host(&self) -> Option<&str> {
        self.final_host.as_deref()
    }

    /// Returns the status of the final upstream response.
    #[must_use]
    pub const fn upstream_status(&self) -> Option<u16> {
        self.upstream_status
    }

    /// Returns the media type of the final response.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// Returns the announced content length of the final response.
    #[must_use]
    pub const fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// Returns the number of decompressed bytes read from the final response.
    #[must_use]
    pub const fn bytes_read(&self) -> Option<u64> {
        self.bytes_read
    }

    /// Returns the time to first byte of the final response, in milliseconds.
    #[must_use]
    pub const fn ttfb_ms(&self) -> Option<u64> {
        self.ttfb_ms
    }

    /// Returns the total fetch or peek time, in milliseconds.
    #[must_use]
    pub const fn total_ms(&self) -> Option<u64> {
        self.total_ms
    }

    /// Whether the returned body was truncated by a size limit.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Whether the read stopped at the end of the document head.
    #[must_use]
    pub const fn stopped_at_head(&self) -> bool {
        self.stopped_at_head
    }
}

/// The kind of infrastructure request being audited.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InfrastructureKind {
    /// The health endpoint.
    Health,
    /// The OpenAPI document endpoint.
    #[serde(rename = "openapi")]
    OpenApi,
    /// The documentation page.
    Docs,
    /// A router-level 404 or 405 fallback.
    Fallback,
    /// Any other infrastructure endpoint.
    Other,
}

/// The infrastructure-specific fields of an audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InfrastructureOperation {
    kind: InfrastructureKind,
}

impl InfrastructureOperation {
    /// Builds infrastructure fields for `kind`.
    #[must_use]
    pub const fn new(kind: InfrastructureKind) -> Self {
        Self { kind }
    }

    /// Returns the infrastructure kind.
    #[must_use]
    pub const fn kind(&self) -> InfrastructureKind {
        self.kind
    }
}

/// Where a content-addressed object is stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentStorage {
    /// A packfile in the packfile store.
    Pack,
    /// An individual object in object storage.
    Object,
}

/// A reference to content stored outside the event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContentReference {
    storage: ContentStorage,
    key: String,
    blake3: String,
    sha256: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
}

impl ContentReference {
    /// Builds a content reference.
    #[must_use]
    pub fn new(
        storage: ContentStorage,
        key: impl Into<String>,
        blake3: impl Into<String>,
        sha256: impl Into<String>,
        size: u64,
    ) -> Self {
        Self {
            storage,
            key: key.into(),
            blake3: blake3.into(),
            sha256: sha256.into(),
            size,
            content_type: None,
        }
    }

    /// Builds a reference to an object in a packfile.
    #[must_use]
    pub fn pack(
        key: impl Into<String>,
        blake3: impl Into<String>,
        sha256: impl Into<String>,
        size: u64,
    ) -> Self {
        Self::new(ContentStorage::Pack, key, blake3, sha256, size)
    }

    /// Builds a reference to an individual object in object storage.
    #[must_use]
    pub fn object(
        key: impl Into<String>,
        blake3: impl Into<String>,
        sha256: impl Into<String>,
        size: u64,
    ) -> Self {
        Self::new(ContentStorage::Object, key, blake3, sha256, size)
    }

    /// Sets the media type of the referenced content.
    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Returns the storage class.
    #[must_use]
    pub const fn storage(&self) -> ContentStorage {
        self.storage
    }

    /// Returns the storage key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the lowercase hex BLAKE3 digest.
    #[must_use]
    pub fn blake3(&self) -> &str {
        &self.blake3
    }

    /// Returns the lowercase hex SHA-256 digest.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Returns the content size in bytes.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns the media type of the referenced content.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }
}

/// A complete, canonical audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    event: EventName,
    schema_version: String,
    event_id: EventId,
    timestamp: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<Timestamp>,
    service: ServiceInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span_id: Option<String>,
    request_id: RequestId,
    http: HttpInfo,
    outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<Operation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    content_references: Vec<ContentReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_headers: Option<crate::HeaderCapture>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_headers: Option<crate::HeaderCapture>,
}

impl AuditEvent {
    /// Assembles a finished event.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        event_id: EventId,
        timestamp: Timestamp,
        observed_at: Option<Timestamp>,
        service: ServiceInfo,
        trace_id: Option<String>,
        span_id: Option<String>,
        request_id: RequestId,
        http: HttpInfo,
        outcome: Outcome,
        error_type: Option<String>,
        operation: Option<Operation>,
        content_references: Vec<ContentReference>,
        request_headers: Option<crate::HeaderCapture>,
        response_headers: Option<crate::HeaderCapture>,
    ) -> Self {
        Self {
            event: EventName::request(),
            schema_version: SCHEMA_VERSION.to_owned(),
            event_id,
            timestamp,
            observed_at,
            service,
            trace_id,
            span_id,
            request_id,
            http,
            outcome,
            error_type,
            operation,
            content_references,
            request_headers,
            response_headers,
        }
    }

    /// Returns the event name.
    #[must_use]
    pub const fn event_name(&self) -> &EventName {
        &self.event
    }

    /// Returns the schema version.
    #[must_use]
    pub fn schema_version(&self) -> &str {
        &self.schema_version
    }

    /// Returns the event identifier.
    #[must_use]
    pub const fn event_id(&self) -> &EventId {
        &self.event_id
    }

    /// Returns when the event was created.
    #[must_use]
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns when the upstream observation happened, when distinct from [`timestamp`](Self::timestamp).
    #[must_use]
    pub const fn observed_at(&self) -> Option<Timestamp> {
        self.observed_at
    }

    /// Returns the service that emitted the event.
    #[must_use]
    pub const fn service(&self) -> &ServiceInfo {
        &self.service
    }

    /// Returns the trace identifier, when the request was traced.
    #[must_use]
    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.as_deref()
    }

    /// Returns the span identifier, when the request was traced.
    #[must_use]
    pub fn span_id(&self) -> Option<&str> {
        self.span_id.as_deref()
    }

    /// Returns the request identifier.
    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// Returns the HTTP facts.
    #[must_use]
    pub const fn http(&self) -> &HttpInfo {
        &self.http
    }

    /// Returns the final outcome.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Returns the machine-readable error type, when the request failed.
    #[must_use]
    pub fn error_type(&self) -> Option<&str> {
        self.error_type.as_deref()
    }

    /// Returns the operation-specific fields.
    #[must_use]
    pub const fn operation(&self) -> Option<&Operation> {
        self.operation.as_ref()
    }

    /// Returns the content references.
    #[must_use]
    pub fn content_references(&self) -> &[ContentReference] {
        &self.content_references
    }

    /// Returns the captured request headers.
    #[must_use]
    pub const fn request_headers(&self) -> Option<&crate::HeaderCapture> {
        self.request_headers.as_ref()
    }

    /// Returns the captured response headers.
    #[must_use]
    pub const fn response_headers(&self) -> Option<&crate::HeaderCapture> {
        self.response_headers.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn request_event_name_is_stable() {
        let name = EventName::request();

        assert_eq!(name.as_str(), "neuradex.request");
        assert_eq!(name.to_string(), "neuradex.request");
        assert_eq!(
            serde_json::to_value(&name).expect("serializes"),
            json!("neuradex.request")
        );
    }

    #[test]
    fn generated_ids_are_unique_and_prefixed() {
        let first = EventId::generate();
        let second = EventId::generate();

        assert_ne!(first, second);
        assert!(first.as_str().starts_with("evt-"));
        assert!(RequestId::generate().as_str().starts_with("req-"));
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let timestamp = Timestamp::from_datetime(datetime!(2026-11-10 12:34:56.789 UTC));
        let encoded = serde_json::to_value(timestamp).expect("serializes");

        let text = encoded.as_str().expect("a timestamp is a string");
        assert!(text.starts_with("2026-11-10T12:34:56.789"));
        assert!(text.ends_with('Z') || text.ends_with("+00:00"), "{text}");
        assert_eq!(
            Timestamp::parse_rfc3339(text).expect("parses"),
            timestamp,
            "the representation round-trips"
        );
    }

    #[test]
    fn offsets_are_normalized_to_utc() {
        let parsed = Timestamp::parse_rfc3339("2026-11-10T13:34:56+01:00").expect("parses");

        assert_eq!(parsed.unix_timestamp(), 1_794_314_096);
        assert_eq!(parsed.to_string(), "2026-11-10T12:34:56Z");
    }

    #[test]
    fn outcomes_follow_status_classes() {
        assert_eq!(Outcome::from_status(200), Outcome::Success);
        assert_eq!(Outcome::from_status(302), Outcome::Success);
        assert_eq!(Outcome::from_status(404), Outcome::ClientError);
        assert_eq!(Outcome::from_status(503), Outcome::ServerError);
        assert!(Outcome::ClientError.is_failure());
        assert!(Outcome::ServerError.is_failure());
        assert!(Outcome::Panic.is_failure());
        assert!(!Outcome::Success.is_failure());
        assert!(Outcome::Aborted.is_failure());
    }

    #[test]
    fn search_results_are_digested_and_ranked() {
        let operation = SearchOperation::new("rust web servers")
            .with_limit(10)
            .with_result_counts(4, 3)
            .with_results([
                "https://example.com/one",
                "https://docs.example.com/two",
                "https://example.com/three",
            ]);

        assert_eq!(operation.query_length(), 16);
        assert_eq!(operation.query_terms(), 3);
        assert_eq!(operation.query_sha256().len(), 64);
        assert_eq!(operation.returned_result_count(), Some(3));
        assert!(!operation.zero_results());
        assert_eq!(
            operation.top_hosts(),
            &[
                HostCount::new("example.com", 2),
                HostCount::new("docs.example.com", 1),
            ]
        );
        assert_eq!(operation.result_digest().expect("a digest").len(), 64);
    }

    #[test]
    fn empty_results_set_zero_results() {
        let operation = SearchOperation::new("nothing").with_results(Vec::<String>::new());

        assert!(operation.zero_results());
        assert_eq!(operation.returned_result_count(), Some(0));
        assert_eq!(operation.top_hosts(), []);
        assert_eq!(
            operation.result_digest(),
            Some(sha256_hex(b"").as_str()),
            "the empty result set is still digested"
        );
    }

    #[test]
    fn fetch_targets_are_sanitized_and_hashed() {
        let uri: Uri = "https://example.com:8443/a/path?token=secret#fragment"
            .parse()
            .expect("valid URI");
        let operation = FetchOperation::from_uri(&uri);

        assert_eq!(operation.target_scheme(), Some("https"));
        assert_eq!(operation.target_host(), Some("example.com"));
        assert_eq!(operation.target_port(), Some(8443));
        assert_eq!(operation.target_path(), "/a/path");
        assert_eq!(operation.target_url_sha256().len(), 64);
        assert_ne!(
            operation.target_url_sha256(),
            sha256_hex(b"https://example.com:8443/a/path")
        );
    }

    #[test]
    fn invalid_fetch_targets_report_the_field() {
        let error = FetchOperation::from_url_str("not a URL").expect_err("the target is invalid");

        assert_eq!(error.field(), Some("target_url"));
    }

    #[test]
    fn operations_serialize_with_stable_tags() {
        let search = Operation::Search(SearchOperation::new("rust"));
        let openwebui = Operation::OpenWebUiSearch(SearchOperation::new("rust"));
        let fetch =
            Operation::Fetch(FetchOperation::from_url_str("https://example.com").expect("valid"));
        let peek =
            Operation::Peek(FetchOperation::from_url_str("https://example.com").expect("valid"));
        let infrastructure =
            Operation::Infrastructure(InfrastructureOperation::new(InfrastructureKind::OpenApi));

        assert_eq!(search.as_str(), "search");
        assert_eq!(openwebui.as_str(), "openwebui_search");
        assert_eq!(fetch.as_str(), "fetch");
        assert_eq!(peek.as_str(), "peek");
        assert_eq!(infrastructure.as_str(), "infrastructure");

        for (operation, expected) in [
            (search, "search"),
            (openwebui, "openwebui_search"),
            (fetch, "fetch"),
            (peek, "peek"),
            (infrastructure, "infrastructure"),
        ] {
            let encoded = serde_json::to_value(&operation).expect("serializes");
            assert_eq!(encoded["type"], expected);
        }
    }

    #[test]
    fn content_references_serialize_storage_and_digests() {
        let reference = ContentReference::pack("packs/2026-11-10.pack", "abc", "def", 42)
            .with_content_type("text/html");

        let encoded = serde_json::to_value(&reference).expect("serializes");

        assert_eq!(encoded["storage"], "pack");
        assert_eq!(encoded["key"], "packs/2026-11-10.pack");
        assert_eq!(encoded["blake3"], "abc");
        assert_eq!(encoded["sha256"], "def");
        assert_eq!(encoded["size"], 42);
        assert_eq!(encoded["content_type"], "text/html");
    }
}
