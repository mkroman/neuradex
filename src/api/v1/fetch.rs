//! `/v1/fetch` — fetches a page and returns its metadata, body, headers, and metrics.

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::api::v1::error::ApiError;
use crate::api::v1::extract::FetchParams;
use crate::api::v1::{AppState, redirect, stream};
use crate::metadata::PageMetadata;
use crate::metrics::Metrics;

/// The maximum size of a fetched response body.
pub(crate) const FETCH_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// The response of the fetch endpoint.
#[derive(Debug, Serialize)]
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
    pub request_headers: Option<std::collections::BTreeMap<String, String>>,
    /// The response headers, when requested through `include=headers`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    /// The request metrics and stats.
    pub metrics: Metrics,
}

/// Handles `GET /v1/fetch`.
///
/// # Errors
///
/// Returns an error for invalid parameters, binary content types, and failed requests.
pub(crate) async fn fetch(
    State(state): State<Arc<AppState>>,
    params: FetchParams,
) -> Result<Json<FetchResponse>, ApiError> {
    let started = Instant::now();
    let (response, fetched) =
        redirect::fetch(&state.client, params.url.clone(), params.redirects).await?;

    stream::ensure_text_content(&response)?;

    let response_headers = params
        .includes
        .headers
        .then(|| stream::headers_to_json(response.headers()));

    let read = stream::read(response, stream::Mode::Full(FETCH_MAX_BYTES)).await?;
    let body = String::from_utf8_lossy(read.body.as_deref().unwrap_or_default()).into_owned();
    let metrics = fetched.into_metrics(
        read.bytes_read,
        read.truncated,
        params.includes.redirects,
        started,
    );

    Ok(Json(FetchResponse {
        url: params.url.to_string(),
        metadata: read.metadata,
        body,
        truncated: read.truncated,
        request_headers: params
            .includes
            .headers
            .then(|| stream::headers_to_json(&state.default_headers)),
        response_headers,
        metrics,
    }))
}
