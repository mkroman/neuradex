//! `/v1/peek` — fetches a page and returns its document head metadata.
//!
//! The response is streamed through the head parser and the download is aborted as soon as the
//! head has been received, so the endpoint returns as early as possible.

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
use crate::api::v1::extract::{ApiQuery, PEEK_INCLUDES, PageParams};
use crate::api::v1::{redirect, stream};
use crate::audit::RequestAuditHandle;
use crate::metadata::PageMetadata;
use crate::metrics::FetchMetrics;
use crate::state::AppState;

/// The response of the peek endpoint.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(examples(json!({
    "url": "https://example.com/",
    "metadata": {
        "title": "Maero",
        "canonical": "https://example.com/",
        "description": "A small API service implementing tools for LLM agents.",
        "og": {"site_name": "Maero"},
        "twitter": {},
        "other": {}
    },
    "metrics": {
        "status": 200,
        "final_url": "https://example.com/",
        "http_version": "HTTP/2",
        "content_type": "text/html",
        "content_length": 5123,
        "bytes_read": 1024,
        "ttfb_ms": 87,
        "total_ms": 95,
        "redirects_followed": 0,
        "truncated": false
    }
})))]
pub struct PeekResponse {
    /// The requested URL.
    #[schema(format = "uri")]
    pub url: String,
    /// The metadata extracted from the document head.
    pub metadata: PageMetadata,
    /// The request metrics and stats.
    pub metrics: FetchMetrics,
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
    operation_id = "peek",
    tag = "peek",
    summary = "Fetch a page's head metadata.",
    description = "Fetches the page and returns only its document-head metadata. The response \
                   is streamed through the head parser and the download is aborted as soon as \
                   the head has been received — through `</head>` or the start of `<body>` — so \
                   the endpoint returns as early as possible. Unlike `/v1/fetch`, the body is \
                   not returned and `include=headers` is not supported.",
    params(PageParams),
    responses(
        (status = 200, description = "The page head was fetched", body = PeekResponse),
        ApiError,
    )
)]
pub(crate) async fn peek(
    State(state): State<Arc<AppState>>,
    audit: Option<Extension<RequestAuditHandle>>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Response, ApiError> {
    let url = params.url()?;
    let redirects = params.redirects()?;
    let includes = params.includes(PEEK_INCLUDES)?;
    let requested = url.to_string();
    let audit = audit.as_deref();

    if let Some(audit) = audit
        && let Ok(uri) = requested.parse::<axum::http::Uri>()
    {
        audit.update(|record| {
            record.set_peek(FetchOperation::from_uri(&uri));
        });
    }

    let started = Instant::now();
    let (response, fetched) =
        redirect::fetch_with_redirects(&state.client, url, redirects, includes.redirects).await?;

    stream::ensure_text_content(fetched.info.content_type.as_deref())?;
    let final_host = fetched.url.host_str().map(str::to_owned);

    // Only capture the head bytes when content storage is enabled; otherwise the parser keeps
    // its previous zero-copy behavior.
    let read = stream::read_body(
        response,
        stream::BodyMode::Head {
            capture: state.content().is_some(),
        },
        None,
    )
    .await?;
    let stream::StreamedBody {
        metadata,
        body,
        bytes_read,
        truncated,
    } = read;
    let metrics = fetched.into_metrics(bytes_read, truncated, started);
    let content_type = metrics.content_type.clone();

    if let Some(audit) = audit {
        audit.update_fetch(|fetch| {
            let mut updated = fetch
                .clone()
                .with_redirects(metrics.redirects_followed)
                .with_upstream_status(metrics.status)
                .with_bytes_read(metrics.bytes_read)
                .with_ttfb_ms(metrics.ttfb_ms)
                .with_total_ms(metrics.total_ms)
                .with_truncated(metrics.truncated)
                .with_stopped_at_head(true);

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

    let payload = PeekResponse {
        url: requested,
        metadata,
        metrics,
    };
    let encoded = serde_json::to_vec(&payload).map_err(ApiError::internal)?;

    if let Some(audit) = audit {
        audit.update(|record| {
            record.set_response_body_bytes(u64::try_from(encoded.len()).unwrap_or(u64::MAX));
        });
    }

    match body {
        // Store only the bytes peek actually read: the head, never the rest of the page.
        Some(body) => {
            crate::content::capture(
                state.content(),
                audit,
                "peek-response",
                &body,
                Some(
                    content_type
                        .as_deref()
                        .unwrap_or("application/octet-stream"),
                ),
            )
            .await;
        }
        // A capture-disabled read has no body bytes; the event still gets the response JSON so
        // the request has a content reference consistent with the other endpoints.
        None => {
            crate::content::capture(
                state.content(),
                audit,
                "peek-response",
                &encoded,
                Some("application/json"),
            )
            .await;
        }
    }

    Ok(([(CONTENT_TYPE, "application/json")], Body::from(encoded)).into_response())
}
