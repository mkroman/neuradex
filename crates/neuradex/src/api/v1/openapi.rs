//! The base OpenAPI document of the API.
//!
//! The paths are not listed here: they are derived from the `#[utoipa::path]` handlers at
//! router-build time through [`utoipa_axum`] (see `crate::api::v1::api`), so the router and
//! the document cannot drift apart. This module only carries the document's metadata — info,
//! tags, and the explicitly registered schemas (`ErrorBody` must stay listed here because
//! manual `IntoResponses` impls are invisible to utoipa's compile-time schema collection).

use utoipa::OpenApi;

use crate::api::v1::error::ErrorBody;
use crate::api::v1::extract::{OpenWebUiSearchRequest, SearchRequest};
use crate::api::v1::fetch::FetchResponse;
use crate::api::v1::openwebui_search::OpenWebUiSearchResult;
use crate::api::v1::peek::PeekResponse;
use crate::api::v1::search::{SearchResponse, SearchResult};
use crate::metadata::PageMetadata;
use crate::metrics::{PageMetrics, RedirectHop, SearchMetrics};

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
        PageMetrics,
        SearchMetrics,
        RedirectHop,
    ))
)]
pub struct BaseDoc;

/// Returns the base document the router-derived paths are collected into.
pub fn base() -> utoipa::openapi::OpenApi {
    BaseDoc::openapi()
}

#[cfg(test)]
mod tests {
    //! The OpenAPI test suite.
    //!
    //! `matches_the_checked_in_spec` is the broad contract check: it compares the whole
    //! generated document against the checked-in fixture, so any wire change fails there.
    //! `passes_the_structural_lints` validates the document's internal references and its
    //! response/tag/component consistency. `documents_the_shared_error_responses` keeps the
    //! cross-endpoint error-envelope invariant, which the fixture cannot express because it is
    //! regenerated on every intentional document change. The remaining focused tests document
    //! intent; they do not repeat the fixture's field-by-field assertions.

    use std::collections::BTreeSet;

    use serde_json::{Value, json};

    /// The expected paths and their methods.
    const EXPECTED_OPERATIONS: &[(&str, &str)] = &[
        ("/healthz", "get"),
        ("/v1/fetch", "get"),
        ("/v1/peek", "get"),
        ("/v1/search", "post"),
        ("/v1/openwebui_search", "post"),
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

        let mut expected: Vec<(&str, &str)> = EXPECTED_OPERATIONS.to_vec();
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

    /// Every non-probe endpoint documents the shared error statuses, each referencing
    /// `ErrorBody`. The golden fixture cannot enforce this across endpoints: regenerating it
    /// would also accept a missing error response.
    #[test]
    fn documents_the_shared_error_responses() {
        let document = document();

        for &(path, method) in EXPECTED_OPERATIONS {
            if path == "/healthz" {
                continue;
            }

            let responses = &operation(&document, path, method).responses;

            for status in ["400", "415", "500", "502"] {
                let response = responses.responses.get(status).unwrap_or_else(|| {
                    panic!("the {status} response is missing from {method} {path}")
                });
                let rendered = serde_json::to_value(response).expect("serializes");

                assert_eq!(
                    rendered["content"]["application/json"]["schema"]["$ref"],
                    "#/components/schemas/ErrorBody",
                    "the {status} response of {method} {path} does not reference ErrorBody"
                );
            }
        }
    }

    /// The golden fixture pins the exact IDs; this keeps the semantic properties the fixture
    /// cannot express: every operation has a URL-safe, unique ID.
    #[test]
    fn operation_ids_are_unique_and_url_safe() {
        let document = document();
        let mut ids = Vec::new();

        for &(path, method) in EXPECTED_OPERATIONS {
            let id = operation(&document, path, method)
                .operation_id
                .as_deref()
                .unwrap_or_else(|| panic!("the operation for {method} {path} has no operationId"));

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
    /// The comparison is semantic: the docs crate enables `serde_json`'s `preserve_order`
    /// feature, so with the `docs` feature the object keys keep their insertion order while
    /// without it they serialize in an arbitrary order. Comparing `Value`s ignores key order,
    /// which keeps the fixture check valid in both feature configurations.
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
