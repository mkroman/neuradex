//! `/v1/fetch` — fetches a page and returns its metadata, body, headers, and metrics.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use neuradex_audit::FetchOperation;
use serde::Serialize;

use crate::api::v1::error::ApiError;
use crate::api::v1::extract::{ApiQuery, FETCH_INCLUDES, PageParams};
use crate::api::v1::{redirect, stream};
use crate::audit::RequestAuditHandle;
use crate::metadata::PageMetadata;
use crate::metrics::FetchMetrics;
use crate::state::AppState;

/// The maximum size of a fetched response body.
pub(crate) const FETCH_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// The response of the fetch endpoint.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(examples(json!({
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
        "truncated": false
    }
})))]
pub struct FetchResponse {
    /// The requested URL.
    #[schema(format = "uri")]
    pub url: String,
    /// The metadata extracted from the document head.
    pub metadata: PageMetadata,
    /// The response body, decoded as UTF-8 with invalid sequences replaced.
    pub body: String,
    /// Whether the body was cut short by the size limit.
    pub truncated: bool,
    /// The configured default request headers, when requested through `include=headers`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<BTreeMap<String, String>>,
    /// The response headers, when requested through `include=headers`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<BTreeMap<String, String>>,
    /// The request metrics and stats.
    pub metrics: FetchMetrics,
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
    operation_id = "fetch",
    tag = "fetch",
    summary = "Fetch a page.",
    description = "Fetches the page and returns its document-head metadata, body, and request \
                   metrics. Up to `redirects` redirects are followed by hand, and each hop can \
                   be reported through `include=redirects`. The body is decoded as UTF-8 text \
                   with invalid sequences replaced; binary content types are rejected, and \
                   bodies beyond 25 MiB are cut short with `truncated: true`. The request and \
                   response headers are returned when `include=headers` is requested.",
    params(PageParams),
    responses(
        (status = 200, description = "The page was fetched", body = FetchResponse),
        ApiError,
    )
)]
pub(crate) async fn fetch(
    State(state): State<Arc<AppState>>,
    audit: Option<Extension<RequestAuditHandle>>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Response, ApiError> {
    let url = params.url()?;
    let redirects = params.redirects()?;
    let includes = params.includes(FETCH_INCLUDES)?;
    let requested = url.to_string();
    let audit = audit.as_deref();

    if let Some(audit) = audit
        && let Ok(uri) = requested.parse::<axum::http::Uri>()
    {
        audit.update(|record| {
            record.set_fetch(FetchOperation::from_uri(&uri));
        });
    }

    let started = Instant::now();
    let (response, fetched) =
        redirect::fetch_with_redirects(&state.client, url, redirects, includes.redirects).await?;

    stream::ensure_text_content(fetched.info.content_type.as_deref())?;
    let final_host = fetched.url.host_str().map(str::to_owned);

    let response_headers = includes
        .headers
        .then(|| stream::headers_to_json(response.headers()));

    let read = stream::read_body(
        response,
        stream::BodyMode::Full(FETCH_MAX_BYTES),
        fetched.info.content_length,
    )
    .await?;
    let body = String::from_utf8(read.body.expect("full mode collects the body"))
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
    let metrics = fetched.into_metrics(read.bytes_read, read.truncated, started);

    if let Some(audit) = audit {
        audit.update_fetch(|fetch| {
            let mut updated = fetch
                .clone()
                .with_redirects(metrics.redirects_followed)
                .with_upstream_status(metrics.status)
                .with_bytes_read(metrics.bytes_read)
                .with_ttfb_ms(metrics.ttfb_ms)
                .with_total_ms(metrics.total_ms)
                .with_truncated(metrics.truncated);

            if let Some(host) = &final_host {
                updated = updated.with_final_host(host.clone());
            }
            if let Some(content_type) = &metrics.content_type {
                updated = updated.with_content_type(content_type.clone());
            }
            if let Some(content_length) = metrics.content_length {
                updated = updated.with_content_length(content_length);
            }

            *fetch = updated;
        });
    }

    crate::content::capture(
        state.content(),
        audit,
        "fetch-response",
        body.as_bytes(),
        Some(
            metrics
                .content_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        ),
    )
    .await;

    // Serialize into a pre-sized buffer: axum's `Json` starts from a small buffer and grows it
    // geometrically, re-copying a body of several megabytes as it does.
    let body_capacity = body.len().saturating_add(1024);
    let payload = FetchResponse {
        url: requested,
        metadata: read.metadata,
        body,
        truncated: read.truncated,
        request_headers: includes
            .headers
            .then(|| stream::headers_to_json(&state.default_headers)),
        response_headers,
        metrics,
    };

    let mut encoded = Vec::with_capacity(body_capacity);
    serde_json::to_writer(&mut encoded, &payload).map_err(ApiError::internal)?;

    if let Some(audit) = audit {
        audit.update(|record| {
            record.set_response_body_bytes(u64::try_from(encoded.len()).unwrap_or(u64::MAX));
        });
    }

    Ok(([(CONTENT_TYPE, "application/json")], Body::from(encoded)).into_response())
}
