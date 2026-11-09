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
use crate::api::v1::search;

/// The number of results returned when the request carries no `count`.
pub(crate) const DEFAULT_COUNT: usize = 5;

/// The request body of the Open WebUI external search endpoint, as it arrives on the wire.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(examples(json!({"query": "rust programming", "count": 5})))]
#[serde(deny_unknown_fields)]
pub struct OpenWebUiSearchRequest {
    /// The user's search query string.
    #[schema(example = "rust programming")]
    pub query: String,
    /// The suggested maximum number of results; defaults to `DEFAULT_COUNT` when omitted.
    #[schema(default = 5, example = 5)]
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
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(examples(json!({
    "link": "https://doc.rust-lang.org/book/",
    "title": "The Rust Programming Language",
    "snippet": "Learn Rust with an official book."
})))]
pub struct OpenWebUiSearchResult {
    /// The direct URL to the search result.
    #[schema(format = "uri")]
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
#[utoipa::path(
    post,
    path = "/v1/openwebui_search",
    operation_id = "openwebui_search",
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
)]
pub(crate) async fn openwebui_search(
    State(state): State<Arc<AppState>>,
    ApiJson(request): ApiJson<OpenWebUiSearchRequest>,
) -> Result<Json<Vec<OpenWebUiSearchResult>>, ApiError> {
    request.validate()?;

    let count = request.count();
    let (_, results) = search::run_queued_search(&state, &request.query, None).await?;
    let results = results
        .into_iter()
        .take(count)
        .map(OpenWebUiSearchResult::from)
        .collect();

    Ok(Json(results))
}
