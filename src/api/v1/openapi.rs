//! The base OpenAPI document of the API.
//!
//! The paths are not listed here: they are derived from the `#[utoipa::path]` handlers at
//! router-build time through [`utoipa_axum`] (see [`crate::api::v1::api`]), so the router and
//! the document cannot drift apart. This module only carries the document's metadata — info,
//! tags, and the explicitly registered schemas (`ErrorBody` must stay listed here because
//! manual `IntoResponses` impls are invisible to utoipa's compile-time schema collection).

use utoipa::OpenApi;

use crate::api::v1::{
    error::ErrorBody, fetch::FetchResponse, peek::PeekResponse, search::SearchResponse,
    search::SearchResult,
};
use crate::metadata::PageMetadata;
use crate::metrics::{Metrics, RedirectHop, SearchMetrics};

/// The metadata of the OpenAPI documentation; the paths are collected by the router.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "neuradex",
        description = "A small API service implementing tools for LLM agents.",
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
pub(crate) struct BaseDoc;

/// Returns the base document the router-derived paths are collected into.
pub(crate) fn base() -> utoipa::openapi::OpenApi {
    BaseDoc::openapi()
}

#[cfg(test)]
mod tests {
    /// The OpenAPI document derived from the API's routes.
    fn document() -> utoipa::openapi::OpenApi {
        crate::api::v1::api().1
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
    fn documents_the_query_parameters() {
        let document = document();

        for (path, expected) in [
            ("/healthz", &[][..]),
            ("/v1/fetch", &["include", "redirects", "url"][..]),
            ("/v1/peek", &["include", "redirects", "url"][..]),
            ("/v1/search", &["limit", "query", "timeout"][..]),
        ] {
            let operation = document
                .paths
                .paths
                .get(path)
                .and_then(|item| item.get.as_ref())
                .expect("a get operation");
            let rendered = serde_json::to_value(operation).expect("serializes");

            let mut names: Vec<&str> = rendered["parameters"]
                .as_array()
                .map(|parameters| {
                    parameters
                        .iter()
                        .filter_map(|parameter| parameter["name"].as_str())
                        .collect()
                })
                .unwrap_or_default();
            names.sort_unstable();

            assert_eq!(names, expected, "unexpected query parameters for {path}");
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
