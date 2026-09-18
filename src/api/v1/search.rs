//! `/v1/search` — searches the web with Kagi.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::api::v1::error::ApiError;
use crate::api::v1::extract::SearchParams;
use crate::api::v1::{AppState, redirect};
use crate::metrics::SearchMetrics;

/// The response of the search endpoint.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// The search query.
    pub query: String,
    /// The search results.
    pub results: Vec<SearchResult>,
    /// The request metrics and stats.
    pub metrics: SearchMetrics,
}

/// A single search result.
#[derive(Clone, Debug, Serialize)]
pub struct SearchResult {
    /// The title of the search result.
    pub title: String,
    /// The URL of the search result.
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

/// Handles `GET /v1/search`.
///
/// Searches are queued: at most [`MAX_CONCURRENT_SEARCHES`] run at once, and the request blocks
/// here until a slot and the results are ready. The optional `timeout` covers both the queue
/// wait and the search itself.
///
/// # Errors
///
/// Returns an error for invalid parameters, failed searches, and searches that exceed the
/// requested timeout.
pub(crate) async fn search(
    State(state): State<Arc<AppState>>,
    params: SearchParams,
) -> Result<Json<SearchResponse>, ApiError> {
    let started = Instant::now();

    let timed_out = |timeout: Option<Duration>| {
        ApiError::upstream(timeout.map_or_else(
            || "the search timed out".to_owned(),
            |timeout| format!("the search timed out after {}s", timeout.as_secs()),
        ))
    };

    // Blocks while all search slots are busy; FIFO order. Dropping the permit — including when
    // the surrounding timeout cancels the future mid-search — releases the slot.
    let (_permit, queue_ms, mut results) =
        tokio::time::timeout(params.timeout.unwrap_or(Duration::MAX), async {
            let permit = state
                .search_gate
                .clone()
                .acquire_owned()
                .await
                .map_err(|error| {
                    ApiError::internal(format!("the search gate is closed: {error}"))
                })?;
            let queue_ms = redirect::duration_ms(started.elapsed());

            let results: Vec<SearchResult> = state
                .kagi_client
                .search(&params.query)
                .await
                .map_err(|error| ApiError::upstream(error.to_string()))?
                .into_iter()
                .map(SearchResult::from)
                .collect();

            Ok::<_, ApiError>((permit, queue_ms, results))
        })
        .await
        .map_err(|_| timed_out(params.timeout))??;

    if let Some(limit) = params.limit {
        results.truncate(limit);
    }

    let metrics = SearchMetrics {
        queue_ms,
        total_ms: redirect::duration_ms(started.elapsed()),
        result_count: results.len(),
    };

    Ok(Json(SearchResponse {
        query: params.query,
        results,
        metrics,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn truncates_results_to_the_limit() {
        let results: Vec<SearchResult> = (0..5)
            .map(|index| SearchResult {
                title: format!("title {index}"),
                url: format!("https://maero.dk/{index}"),
                description: String::new(),
            })
            .collect();

        let mut limited = results.clone();
        limited.truncate(3);
        assert_eq!(limited.len(), 3);
        assert_eq!(limited.last().unwrap().url, "https://maero.dk/2");

        let mut unlimited = results;
        unlimited.truncate(usize::MAX);
        assert_eq!(unlimited.len(), 5);
    }

    #[test]
    fn serializes_the_queue_wait_metric() {
        let metrics = SearchMetrics {
            queue_ms: 42,
            total_ms: 100,
            result_count: 1,
        };

        assert_eq!(
            serde_json::to_value(&metrics).unwrap(),
            json!({
                "queue_ms": 42,
                "total_ms": 100,
                "result_count": 1,
            })
        );
    }
}
