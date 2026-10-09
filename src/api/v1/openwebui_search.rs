//! `/v1/openwebui_search` — searches the web with Kagi for Open WebUI.
//!
//! Implements the external web search API Open WebUI expects when its web search engine is set
//! to `external`: a `POST` with a `{"query", "count"}` JSON body answered by a bare JSON array
//! of `{link, title, snippet}` results — see
//! <https://docs.openwebui.com/features/chat-conversations/web-search/providers/external/>.
//! The contract leaves no room for envelopes or metrics in the response: results (or an empty
//! array) are the whole body, and errors render the service's JSON error envelope, which Open
//! WebUI handles like any failed upstream search.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::api::v1::AppState;
use crate::api::v1::error::ApiError;
use crate::api::v1::extract::ApiJson;

/// The number of results returned when the request carries no `count`.
pub(crate) const DEFAULT_COUNT: usize = 5;

/// The request body of the Open WebUI external search endpoint, as it arrives on the wire.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[cfg_attr(
    feature = "utoipa",
    schema(examples(json!({"query": "rust programming", "count": 5})))
)]
#[serde(deny_unknown_fields)]
pub struct OpenWebUiSearchRequest {
    /// The user's search query string.
    #[cfg_attr(feature = "utoipa", schema(example = "rust programming"))]
    pub query: String,
    /// The suggested maximum number of results; defaults to [`DEFAULT_COUNT`] when omitted.
    #[cfg_attr(feature = "utoipa", schema(default = 5, example = 5))]
    pub count: Option<usize>,
}

impl OpenWebUiSearchRequest {
    /// Resolves the requested result count, defaulting to [`DEFAULT_COUNT`].
    pub(crate) fn count(&self) -> usize {
        self.count.unwrap_or(DEFAULT_COUNT)
    }

    /// Validates the request body.
    ///
    /// # Errors
    ///
    /// Returns an error when the query is empty.
    pub(crate) fn validate(&self) -> Result<(), ApiError> {
        if self.query.is_empty() {
            return Err(ApiError::invalid_param("the query is empty"));
        }

        Ok(())
    }
}

/// A single search result, in the shape Open WebUI expects.
#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "utoipa", schema(examples(json!({
    "link": "https://doc.rust-lang.org/book/",
    "title": "The Rust Programming Language",
    "snippet": "Learn Rust with an official book."
}))))]
pub struct OpenWebUiSearchResult {
    /// The direct URL to the search result.
    #[cfg_attr(feature = "utoipa", schema(format = "uri"))]
    pub link: String,
    /// The title of the web page.
    pub title: String,
    /// A snippet from the page content relevant to the query.
    pub snippet: String,
}

impl From<kagi::SearchResult> for OpenWebUiSearchResult {
    fn from(result: kagi::SearchResult) -> Self {
        Self {
            link: result.url,
            title: result.title,
            snippet: result.description,
        }
    }
}

/// Searches the web with Kagi, for Open WebUI's `external` web search engine.
///
/// Answers the external web search API's `POST` body — `{"query", "count"}` — with the bare
/// JSON array of `{link, title, snippet}` results it expects, truncated to `count` (default
/// [`DEFAULT_COUNT`]). Like [`search`](crate::api::v1::search::search), searches are queued: at
/// most `MAX_CONCURRENT_SEARCHES` run at once, and the request blocks here until a slot and
/// the results are ready.
///
/// # Errors
///
/// Returns an error for an invalid request body and failed searches. A search that simply
/// finds nothing is not an error: it renders as the empty array the specification recommends.
#[cfg_attr(
    feature = "utoipa",
    utoipa::path(
        post,
        path = "/v1/openwebui_search",
        tag = "search",
        summary = "Search the web for Open WebUI.",
        description = "Searches the web through Kagi and answers in the shape Open WebUI's \
                       `external` web search engine expects: a bare JSON array of \
                       `{link, title, snippet}` objects, truncated to `count`. Searches are \
                       queued: at most two run at once, and the request blocks until a slot \
                       and the results are ready.",
        request_body(
            content = OpenWebUiSearchRequest,
            content_type = "application/json",
            description = "The query to search for and the suggested maximum number of results.",
        ),
        responses(
            (
                status = 200,
                description = "The search completed; an empty array means no results.",
                body = [OpenWebUiSearchResult],
            ),
            ApiError,
        )
    )
)]
pub(crate) async fn openwebui_search(
    State(state): State<Arc<AppState>>,
    ApiJson(request): ApiJson<OpenWebUiSearchRequest>,
) -> Result<Json<Vec<OpenWebUiSearchResult>>, ApiError> {
    request.validate()?;

    // Blocks while all search slots are busy; FIFO order. Dropping the permit when the handler
    // returns releases the slot.
    let _permit = state
        .search_gate
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| ApiError::internal(format!("the search gate is closed: {error}")))?;

    let mut results: Vec<OpenWebUiSearchResult> = state
        .kagi_client
        .search(&request.query)
        .await
        .map_err(|error| ApiError::upstream(error.to_string()))?
        .into_iter()
        .map(OpenWebUiSearchResult::from)
        .collect();

    let count = request.count();
    results.truncate(count);

    Ok(Json(results))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn maps_kagi_results_to_the_openwebui_shape() {
        let result = OpenWebUiSearchResult::from(kagi::SearchResult {
            title: "The Rust Programming Language".to_owned(),
            url: "https://doc.rust-lang.org/book/".to_owned(),
            description: "Learn Rust with an official book.".to_owned(),
        });

        assert_eq!(result.link, "https://doc.rust-lang.org/book/");
        assert_eq!(result.title, "The Rust Programming Language");
        assert_eq!(result.snippet, "Learn Rust with an official book.");
    }

    #[test]
    fn serializes_results_with_the_documented_field_names() {
        let rendered: Value = serde_json::to_value(OpenWebUiSearchResult {
            link: "https://example.com/".to_owned(),
            title: "example".to_owned(),
            snippet: "an example".to_owned(),
        })
        .expect("serializes");

        assert_eq!(
            rendered,
            json!({
                "link": "https://example.com/",
                "title": "example",
                "snippet": "an example",
            })
        );
    }

    #[test]
    fn serializes_no_results_as_an_empty_array() {
        let rendered =
            serde_json::to_value(Vec::<OpenWebUiSearchResult>::new()).expect("serializes");

        assert_eq!(rendered, json!([]));
    }

    #[test]
    fn parses_the_request_body() {
        let request: OpenWebUiSearchRequest =
            serde_json::from_value(json!({"query": "rust programming", "count": 3}))
                .expect("valid body");

        assert_eq!(request.query, "rust programming");
        assert_eq!(request.count(), 3);

        let request: OpenWebUiSearchRequest =
            serde_json::from_value(json!({"query": "rust programming"})).expect("valid body");

        assert_eq!(request.count(), DEFAULT_COUNT);
    }

    #[test]
    fn rejects_malformed_request_bodies() {
        // Unknown fields, a missing query, and non-integer counts all fail deserialization —
        // the handler never sees them.
        for body in [
            json!({"query": "rust", "count": 5, "nope": 1}),
            json!({"count": 5}),
            json!({"query": "rust", "count": "five"}),
            json!({"query": "rust", "count": -1}),
            json!({"query": "rust", "count": 1.5}),
        ] {
            assert!(
                serde_json::from_value::<OpenWebUiSearchRequest>(body.clone()).is_err(),
                "expected {body} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_an_empty_query() {
        let request: OpenWebUiSearchRequest =
            serde_json::from_value(json!({"query": "", "count": 5})).expect("valid body");

        assert!(matches!(request.validate(), Err(ApiError::InvalidParam(_))));
    }

    #[test]
    fn truncates_results_to_the_count() {
        let results: Vec<OpenWebUiSearchResult> = (0..5)
            .map(|index| OpenWebUiSearchResult {
                link: format!("https://example.com/{index}"),
                title: format!("title {index}"),
                snippet: String::new(),
            })
            .collect();

        let mut limited = results;
        limited.truncate(3);

        assert_eq!(limited.len(), 3);
        assert_eq!(limited.last().unwrap().link, "https://example.com/2");
    }
}
