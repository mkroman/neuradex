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
    use std::collections::BTreeSet;

    use serde_json::{Value, json};

    /// The expected paths, their methods, operation IDs, query parameters, and tags.
    const EXPECTED_OPERATIONS: &[(&str, &str, &str, &[&str], &str)] = &[
        ("/healthz", "get", "healthz", &[], "healthz"),
        (
            "/v1/fetch",
            "get",
            "fetch",
            &["include", "redirects", "url"],
            "fetch",
        ),
        (
            "/v1/peek",
            "get",
            "peek",
            &["include", "redirects", "url"],
            "peek",
        ),
        ("/v1/search", "post", "search", &[], "search"),
        (
            "/v1/openwebui_search",
            "post",
            "openwebui_search",
            &[],
            "search",
        ),
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
            .map(|(path, method, _, _, _)| (*path, *method))
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
    fn pins_the_operation_ids() {
        let document = document();
        let mut ids = Vec::new();

        for &(path, method, expected, _, _) in EXPECTED_OPERATIONS {
            let id = operation(&document, path, method)
                .operation_id
                .as_deref()
                .unwrap_or_else(|| panic!("the operation for {method} {path} has no operationId"));

            assert_eq!(
                id, expected,
                "the operationId for {method} {path} changed; it is part of the generated \
                 clients' API"
            );
            assert!(
                id.chars()
                    .all(|character| character.is_ascii_alphanumeric()
                        || matches!(character, '_' | '-')),
                "the operationId {id} is not URL-safe"
            );
            ids.push(id);
        }

        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            EXPECTED_OPERATIONS.len(),
            "operationIds must be unique"
        );
    }

    #[test]
    fn documents_the_error_responses_on_every_endpoint() {
        let document = document();

        for &(path, method, _, _, _) in EXPECTED_OPERATIONS {
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

        for &(path, method, _, expected, _) in EXPECTED_OPERATIONS {
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
            "ErrorDetail",
            "ErrorKind",
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

        for &(path, method, _, _, _) in EXPECTED_OPERATIONS {
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

    #[test]
    fn references_the_shared_error_body_from_every_error_response() {
        let document = document();

        for &(path, method, _, _, _) in EXPECTED_OPERATIONS {
            if path == "/healthz" {
                continue;
            }

            let operation = operation(&document, path, method);

            for status in ["400", "415", "500", "502"] {
                let response = &operation.responses.responses[status];
                let rendered = serde_json::to_value(response).expect("serializes");

                assert_eq!(
                    rendered["content"]["application/json"]["schema"]["$ref"],
                    "#/components/schemas/ErrorBody",
                    "the {status} response of {method} {path} does not reference ErrorBody"
                );
            }
        }
    }

    #[test]
    fn documents_the_error_type_values() {
        let document = document();
        let schemas = &document.components.as_ref().expect("components").schemas;
        let detail = serde_json::to_value(&schemas["ErrorDetail"]).expect("serializes");
        assert_eq!(
            detail["properties"]["type"]["$ref"],
            "#/components/schemas/ErrorKind"
        );

        let kind = serde_json::to_value(&schemas["ErrorKind"]).expect("serializes");
        let values: Vec<&str> = kind["enum"]
            .as_array()
            .expect("the error type is documented as an enum")
            .iter()
            .filter_map(Value::as_str)
            .collect();

        assert_eq!(
            values,
            [
                "invalid_param",
                "unsupported_media_type",
                "upstream_error",
                "internal_error",
                "not_found",
                "method_not_allowed",
            ]
        );
    }

    /// Collects every local `$ref` in `value`, checks that it resolves, and records the
    /// referenced component schemas.
    fn collect_refs(document: &Value, value: &Value, schemas: &mut BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                    let pointer = reference.strip_prefix('#').unwrap_or_else(|| {
                        panic!("the single-document spec contains the external $ref {reference}")
                    });

                    assert!(
                        document.pointer(pointer).is_some(),
                        "the $ref {reference} does not resolve"
                    );

                    if let Some(name) = reference.strip_prefix("#/components/schemas/") {
                        schemas.insert(name.to_owned());
                    }
                }

                for child in object.values() {
                    collect_refs(document, child, schemas);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_refs(document, item, schemas);
                }
            }
            _ => {}
        }
    }

    /// The structural subset of the recommended OpenAPI lint rules: declared tags,
    /// resolvable `$ref`s, documented 2xx and error responses, and no unused components.
    #[test]
    fn passes_the_structural_lints() {
        let document = serde_json::to_value(document()).expect("serializes");
        let schemas = document
            .pointer("/components/schemas")
            .and_then(Value::as_object)
            .expect("component schemas");
        let declared_tags: BTreeSet<&str> = document["tags"]
            .as_array()
            .expect("tags")
            .iter()
            .filter_map(|tag| tag["name"].as_str())
            .collect();
        let methods = [
            "get", "post", "put", "patch", "delete", "head", "options", "trace",
        ];
        let mut referenced = BTreeSet::new();

        for (path, item) in document["paths"].as_object().expect("paths") {
            let item = item.as_object().expect("a path item");
            if let Some(parameters) = item.get("parameters") {
                collect_refs(&document, parameters, &mut referenced);
            }

            for (method, operation) in item {
                if !methods.contains(&method.as_str()) {
                    continue;
                }

                let responses = operation["responses"]
                    .as_object()
                    .expect("operation responses");
                assert!(
                    responses.keys().any(|status| status.starts_with('2')),
                    "{method} {path} has no 2xx response"
                );

                for (status, response) in responses {
                    assert!(
                        response["description"]
                            .as_str()
                            .is_some_and(|description| !description.trim().is_empty()),
                        "{method} {path} response {status} has no description"
                    );
                    collect_refs(&document, response, &mut referenced);
                }

                let tags = operation["tags"].as_array().expect("operation tags");
                assert!(!tags.is_empty(), "{method} {path} has no tags");

                for tag in tags {
                    let tag = tag.as_str().expect("a tag name");
                    assert!(
                        declared_tags.contains(tag),
                        "{method} {path} uses the undeclared tag {tag}"
                    );
                }

                for key in ["parameters", "requestBody"] {
                    if let Some(value) = operation.get(key) {
                        collect_refs(&document, value, &mut referenced);
                    }
                }
            }
        }

        // Follow component-to-component references until the set stops growing.
        loop {
            let before = referenced.len();

            for name in referenced.clone() {
                let schema = schemas
                    .get(&name)
                    .unwrap_or_else(|| panic!("operations reference the missing schema {name}"));
                collect_refs(&document, schema, &mut referenced);
            }

            if referenced.len() == before {
                break;
            }
        }

        for name in schemas.keys() {
            assert!(
                referenced.contains(name),
                "the component schema {name} is not referenced from any operation"
            );
        }
    }

    /// The checked-in document is the contract reviewers and client generators see; if
    /// this fails, regenerate it (the diff is the API change under review).
    ///
    /// The comparison is semantic: `serde_json` preserves insertion order only when the
    /// `preserve_order` feature happens to be enabled (the `docs` feature pulls it in),
    /// so the same document can serialize its object keys in a different order.
    #[test]
    fn matches_the_checked_in_spec() {
        let generated = serde_json::to_value(document()).expect("the document serializes");
        let checked_in: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../support/design/openapi-fixture.json"
        )))
        .expect("the checked-in document is valid JSON");

        assert_eq!(
            generated, checked_in,
            "the checked-in OpenAPI document is stale; regenerate it with \
             `cargo run -- export-docs /tmp/docs.html --spec support/design/openapi-fixture.json`"
        );
    }
}
