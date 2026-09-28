//! The OpenAPI document of the API, generated from the `#[utoipa::path]` handlers.
//!
//! The document is served as JSON at `/openapi.json` and consumed by the Swagger UI at
//! `/swagger-ui`; both are wired into the router in [`crate::api::v1::router`].

use utoipa::OpenApi;

use crate::api::v1::{
    error::ErrorBody, fetch::FetchResponse, peek::PeekResponse, search::SearchResponse,
    search::SearchResult,
};
use crate::metadata::PageMetadata;
use crate::metrics::{Metrics, RedirectHop, SearchMetrics};

/// The generated OpenAPI documentation of the API.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "neuradex",
        description = "A small API service implementing tools for LLM agents.",
    ),
    paths(
        crate::api::v1::healthz,
        crate::api::v1::fetch::fetch,
        crate::api::v1::peek::peek,
        crate::api::v1::search::search,
    ),
    tags(
        (name = "fetch", description = "Fetching pages over HTTP."),
        (name = "peek", description = "Extracting document head metadata."),
        (name = "search", description = "Searching the web with Kagi."),
        (name = "healthz", description = "Liveness probe."),
    ),
    components(schemas(
        ErrorBody,
        FetchResponse,
        PeekResponse,
        SearchResponse,
        SearchResult,
        PageMetadata,
        Metrics,
        SearchMetrics,
        RedirectHop,
    ))
)]
pub(crate) struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    /// The OpenAPI document generated from the API.
    fn document() -> utoipa::openapi::OpenApi {
        ApiDoc::openapi()
    }

    #[test]
    fn documents_all_endpoints() {
        let document = document();

        for path in ["/healthz", "/v1/fetch", "/v1/peek", "/v1/search"] {
            assert!(
                document.paths.paths.contains_key(path),
                "the document is missing {path}"
            );
        }
    }

    #[test]
    fn documents_the_error_responses_on_every_endpoint() {
        let document = document();

        for path in ["/v1/fetch", "/v1/peek", "/v1/search"] {
            let responses = &document
                .paths
                .paths
                .get(path)
                .and_then(|item| item.get.as_ref())
                .expect("a get operation")
                .responses;

            for status in ["400", "415", "500", "502"] {
                assert!(
                    responses.responses.contains_key(status),
                    "the {status} response is missing from {path}"
                );
            }
        }
    }

    #[test]
    fn registers_the_schemas() {
        let document = document();
        let components = document.components.as_ref().expect("components");

        for schema in [
            "ErrorBody",
            "FetchResponse",
            "PeekResponse",
            "SearchResponse",
        ] {
            assert!(
                components.schemas.contains_key(schema),
                "the components are missing {schema}"
            );
        }
    }

    #[test]
    fn tags_the_operations() {
        let document = document();

        for (path, tag) in [
            ("/healthz", "healthz"),
            ("/v1/fetch", "fetch"),
            ("/v1/peek", "peek"),
            ("/v1/search", "search"),
        ] {
            let tags = document
                .paths
                .paths
                .get(path)
                .and_then(|item| item.get.as_ref())
                .expect("a get operation")
                .tags
                .clone()
                .unwrap_or_default();

            assert_eq!(tags, [tag], "unexpected tags for {path}");
        }
    }

    #[test]
    fn serializes_to_json() {
        let json = document().to_pretty_json().expect("serializes");

        assert!(json.contains("\"openapi\""));
    }
}
