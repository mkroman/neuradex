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
use crate::api::v1::extract::{ApiQuery, FetchParams, PEEK_INCLUDES};
use crate::api::v1::{AppState, redirect, stream};
use crate::metadata::PageMetadata;
use crate::metrics::Metrics;

/// The response of the peek endpoint.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "utoipa", schema(examples(json!({
    "url": "https://maero.dk/",
    "metadata": {
        "title": "Maero",
        "canonical": "https://maero.dk/",
        "description": "A small API service implementing tools for LLM agents.",
        "og": {"site_name": "Maero"},
        "twitter": {},
        "other": {}
    },
    "metrics": {
        "status": 200,
        "final_url": "https://maero.dk/",
        "http_version": "HTTP/2",
        "content_type": "text/html",
        "content_length": 5123,
        "bytes_read": 1024,
        "ttfb_ms": 87,
        "total_ms": 95,
        "redirects_followed": 0,
        "redirects": [],
        "truncated": false
    }
}))))]
pub struct PeekResponse {
    /// The requested URL.
    #[cfg_attr(feature = "utoipa", schema(format = "uri"))]
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
#[cfg_attr(
    feature = "utoipa",
    utoipa::path(
        get,
        path = "/v1/peek",
        tag = "peek",
        summary = "Fetch a page's head metadata.",
        description = "Fetches the page and returns only its document-head metadata. The response \
                       is streamed through the head parser and the download is aborted as soon as \
                       the head has been received — through `</head>` or the start of `<body>` — so \
                       the endpoint returns as early as possible. Unlike `/v1/fetch`, the body is \
                       not returned and `include=headers` is not supported.",
        params(FetchParams),
        responses(
            (status = 200, description = "The page head was fetched", body = PeekResponse),
            ApiError,
        )
    )
)]
pub(crate) async fn peek(
    State(state): State<Arc<AppState>>,
    ApiQuery(params): ApiQuery<FetchParams>,
) -> Result<Json<PeekResponse>, ApiError> {
    let url = params.url()?;
    let redirects = params.redirects()?;
    let includes = params.includes(PEEK_INCLUDES)?;

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
