//! `/v1/fetch` — fetches a page and returns its metadata, body, headers, and metrics.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use crate::api::v1::error::ApiError;
use crate::api::v1::extract::{FETCH_INCLUDES, FetchQuery, ValidatedQuery};
use crate::api::v1::{AppState, redirect, stream};
use crate::metadata::PageMetadata;
use crate::metrics::Metrics;

/// The maximum size of a fetched response body.
pub(crate) const FETCH_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// The response of the fetch endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct FetchResponse {
    /// The requested URL.
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
#[utoipa::path(
    get,
    path = "/v1/fetch",
    tag = "fetch",
    params(FetchQuery),
    responses(
        (status = 200, description = "The page was fetched", body = FetchResponse),
        ApiError,
    )
)]
pub(crate) async fn fetch(
    State(state): State<Arc<AppState>>,
    ValidatedQuery(query): ValidatedQuery<FetchQuery>,
) -> Result<Json<FetchResponse>, ApiError> {
    let url = query.url()?;
    let redirects = query.redirects()?;
    let includes = query.includes(FETCH_INCLUDES)?;

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
