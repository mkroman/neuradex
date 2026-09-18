//! `/v1/peek` — fetches a page and returns its document head metadata.
//!
//! The response is streamed through the head parser and the download is aborted as soon as the
//! head has been received, so the endpoint returns as early as possible.

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::api::v1::error::ApiError;
use crate::api::v1::extract::PeekParams;
use crate::api::v1::{AppState, redirect, stream};
use crate::metadata::PageMetadata;
use crate::metrics::Metrics;

/// The response of the peek endpoint.
#[derive(Debug, Serialize)]
pub struct PeekResponse {
    /// The requested URL.
    pub url: String,
    /// The metadata extracted from the document head.
    pub metadata: PageMetadata,
    /// The request metrics and stats.
    pub metrics: Metrics,
}

/// Handles `GET /v1/peek`.
///
/// # Errors
///
/// Returns an error for invalid parameters, binary content types, and failed requests.
pub(crate) async fn peek(
    State(state): State<Arc<AppState>>,
    params: PeekParams,
) -> Result<Json<PeekResponse>, ApiError> {
    let started = Instant::now();
    let (response, fetched) =
        redirect::fetch(&state.client, params.url.clone(), params.redirects).await?;

    stream::ensure_text_content(&response)?;

    let read = stream::read(response, stream::Mode::Head).await?;
    let metrics = fetched.into_metrics(
        read.bytes_read,
        read.truncated,
        params.includes.redirects,
        started,
    );

    Ok(Json(PeekResponse {
        url: params.url.to_string(),
        metadata: read.metadata,
        metrics,
    }))
}
