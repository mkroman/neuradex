//! Request metrics and stats shared by the API endpoints.

use std::time::Duration;

use serde::Serialize;

/// Converts a duration to whole milliseconds.
#[must_use]
pub(crate) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// A single intermediate redirect hop.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(examples(json!({"status": 301, "url": "https://example.com/index.html"})))]
pub struct RedirectHop {
    /// The status code of the redirect response.
    pub status: u16,
    /// The URL that returned the redirect.
    #[schema(format = "uri")]
    pub url: String,
}

/// Request metrics and stats for a fetched page.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = Metrics, examples(json!({
    "status": 200,
    "final_url": "https://example.com/",
    "http_version": "HTTP/2",
    "content_type": "text/html",
    "content_length": 5123,
    "bytes_read": 5123,
    "ttfb_ms": 87,
    "total_ms": 120,
    "redirects_followed": 0,
    "truncated": false
})))]
pub struct FetchMetrics {
    /// The status code of the final response.
    pub status: u16,
    /// The URL of the final response, after following redirects.
    #[schema(format = "uri")]
    pub final_url: String,
    /// The HTTP version of the final response, e.g. `HTTP/1.1`.
    #[schema(example = "HTTP/2")]
    pub http_version: String,
    /// The content type of the final response, without parameters.
    #[schema(example = "text/html")]
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
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(examples(json!({"queue_ms": 12, "total_ms": 340, "result_count": 10})))]
pub struct SearchMetrics {
    /// The time spent waiting for a search slot before the search started, in milliseconds.
    pub queue_ms: u64,
    /// The total processing time, in milliseconds.
    pub total_ms: u64,
    /// The number of results returned.
    pub result_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_durations() {
        assert_eq!(duration_ms(Duration::from_millis(1_234)), 1_234);
        assert_eq!(duration_ms(Duration::from_nanos(999)), 0);
    }
}
