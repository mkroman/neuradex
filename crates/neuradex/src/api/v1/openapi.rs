//! The base OpenAPI document of the API.
//!
//! The paths are not listed here: they are derived from the `#[utoipa::path]` handlers at
//! router-build time through [`utoipa_axum`] (see `crate::api::v1::api`), so the router and
//! the document cannot drift apart. This module only carries the document's metadata — info,
//! tags, and the explicitly registered schemas (`ErrorBody` must stay listed here because
//! manual `IntoResponses` impls are invisible to utoipa's compile-time schema collection).

use utoipa::OpenApi;

use crate::api::v1::error::ErrorBody;
use crate::api::v1::extract::SearchRequest;
use crate::api::v1::fetch::FetchResponse;
use crate::api::v1::openwebui_search::{OpenWebUiSearchRequest, OpenWebUiSearchResult};
use crate::api::v1::peek::PeekResponse;
use crate::api::v1::search::{SearchResponse, SearchResult};
use crate::metadata::PageMetadata;
use crate::metrics::{Metrics, RedirectHop, SearchMetrics};

/// The metadata of the OpenAPI documentation; the paths are collected by the router.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "neuradex",
        description = "A small API service implementing tools for LLM agents: fetching pages \
                       (`/v1/fetch`, `/v1/peek`), searching the web (`POST /v1/search`), and a \
                       search endpoint shaped for Open WebUI's `external` web search engine \
                       (`POST /v1/openwebui_search`). Every error response — including \
                       router-level 404 and 405 — is rendered as \
                       `{\"error\": {\"type\", \"message\"}}`; see the `ErrorBody` schema.",
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
        SearchRequest,
        SearchResponse,
        SearchResult,
        OpenWebUiSearchRequest,
        OpenWebUiSearchResult,
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
    use serde_json::json;

    /// The expected paths, their methods, query parameters, and tags.
    const EXPECTED_OPERATIONS: &[(&str, &str, &[&str], &str)] = &[
        ("/healthz", "get", &[], "healthz"),
        (
            "/v1/fetch",
            "get",
            &["include", "redirects", "url"],
            "fetch",
        ),
        ("/v1/peek", "get", &["include", "redirects", "url"], "peek"),
        ("/v1/search", "post", &[], "search"),
        ("/v1/openwebui_search", "post", &[], "search"),
    ];

    /// The OpenAPI document derived from the API's routes.
    fn document() -> utoipa::openapi::OpenApi {
        crate::api::v1::api().1
    }

    /// Looks up the operation documented for `path` under `method`.
    fn operation<'d>(
        document: &'d utoipa::openapi::OpenApi,
        path: &str,
        method: &str,
    ) -> &'d utoipa::openapi::path::Operation {
        let item = document.paths.paths.get(path).expect("a documented path");

        match method {
            "get" => item.get.as_ref(),
            "post" => item.post.as_ref(),
            _ => panic!("unsupported method in EXPECTED_OPERATIONS: {method}"),
        }
        .expect("an operation")
    }

    #[test]
    fn documents_the_expected_operations() {
        let document = document();
        let mut documented: Vec<(&str, &str)> = Vec::new();

        for (path, item) in &document.paths.paths {
            if item.get.is_some() {
                documented.push((path, "get"));
            }
            if item.post.is_some() {
                documented.push((path, "post"));
            }
        }

        let mut expected: Vec<(&str, &str)> = EXPECTED_OPERATIONS
            .iter()
            .map(|(path, method, _, _)| (*path, *method))
            .collect();
        documented.sort_unstable();
        expected.sort_unstable();

        assert_eq!(documented, expected);

        let search = document
            .paths
            .paths
            .get("/v1/search")
            .expect("the search path");
        assert!(
            search.get.is_none(),
            "GET /v1/search must not be documented"
        );
        assert!(search.post.is_some(), "POST /v1/search must be documented");
    }

    #[test]
    fn documents_the_error_responses_on_every_endpoint() {
        let document = document();

        for &(path, method, _, _) in EXPECTED_OPERATIONS {
            if path == "/healthz" {
                continue;
            }

            let responses = &operation(&document, path, method).responses;

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

        for &(path, method, expected, _) in EXPECTED_OPERATIONS {
            let rendered =
                serde_json::to_value(operation(&document, path, method)).expect("serializes");

            let mut names: Vec<&str> =
                rendered["parameters"]
                    .as_array()
                    .map_or_default(|parameters| {
                        parameters
                            .iter()
                            .filter_map(|parameter| parameter["name"].as_str())
                            .collect()
                    });
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
            "OpenWebUiSearchRequest",
            "OpenWebUiSearchResult",
            "PeekResponse",
            "SearchRequest",
            "SearchResponse",
            "SearchResult",
        ] {
            assert!(
                components.schemas.contains_key(schema),
                "the components are missing {schema}"
            );
        }
    }

    #[test]
    fn summarizes_the_operations() {
        let document = document();

        for &(path, method, _, _) in EXPECTED_OPERATIONS {
            let operation = operation(&document, path, method);

            assert!(
                operation
                    .summary
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty()),
                "the operation for {path} is missing a summary"
            );
            assert!(
                operation
                    .description
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty()),
                "the operation for {path} is missing a description"
            );
        }
    }

    #[test]
    fn documents_the_openwebui_search_contract() {
        let document = document();
        let rendered = serde_json::to_value(operation(&document, "/v1/openwebui_search", "post"))
            .expect("serializes");

        // The operation is a POST with a JSON request body.
        assert_eq!(rendered["requestBody"]["required"], true);
        let request_schema = &rendered["requestBody"]["content"]["application/json"]["schema"];
        assert_eq!(
            request_schema["$ref"],
            "#/components/schemas/OpenWebUiSearchRequest"
        );

        // The 200 response is a bare array of results — the shape Open WebUI expects.
        let response_schema =
            &rendered["responses"]["200"]["content"]["application/json"]["schema"];
        assert_eq!(response_schema["type"], "array");
        assert_eq!(
            response_schema["items"]["$ref"],
            "#/components/schemas/OpenWebUiSearchResult"
        );

        // The request schema: `query` is required, `count` defaults to 5.
        let schemas = &document.components.as_ref().expect("components").schemas;
        let request = serde_json::to_value(&schemas["OpenWebUiSearchRequest"]).expect("serializes");

        assert_eq!(request["properties"]["query"]["type"], "string");
        assert_eq!(request["properties"]["count"]["default"], 5);
        assert_eq!(request["required"], json!(["query"]));
    }

    #[test]
    fn documents_the_search_request_body() {
        let document = document();
        let rendered =
            serde_json::to_value(operation(&document, "/v1/search", "post")).expect("serializes");

        assert_eq!(rendered["requestBody"]["required"], true);
        assert_eq!(
            rendered["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/SearchRequest"
        );

        let schemas = &document.components.as_ref().expect("components").schemas;
        let request = serde_json::to_value(&schemas["SearchRequest"]).expect("serializes");

        for field in ["query", "limit", "timeout"] {
            assert!(
                request["properties"][field].is_object(),
                "the SearchRequest schema is missing {field}"
            );
        }

        let required = request["required"].as_array().expect("required fields");
        assert_eq!(required.len(), 1);
        assert!(required.contains(&json!("query")));

        assert_eq!(request["properties"]["timeout"]["minimum"], 1);
        assert_eq!(request["properties"]["timeout"]["maximum"], 30);
    }
}
