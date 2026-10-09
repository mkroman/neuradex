//! The base OpenAPI document of the API.
//!
//! The paths are not listed here: they are derived from the `#[utoipa::path]` handlers at
//! router-build time through [`utoipa_axum`] (see [`crate::api::v1::api`]), so the router and
//! the document cannot drift apart. This module only carries the document's metadata — info,
//! tags, and the explicitly registered schemas (`ErrorBody` must stay listed here because
//! manual `IntoResponses` impls are invisible to utoipa's compile-time schema collection).

use utoipa::OpenApi;

use crate::api::v1::{
    error::ErrorBody, fetch::FetchResponse, openwebui_search::OpenWebUiSearchRequest,
    openwebui_search::OpenWebUiSearchResult, peek::PeekResponse, search::SearchResponse,
    search::SearchResult,
};
use crate::metadata::PageMetadata;
use crate::metrics::{Metrics, RedirectHop, SearchMetrics};

/// The metadata of the OpenAPI documentation; the paths are collected by the router.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "neuradex",
        description = "A small API service implementing tools for LLM agents: fetching pages \
                       (`/v1/fetch`, `/v1/peek`), searching the web (`/v1/search`), and a \
                       search endpoint shaped for Open WebUI's `external` web search engine \
                       (`/v1/openwebui_search`). Every error response — including \
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
    /// The OpenAPI document derived from the API's routes.
    fn document() -> utoipa::openapi::OpenApi {
        crate::api::v1::api().1
    }

    /// Looks up the single operation documented for `path`, whatever its method.
    fn operation<'d>(
        document: &'d utoipa::openapi::OpenApi,
        path: &str,
    ) -> &'d utoipa::openapi::path::Operation {
        let item = document.paths.paths.get(path).expect("a documented path");

        item.get
            .as_ref()
            .or(item.post.as_ref())
            .expect("an operation")
    }

    #[test]
    fn documents_all_endpoints() {
        let document = document();

        for path in [
            "/healthz",
            "/v1/fetch",
            "/v1/peek",
            "/v1/search",
            "/v1/openwebui_search",
        ] {
            assert!(
                document.paths.paths.contains_key(path),
                "the document is missing {path}"
            );
        }
    }

    #[test]
    fn documents_the_error_responses_on_every_endpoint() {
        let document = document();

        for path in [
            "/v1/fetch",
            "/v1/peek",
            "/v1/search",
            "/v1/openwebui_search",
        ] {
            let responses = &operation(&document, path).responses;

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
            ("/v1/openwebui_search", &[][..]),
        ] {
            let rendered = serde_json::to_value(operation(&document, path)).expect("serializes");

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
            "OpenWebUiSearchRequest",
            "OpenWebUiSearchResult",
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
            ("/v1/openwebui_search", "search"),
        ] {
            let tags = operation(&document, path).tags.clone().unwrap_or_default();

            assert_eq!(tags, [tag], "unexpected tags for {path}");
        }
    }

    #[test]
    fn serializes_to_json() {
        let json = document().to_pretty_json().expect("serializes");

        assert!(json.contains("\"openapi\""));
    }

    #[test]
    fn summarizes_the_operations() {
        let document = document();

        for path in [
            "/healthz",
            "/v1/fetch",
            "/v1/peek",
            "/v1/search",
            "/v1/openwebui_search",
        ] {
            let operation = operation(&document, path);

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
            // The handlers' `# Errors` sections are for rustdoc, not for API consumers.
            let description = operation.description.as_deref().expect("a description");
            assert!(
                !description.contains("# Errors"),
                "the description for {path} leaks the doc-comment error section"
            );
        }
    }

    #[test]
    fn documents_the_openwebui_search_contract() {
        let document = document();
        let rendered =
            serde_json::to_value(operation(&document, "/v1/openwebui_search")).expect("serializes");

        // The operation is a POST with a JSON request body.
        assert_eq!(rendered["requestBody"]["required"], true);
        let request_schema = &rendered["requestBody"]["content"]["application/json"]["schema"];
        assert_eq!(
            request_schema["$ref"],
            "#/components/schemas/OpenWebUiSearchRequest"
        );

        // The 200 response is a bare array of results — the shape Open WebUI expects.
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
        assert_eq!(
            request["properties"]["count"]["type"],
            serde_json::json!(["integer", "null"])
        );
        assert_eq!(request["properties"]["count"]["default"], 5);
        assert_eq!(request["required"], serde_json::json!(["query"]));
    }

    #[test]
    fn documents_examples_and_defaults_on_the_query_parameters() {
        let document = document();

        let parameters = |path: &str| -> Vec<serde_json::Value> {
            let operation = document
                .paths
                .paths
                .get(path)
                .and_then(|item| item.get.as_ref())
                .expect("a get operation");
            serde_json::to_value(operation).expect("serializes")["parameters"]
                .as_array()
                .expect("parameters")
                .to_owned()
        };

        let find = |parameters: &[serde_json::Value], name: &str| -> serde_json::Value {
            parameters
                .iter()
                .find(|parameter| parameter["name"] == name)
                .unwrap_or_else(|| panic!("the {name} parameter is missing"))
                .to_owned()
        };

        let fetch = parameters("/v1/fetch");
        let redirects = find(&fetch, "redirects");
        assert_eq!(redirects["schema"]["default"], 5, "the effective default");
        assert_eq!(redirects["schema"]["maximum"], 5);
        assert!(find(&fetch, "url")["example"].is_string());
        assert!(find(&fetch, "include")["example"].is_array());

        let search = parameters("/v1/search");
        assert!(find(&search, "query")["example"].is_string());
        assert_eq!(find(&search, "timeout")["schema"]["maximum"], 30);
        assert!(find(&search, "limit")["example"].is_i64());
    }

    #[test]
    fn documents_examples_on_the_schemas() {
        let rendered = serde_json::to_value(document()).expect("the document serializes to json");

        for schema in [
            "ErrorBody",
            "FetchResponse",
            "Metrics",
            "OpenWebUiSearchRequest",
            "OpenWebUiSearchResult",
            "PageMetadata",
            "PeekResponse",
            "RedirectHop",
            "SearchMetrics",
            "SearchResponse",
            "SearchResult",
        ] {
            assert!(
                rendered["components"]["schemas"][schema]["examples"].is_array(),
                "the {schema} schema is missing examples"
            );
        }
    }
}
