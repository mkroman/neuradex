//! `/v1/peek` — fetches a page and returns its document head metadata.
//!
//! The response is streamed through the head parser and the download is aborted as soon as the
//! head has been received, so the endpoint returns as early as possible.

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum_extra::extract::Query;
use serde::Serialize;
use utoipa::ToSchema;

use crate::api::v1::error::ApiError;
use crate::api::v1::extract::{FetchQuery, PEEK_INCLUDES};
use crate::api::v1::{AppState, redirect, stream};
use crate::metadata::PageMetadata;
use crate::metrics::Metrics;

/// The response of the peek endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct PeekResponse {
    /// The requested URL.
    pub url: String,
    /// The metadata extracted from the document head.
    pub metadata: PageMetadata,
    /// The request metrics and stats.
    pub metrics: Metrics,
}

/// Fetches a page and returns its document head metadata.
///
/// The response is streamed through the head parser and the download is aborted as soon as the
/// head has been received, so the endpoint returns as early as possible. Unlike `/v1/fetch`, the
/// body is not returned and `include=headers` is not supported.
///
/// # Errors
///
/// Returns an error for invalid parameters, binary content types, and failed requests.
#[utoipa::path(
    get,
    path = "/v1/peek",
    tag = "peek",
    params(FetchQuery),
    responses(
        (status = 200, description = "The page head was fetched", body = PeekResponse),
        ApiError,
    )
)]
pub(crate) async fn peek(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FetchQuery>,
) -> Result<Json<PeekResponse>, ApiError> {
    let url = query.url()?;
    let redirects = query.redirects()?;
    let includes = query.includes(PEEK_INCLUDES)?;

    let started = Instant::now();
    let (response, fetched) = redirect::fetch(&state.client, url.clone(), redirects).await?;

    stream::ensure_text_content(&response)?;

    let read = stream::read(response, stream::Mode::Head).await?;
    let metrics =
        fetched.into_metrics(read.bytes_read, read.truncated, includes.redirects, started);

    Ok(Json(PeekResponse {
        url: url.to_string(),
        metadata: read.metadata,
        metrics,
    }))
}
