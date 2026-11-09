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
use serde::Serialize;

use crate::api::v1::error::ApiError;
use crate::api::v1::extract::{ApiJson, OpenWebUiSearchRequest};
use crate::api::v1::search;
use crate::state::AppState;

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
/// `DEFAULT_COUNT`). Like [`search`](crate::api::v1::search::search), searches are queued: at
/// most the configured `max_sessions` run at once, and the request blocks here until a slot
/// and the results are ready.
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
                   queued: at most the configured limit run at once, and the request blocks \
                   until a slot and the results are ready.",
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
pub async fn openwebui_search(
    State(state): State<Arc<AppState>>,
    ApiJson(request): ApiJson<OpenWebUiSearchRequest>,
) -> Result<Json<Vec<OpenWebUiSearchResult>>, ApiError> {
    request.validate()?;

    let count = request.count();
    let search::QueuedSearch { results, .. } =
        search::queued_search(&state, &request.query, None).await?;
    let results = results
        .into_iter()
        .take(count)
        .map(OpenWebUiSearchResult::from)
        .collect();

    Ok(Json(results))
}
