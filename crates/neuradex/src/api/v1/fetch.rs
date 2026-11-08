//! `/v1/fetch` — fetches a page and returns its metadata, body, headers, and metrics.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::api::v1::error::ApiError;
use crate::api::v1::extract::{ApiQuery, FETCH_INCLUDES, FetchParams};
use crate::api::v1::{AppState, redirect, stream};
use crate::metadata::PageMetadata;
use crate::metrics::Metrics;

/// The maximum size of a fetched response body.
pub(crate) const FETCH_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// The response of the fetch endpoint.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "utoipa", schema(examples(json!({
    "url": "https://example.com/",
    "metadata": {
        "title": "Maero",
        "canonical": "https://example.com/",
        "description": "A small API service implementing tools for LLM agents.",
        "og": {"site_name": "Maero"},
        "twitter": {},
        "other": {"viewport": "width=device-width"}
    },
    "body": "<!doctype html>\n<html>…</html>",
    "truncated": false,
    "request_headers": {"user-agent": "neuradex/0.1", "accept-encoding": "gzip, deflate, br, zstd"},
    "response_headers": {"content-type": "text/html; charset=utf-8", "content-length": "5123"},
    "metrics": {
        "status": 200,
        "final_url": "https://example.com/",
        "http_version": "HTTP/2",
        "content_type": "text/html",
        "content_length": 5123,
        "bytes_read": 5123,
        "ttfb_ms": 87,
        "total_ms": 120,
        "redirects_followed": 0,
        "redirects": [],
        "truncated": false
    }
}))))]
pub struct FetchResponse {
    /// The requested URL.
    #[cfg_attr(feature = "utoipa", schema(format = "uri"))]
    pub url: String,
    /// The metadata extracted from the document head.
    pub metadata: PageMetadata,
    /// The response body, decoded as UTF-8 with invalid sequences replaced.
    pub body: String,
    /// Whether the body was cut short by the size limit.
    pub truncated: bool,
    /// The request headers, when requested through `include=headers`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<BTreeMap<String, String>>,
    /// The response headers, when requested through `include=headers`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<BTreeMap<String, String>>,
    /// The request metrics and stats.
    pub metrics: Metrics,
}

/// Fetches a page and returns its metadata, body, headers, and metrics.
///
/// Follows up to `redirects` redirects by hand and reports the metrics of the final response.
/// The body is decoded as UTF-8 text; binary content types are rejected. Unlike `/v1/peek`, the
/// full body is returned and `include=headers` is supported.
///
/// # Errors
///
/// Returns an error for invalid parameters, binary content types, and failed requests.
#[cfg_attr(
    feature = "utoipa",
    utoipa::path(
        get,
        path = "/v1/fetch",
        tag = "fetch",
        summary = "Fetch a page.",
        description = "Fetches the page and returns its document-head metadata, body, and request \
                       metrics. Up to `redirects` redirects are followed by hand, and each hop can \
                       be reported through `include=redirects`. The body is decoded as UTF-8 text \
                       with invalid sequences replaced; binary content types are rejected, and \
                       bodies beyond 25 MiB are cut short with `truncated: true`. The request and \
                       response headers are returned when `include=headers` is requested.",
        params(FetchParams),
        responses(
            (status = 200, description = "The page was fetched", body = FetchResponse),
            ApiError,
        )
    )
)]
pub(crate) async fn fetch(
    State(state): State<Arc<AppState>>,
    ApiQuery(params): ApiQuery<FetchParams>,
) -> Result<Json<FetchResponse>, ApiError> {
    let url = params.url()?;
    let redirects = params.redirects()?;
    let includes = params.includes(FETCH_INCLUDES)?;

    let started = Instant::now();
    let (response, fetched) = redirect::fetch(&state.client, url.clone(), redirects).await?;

    stream::ensure_text_content(&response)?;

    let response_headers = includes
        .headers
        .then(|| stream::headers_to_json(response.headers()));

    let read = stream::read(response, stream::Mode::Full(FETCH_MAX_BYTES)).await?;
    let body = String::from_utf8_lossy(read.body.as_deref().unwrap_or_default()).into_owned();
    let metrics =
        fetched.into_metrics(read.bytes_read, read.truncated, includes.redirects, started);

    Ok(Json(FetchResponse {
        url: url.to_string(),
        metadata: read.metadata,
        body,
        truncated: read.truncated,
        request_headers: includes
            .headers
            .then(|| stream::headers_to_json(&state.default_headers)),
        response_headers,
        metrics,
    }))
}
