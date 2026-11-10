//! `/v1/search` — searches the web with Kagi.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Extension;
use axum::Json;
use axum::extract::State;
use neuradex_audit::SearchOperation;
use serde::Serialize;
use tracing::Instrument;

use crate::api::v1::error::ApiError;
use crate::api::v1::extract::{ApiJson, SearchRequest};
use crate::audit::RequestAuditHandle;
use crate::metrics::{SearchMetrics, duration_ms};
use crate::state::AppState;

/// The response of the search endpoint.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(examples(json!({
    "query": "rust programming",
    "results": [
        {
            "title": "The Rust Programming Language",
            "url": "https://doc.rust-lang.org/book/",
            "description": "Learn Rust with an official book."
        }
    ],
    "metrics": {"queue_ms": 12, "total_ms": 340, "result_count": 1}
})))]
pub struct SearchResponse {
    /// The search query.
    pub query: String,
    /// The search results.
    pub results: Vec<SearchResult>,
    /// The request metrics and stats.
    pub metrics: SearchMetrics,
}

/// A single search result.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(examples(json!({
    "title": "The Rust Programming Language",
    "url": "https://doc.rust-lang.org/book/",
    "description": "Learn Rust with an official book."
})))]
pub struct SearchResult {
    /// The title of the search result.
    pub title: String,
    /// The URL of the search result.
    #[schema(format = "uri")]
    pub url: String,
    /// The description of the search result.
    pub description: String,
}

impl From<kagi::SearchResult> for SearchResult {
    fn from(result: kagi::SearchResult) -> Self {
        Self {
            title: result.title,
            url: result.url,
            description: result.description,
        }
    }
}

/// Searches the web with Kagi.
///
/// Accepts a JSON request body with the query, an optional result limit, and an optional
/// timeout. Searches are queued: at most the configured `max_sessions` run at once, and the
/// request blocks here until a slot and the results are ready — including while the Kagi client
/// establishes or refreshes a session for the search. The optional `timeout` covers the whole
/// of it: the queue wait, the session wait, and the search itself. Without it, the request
/// stays pending until the client disconnects.
///
/// # Errors
///
/// Returns an error for an invalid request body, failed searches, and searches that exceed the
/// requested timeout.
#[utoipa::path(
    post,
    path = "/v1/search",
    operation_id = "search",
    tag = "search",
    summary = "Search the web.",
    description = "Searches the web through Kagi for a JSON request body. Searches are \
                   queued: at most the configured limit run at once, and the request blocks \
                   until a slot and the results are ready. The optional `timeout` covers the \
                   whole of it — the queue wait, the session wait, and the search itself; \
                   without it, the request stays pending until the client disconnects. Results \
                   are truncated to `limit`.",
    request_body(
        content = SearchRequest,
        content_type = "application/json",
        description = "The query to search for, the maximum number of results, and the \
                       timeout.",
    ),
    responses(
        (status = 200, description = "The search completed", body = SearchResponse),
        ApiError,
    )
)]
pub(crate) async fn search(
    State(state): State<Arc<AppState>>,
    audit: Option<Extension<RequestAuditHandle>>,
    ApiJson { value: request, .. }: ApiJson<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    request.validate()?;

    if let Some(Extension(audit)) = audit {
        let mut operation = SearchOperation::new(&request.query);
        if let Some(limit) = request.limit {
            operation = operation.with_limit(limit);
        }
        audit.update(|record| {
            record.set_search(operation);
        });
    }

    let started = Instant::now();
    let timeout = request.timeout.map(Duration::from_secs);

    let QueuedSearch { queue_ms, results } = queued_search(&state, &request.query, timeout).await?;
    let results: Vec<SearchResult> = results
        .into_iter()
        .take(request.limit.unwrap_or(usize::MAX))
        .map(SearchResult::from)
        .collect();

    let metrics = SearchMetrics {
        queue_ms,
        total_ms: duration_ms(started.elapsed()),
        result_count: results.len(),
    };

    Ok(Json(SearchResponse {
        query: request.query,
        results,
        metrics,
    }))
}

/// A completed search and the time it waited for a slot.
pub(crate) struct QueuedSearch {
    /// The time spent waiting for a search slot before the search started, in milliseconds.
    pub queue_ms: u64,
    /// The raw results returned by Kagi.
    pub results: Vec<kagi::SearchResult>,
}

/// Runs a search behind the shared search gate, returning the queue wait and the raw results.
///
/// The gate blocks while all slots are busy, in FIFO order. Dropping the permit — including when
/// the surrounding timeout cancels the future mid-search — releases the slot. The optional
/// `timeout` covers the queue wait, the session wait, and the search itself.
pub(crate) async fn queued_search(
    state: &AppState,
    query: &str,
    timeout: Option<Duration>,
) -> Result<QueuedSearch, ApiError> {
    let queued = Instant::now();
    let span = tracing::info_span!(
        target: "neuradex::search",
        "search",
        "otel.kind" = "internal",
        "neuradex.search.query.length" = query.chars().count(),
        "neuradex.search.timeout_s" = tracing::field::Empty,
        "neuradex.search.queue_ms" = tracing::field::Empty,
        "neuradex.search.result_count" = tracing::field::Empty,
        "error.type" = tracing::field::Empty,
        "otel.status_code" = tracing::field::Empty,
    );

    if let Some(timeout) = timeout {
        span.record("neuradex.search.timeout_s", timeout.as_secs());
    }

    let search = async {
        let permit = state.search_gate.acquire().await.map_err(|error| {
            ApiError::internal_message(format!("the search gate is closed: {error}"))
        })?;
        let queue_ms = duration_ms(queued.elapsed());

        let results = state
            .kagi_client
            .search(query)
            .await
            .map_err(ApiError::upstream)?;

        // The permit is scoped to the search itself, not the response mapping.
        drop(permit);

        Ok::<_, ApiError>(QueuedSearch { queue_ms, results })
    };

    let result = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, search.instrument(span.clone()))
            .await
            .unwrap_or_else(|_| {
                Err(ApiError::upstream_message(format!(
                    "the search timed out after {}s",
                    timeout.as_secs()
                )))
            }),
        None => search.instrument(span.clone()).await,
    };

    match &result {
        Ok(search) => {
            span.record("neuradex.search.queue_ms", search.queue_ms);
            span.record("neuradex.search.result_count", search.results.len());
        }
        Err(error) => {
            span.record("error.type", error.error_type());
            span.record("otel.status_code", "ERROR");
        }
    }

    let authentication = if state.kagi_client().privacy_pass().is_some() {
        "privacy_pass"
    } else {
        "session"
    };
    // Named audit event: one record per search, independent of trace sampling.
    span.in_scope(|| {
        tracing::event!(
            name: "neuradex.search.request",
            tracing::Level::INFO,
            "neuradex.search.query.length" = query.chars().count(),
            "neuradex.search.authentication" = authentication,
            "neuradex.search.timeout_s" = timeout.map(|timeout| timeout.as_secs()),
            "neuradex.search.queue_ms" = result.as_ref().ok().map(|search| search.queue_ms),
            "neuradex.search.result_count" = result
                .as_ref()
                .ok()
                .map(|search| search.results.len()),
            "error.type" = result.as_ref().err().map(ApiError::error_type),
        );
    });

    result
}
