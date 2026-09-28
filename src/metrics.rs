//! Request metrics and stats shared by the API endpoints.

use serde::Serialize;
use utoipa::ToSchema;

/// A single intermediate redirect hop.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RedirectHop {
    /// The status code of the redirect response.
    pub status: u16,
    /// The URL that returned the redirect.
    pub url: String,
}

/// Request metrics and stats for a fetched page.
#[derive(Debug, Serialize, ToSchema)]
pub struct Metrics {
    /// The status code of the final response.
    pub status: u16,
    /// The URL of the final response, after following redirects.
    pub final_url: String,
    /// The HTTP version of the final response, e.g. `HTTP/1.1`.
    pub http_version: String,
    /// The content type of the final response, without parameters.
    pub content_type: Option<String>,
    /// The content length announced by the final response, if any.
    pub content_length: Option<u64>,
    /// The number of body bytes read (decompressed).
    pub bytes_read: u64,
    /// The time until the headers of the final response were received, in milliseconds.
    pub ttfb_ms: u64,
    /// The total processing time, in milliseconds.
    pub total_ms: u64,
    /// The number of redirects followed.
    pub redirects_followed: u32,
    /// The intermediate redirect hops, when requested through `include=redirects`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<RedirectHop>,
    /// Whether the response was cut short by a size limit.
    pub truncated: bool,
}

/// Request metrics and stats for a search.
#[derive(Debug, Serialize, ToSchema)]
pub struct SearchMetrics {
    /// The time spent waiting for a search slot before the search started, in milliseconds.
    pub queue_ms: u64,
    /// The total processing time, in milliseconds.
    pub total_ms: u64,
    /// The number of results returned.
    pub result_count: usize,
}
