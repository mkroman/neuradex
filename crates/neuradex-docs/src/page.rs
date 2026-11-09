//! Builds the documentation-page model from the serialized OpenAPI document.
//!
//! Everything here walks the document as JSON — the exact bytes `/openapi.json` serves —
//! so the page inherits the document's own key ordering rather than imposing its own.

use std::collections::HashMap;

use serde_json::Value;

use crate::highlight;
use crate::html;
use crate::model::{
    Annotation, BodyView, Endpoint, Page, Param, ResponseView, SchemaDoc, TocGroup, TocLink,
};
use crate::schema;
use crate::tree;

/// The tags pinned to the front of the page, in order; other tags follow in document tag
/// order. This is a neuradex presentation choice — the tools read top to bottom.
const TAG_PRIORITY: &[&str] = &["fetch", "peek", "search"];

/// The methods rendered as endpoint sections, in precedence order.
const HTTP_METHODS: &[(&str, &str)] = &[
    ("get", "GET"),
    ("post", "POST"),
    ("put", "PUT"),
    ("patch", "PATCH"),
    ("delete", "DELETE"),
    ("head", "HEAD"),
    ("options", "OPTIONS"),
    ("trace", "TRACE"),
];

/// An operation collected from the document, before it becomes an endpoint section.
struct Operation<'a> {
    path: &'a str,
    id: String,
    method: &'static str,
    tag: &'a str,
    summary: Option<&'a str>,
    description: Option<&'a str>,
    params: Vec<&'a Value>,
    document: &'a Value,
}

/// Builds the page model from the serialized document.
pub fn build_page(spec: &Value) -> Page {
    let mut operations = collect_operations(spec);

    let doc_tag_order: Vec<&str> =
        spec.get("tags")
            .and_then(Value::as_array)
            .map_or_default(|tags| {
                tags.iter()
                    .filter_map(|tag| html::str_field(tag, "name"))
                    .collect()
            });
    let group_index = |tag: &str| -> usize {
        TAG_PRIORITY
            .iter()
            .position(|prioritized| *prioritized == tag)
            .unwrap_or_else(|| {
                TAG_PRIORITY.len()
                    + doc_tag_order
                        .iter()
                        .position(|documented| *documented == tag)
                        .unwrap_or(999)
            })
    };
    operations.sort_by_key(|operation| group_index(operation.tag));

    let tag_description = |tag: &str| -> Option<String> {
        spec.get("tags")
            .and_then(Value::as_array)
            .and_then(|tags| {
                tags.iter().find_map(|documented| {
                    (html::str_field(documented, "name") == Some(tag))
                        .then(|| html::str_field(documented, "description"))
                        .flatten()
                })
            })
            .map(str::to_owned)
    };

    let mut toc_groups: Vec<TocGroup> = Vec::new();
    let mut endpoints: Vec<Endpoint> = Vec::new();
    for (index, operation) in operations.iter().enumerate() {
        let num = format!("{:02}", index + 1);
        let title = operation.summary.unwrap_or(operation.path);
        let link = TocLink {
            href: operation.id.clone(),
            num: num.clone(),
            label: title.strip_suffix('.').unwrap_or(title).to_owned(),
        };
        let endpoint = build_endpoint(spec, operation, &num);
        match toc_groups
            .last_mut()
            .filter(|group| group.label == operation.tag)
        {
            Some(group) => group.links.push(link),
            None => {
                toc_groups.push(TocGroup {
                    label: operation.tag.to_owned(),
                    description: tag_description(operation.tag),
                    links: vec![link],
                });
            }
        }
        endpoints.push(endpoint);
    }

    let schemas = build_schemas(spec);
    let errors_schema = if schemas.iter().any(|schema| schema.name == "ErrorBody") {
        "`ErrorBody`"
    } else {
        "the error schema"
    };

    Page {
        title: spec
            .get("info")
            .and_then(|info| html::str_field(info, "title"))
            .unwrap_or("neuradex")
            .to_owned(),
        version: spec
            .get("info")
            .and_then(|info| html::str_field(info, "version"))
            .map(str::to_owned),
        description_html: spec
            .get("info")
            .and_then(|info| html::str_field(info, "description"))
            .map_or_else(String::new, html::prose),
        // The service serves the raw document from its own root; this is the one
        // neuradex-specific path in the model.
        spec_url: "/openapi.json".to_owned(),
        spec_version: html::str_field(spec, "openapi").unwrap_or("3.1").to_owned(),
        endpoint_count: endpoints.len(),
        toc_groups,
        endpoints,
        appendix_intro_html: html::prose(&format!(
            "Every schema the API references, rendered as collapsible trees — {} in total. \
             Errors resolve to {errors_schema}.",
            schemas.len()
        )),
        schemas,
    }
}

/// Builds the schema appendix: every component schema, in document order, as its own
/// anchored tree.
fn build_schemas(spec: &Value) -> Vec<SchemaDoc> {
    html::field(spec, "components")
        .and_then(|components| html::field(components, "schemas"))
        .and_then(Value::as_object)
        .map_or_default(|schemas| {
            schemas
                .iter()
                .map(|(name, schema)| SchemaDoc {
                    name: name.clone(),
                    anchor_id: format!("schema-{name}"),
                    tree: tree::root(spec, name, schema),
                })
                .collect()
        })
}

/// Collects every operation from the document, in registration order, with slugged and
/// deduplicated DOM ids.
fn collect_operations(spec: &Value) -> Vec<Operation<'_>> {
    let mut operations = Vec::new();
    let mut used_ids: HashMap<String, usize> = HashMap::new();

    let Some(paths) = spec.get("paths").and_then(Value::as_object) else {
        return operations;
    };
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for &(method, method_label) in HTTP_METHODS {
            let Some(operation) = item.get(method).filter(|operation| operation.is_object()) else {
                continue;
            };
            let base = format!("ep-{}", html::slug(path));
            let seen = used_ids.entry(base.clone()).or_insert(0);
            *seen += 1;
            let id = if *seen == 1 {
                base
            } else {
                format!("{base}-{seen}")
            };
            operations.push(Operation {
                path,
                id,
                method: method_label,
                tag: operation
                    .get("tags")
                    .and_then(Value::as_array)
                    .and_then(|tags| tags.first())
                    .and_then(Value::as_str)
                    .unwrap_or("other"),
                summary: html::str_field(operation, "summary"),
                description: html::str_field(operation, "description"),
                params: operation
                    .get("parameters")
                    .and_then(Value::as_array)
                    .map_or_default(|parameters| {
                        parameters
                            .iter()
                            .filter(|parameter| {
                                html::str_field(parameter, "in")
                                    .is_none_or(|location| location == "query")
                            })
                            .collect::<Vec<_>>()
                    }),
                document: operation,
            });
        }
    }
    operations
}

/// Builds the endpoint section of one operation.
fn build_endpoint(spec: &Value, operation: &Operation<'_>, num: &str) -> Endpoint {
    Endpoint {
        num: num.to_owned(),
        id: operation.id.clone(),
        method: operation.method.to_owned(),
        path: operation.path.to_owned(),
        title: operation.summary.unwrap_or(operation.path).to_owned(),
        description_html: operation.description.map_or_else(String::new, html::prose),
        params: operation
            .params
            .iter()
            .map(|parameter| build_param(parameter))
            .collect(),
        body: build_request_body(spec, operation.document),
        responses: build_responses(operation.document),
        schema_tree: find_success_schema(operation.document).map(|schema| {
            let name = html::str_field(schema, "$ref").map_or_else(
                || "response".to_owned(),
                |reference| html::ref_name(reference).to_owned(),
            );
            tree::root(spec, &name, schema)
        }),
    }
}

/// Builds one query-parameter row: description, constraint annotations, and the request
/// builder's seed value.
fn build_param(parameter: &Value) -> Param {
    let schema = html::field(parameter, "schema");
    let schema_type = schema.and_then(schema::primary_type);
    let example = html::field(parameter, "example");

    let mut anns = Vec::new();
    schema::push_type_annotation(&mut anns, schema, schema_type);
    if let Some(schema) = schema {
        schema::push_common_annotations(&mut anns, schema);
    }
    schema::push_example_annotation(&mut anns, example, Some(highlight::EXAMPLE_CAP));
    if schema_type == Some("array") {
        anns.push(Annotation::new("send as", "repeated keys".to_owned()));
    }
    if parameter.get("style").is_some() || parameter.get("explode").is_some() {
        let style = html::str_field(parameter, "style").unwrap_or("form");
        let explode = if parameter.get("explode").and_then(Value::as_bool) == Some(false) {
            " · no explode"
        } else {
            " · explode"
        };
        anns.push(Annotation::new("style", format!("{style}{explode}")));
    }

    Param {
        name: html::str_field(parameter, "name")
            .unwrap_or_default()
            .to_owned(),
        required: html::field(parameter, "required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        description_html: html::prose(
            html::str_field(parameter, "description").unwrap_or_default(),
        ),
        anns,
        seed: builder_seed(example, schema),
        is_array: schema_type == Some("array"),
        schema_type: schema_type.map(str::to_owned),
    }
}

/// Builds the request-body view of an operation, resolving its JSON schema reference.
fn build_request_body(spec: &Value, operation: &Value) -> Option<BodyView> {
    let body = html::field(operation, "requestBody")?;
    let content = html::field(body, "content")?.as_object()?;
    let (media_type, media) = content
        .iter()
        .find(|(media_type, _)| media_type.as_str() == "application/json")
        .or_else(|| content.iter().next())?;
    let schema = resolve_schema(spec, html::field(media, "schema")?);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map_or_default(|required| required.iter().filter_map(Value::as_str).collect());

    let fields = html::field(schema, "properties")
        .and_then(Value::as_object)
        .map_or_default(|properties| {
            properties
                .iter()
                .map(|(name, property)| {
                    build_body_param(spec, name, property, required.contains(&name.as_str()))
                })
                .collect()
        });

    Some(BodyView {
        media_type: media_type.clone(),
        description_html: html::prose(html::str_field(body, "description").unwrap_or_default()),
        fields,
    })
}

/// Builds one request-body field row from its schema property.
fn build_body_param(spec: &Value, name: &str, property: &Value, required: bool) -> Param {
    let schema = resolve_schema(spec, property);
    let schema_type = schema::primary_type(schema);
    let example = schema::body_example(schema);

    let mut anns = Vec::new();
    schema::push_type_annotation(&mut anns, Some(schema), schema_type);
    schema::push_common_annotations(&mut anns, schema);
    schema::push_example_annotation(&mut anns, example, Some(highlight::EXAMPLE_CAP));

    Param {
        name: name.to_owned(),
        required,
        description_html: html::prose(html::str_field(schema, "description").unwrap_or_default()),
        anns,
        seed: builder_seed(example, Some(schema)),
        is_array: schema_type == Some("array"),
        schema_type: schema_type.map(str::to_owned),
    }
}

/// Resolves a local component `$ref` to its schema; other schemas are returned as-is.
fn resolve_schema<'a>(spec: &'a Value, schema: &'a Value) -> &'a Value {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix("#/components/schemas/"))
        .and_then(|name| {
            spec.get("components")
                .and_then(|components| components.get("schemas"))
                .and_then(|schemas| schemas.get(name))
        })
        .unwrap_or(schema)
}

/// The value the request builder's input is seeded with: the example when documented, the
/// default otherwise.
fn builder_seed(example: Option<&Value>, schema: Option<&Value>) -> String {
    if let Some(example) = example {
        return schema::flat_value(example);
    }
    match schema.and_then(|schema| html::field(schema, "default")) {
        Some(Value::String(default)) => default.clone(),
        Some(default) => default.to_string(),
        None => String::new(),
    }
}

/// Builds the documented-response rows of one operation.
fn build_responses(operation: &Value) -> Vec<ResponseView> {
    let Some(responses) = operation.get("responses").and_then(Value::as_object) else {
        return Vec::new();
    };
    responses
        .iter()
        .map(|(code, response)| ResponseView {
            code: code.clone(),
            ok: code.starts_with('2') || code.starts_with('3'),
            description_html: html::prose(
                html::str_field(response, "description").unwrap_or_default(),
            ),
            refs: collect_schema_refs(response),
        })
        .collect()
}

/// The names of the component schemas a response references, in first-mention order.
///
/// Walks the response's `content` value structurally, collecting the names of every
/// local `#/components/schemas/` `$ref` — so pointer-shaped text inside prose or
/// examples never becomes a link.
fn collect_schema_refs(response: &Value) -> Vec<String> {
    fn walk(value: &Value, names: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(reference) = map
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|reference| reference.strip_prefix("#/components/schemas/"))
                    && !names.iter().any(|seen| seen == reference)
                {
                    names.push(reference.to_owned());
                }
                for child in map.values() {
                    walk(child, names);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, names);
                }
            }
            _ => {}
        }
    }

    let mut names = Vec::new();
    walk(
        html::field(response, "content").unwrap_or(&Value::Null),
        &mut names,
    );
    names
}

/// The first 2xx response's JSON schema, when the operation documents one.
fn find_success_schema(operation: &Value) -> Option<&Value> {
    let responses = operation.get("responses").and_then(Value::as_object)?;
    let (_, response) = responses.iter().find(|(code, _)| code.starts_with('2'))?;
    let content = response.get("content").and_then(Value::as_object)?;
    let media = content
        .get("application/json")
        .or_else(|| content.get("text/json"))?;
    html::field(media, "schema")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A synthetic document with the same shapes the service produces: tagged operations,
    /// documented query parameters, and a referenced response schema.
    fn spec() -> Value {
        json!({
            "openapi": "3.1.0",
            "info": {"title": "test", "version": "1.2.3"},
            "tags": [
                {"name": "things", "description": "Listing things."},
                {"name": "other", "description": "Other things."}
            ],
            "paths": {
                "/ping": {"get": {
                    "tags": ["other"],
                    "summary": "Ping the service.",
                    "responses": {"204": {"description": "Pong."}}
                }},
                "/things": {"get": {
                    "tags": ["things"],
                    "summary": "List things.",
                    "parameters": [
                        {"name": "limit", "in": "query", "description": "Max items.",
                         "schema": {"type": "integer", "format": "int64", "minimum": 1},
                         "example": 10},
                        {"name": "kind", "in": "query", "description": "Filter by kind.",
                         "style": "form", "explode": true,
                         "schema": {"type": "array", "items": {"type": "string"}},
                         "example": ["big"]}
                    ],
                    "responses": {
                        "200": {"description": "The things.",
                                "content": {"application/json": {"schema":
                                    {"$ref": "#/components/schemas/Thing"}}}},
                        "404": {"description": "No things."}
                    }
                }}
            },
            "components": {"schemas": {
                "Thing": {"type": "object", "description": "A thing.",
                          "properties": {"name": {"type": "string"}}}
            }}
        })
    }

    /// A multibyte example longer than [`highlight::EXAMPLE_CAP`] whose cut lands inside
    /// a character, so truncation must back off to a boundary.
    fn long_example() -> String {
        "é<\"&ab".repeat(3_000)
    }

    #[test]
    fn orders_endpoints_by_tag_priority_then_document_order() {
        let page = build_page(&spec());

        assert_eq!(page.endpoints[0].id, "ep-things");
        assert_eq!(page.endpoints[0].num, "01");
        assert_eq!(page.endpoints[1].id, "ep-ping");
        assert_eq!(page.endpoints[1].num, "02");
        // `/things` (tag `things`) comes before `/ping` (tag `other`) even though
        // `/ping` is registered first.
        assert_eq!(page.toc_groups[0].label, "things");
        assert_eq!(
            page.toc_groups[0].description.as_deref(),
            Some("Listing things.")
        );
        assert_eq!(page.toc_groups[1].label, "other");
    }

    #[test]
    fn builds_parameter_rows_and_seeds() {
        let page = build_page(&spec());
        let params = &page.endpoints[0].params;

        assert_eq!(params.len(), 2);
        assert!(!params[0].required);
        assert_eq!(params[0].name, "limit");
        assert_eq!(params[0].seed, "10");
        assert!(!params[0].is_array);
        assert!(
            params[0]
                .anns
                .iter()
                .any(|annotation| annotation.key == "min" && annotation.value == "1")
        );
        // Array parameters are annotated as repeated keys.
        assert!(params[1].is_array);
        assert!(
            params[1].anns.iter().any(
                |annotation| annotation.key == "send as" && annotation.value == "repeated keys"
            )
        );
        assert_eq!(params[1].seed, "big");
    }

    #[test]
    fn truncates_long_parameter_example_annotation_but_not_seed() {
        let example = long_example();
        let spec = json!({
            "openapi": "3.1.0",
            "info": {"title": "test", "version": "1.2.3"},
            "paths": {"/things": {"get": {
                "tags": ["things"],
                "summary": "List things.",
                "parameters": [{
                    "name": "q",
                    "in": "query",
                    "schema": {"type": "string"},
                    "example": example
                }],
                "responses": {"200": {"description": "The things."}}
            }}}
        });
        let page = build_page(&spec);
        let param = &page.endpoints[0].params[0];
        let annotation = param
            .anns
            .iter()
            .find(|annotation| annotation.key == "example")
            .expect("an example annotation");

        assert!(annotation.value.contains("truncated for display"));
        // Only whole repeats fit under the cap: the cut backed off from the incomplete
        // `é` at byte 20,000 to the previous character boundary.
        assert_eq!(
            annotation.value.matches('é').count(),
            highlight::EXAMPLE_CAP / "é<\"&ab".len()
        );
        // The request builder's input keeps the full example.
        assert_eq!(param.seed, example);
        assert!(param.seed.len() > highlight::EXAMPLE_CAP);
        assert!(!param.seed.contains("truncated for display"));
    }

    #[test]
    fn builds_parameterless_endpoints() {
        let page = build_page(&spec());

        assert!(page.endpoints[1].params.is_empty());
        assert!(page.endpoints[1].body.is_none());
        assert_eq!(page.endpoint_count, 2);
    }

    /// A document whose operation carries a referenced JSON request body.
    fn body_spec() -> Value {
        json!({
            "openapi": "3.1.0",
            "info": {"title": "test", "version": "1.2.3"},
            "tags": [{"name": "search", "description": "Searching things."}],
            "paths": {
                "/search": {"post": {
                    "tags": ["search"],
                    "summary": "Search things.",
                    "requestBody": {
                        "required": true,
                        "description": "The search request.",
                        "content": {"application/json": {"schema":
                            {"$ref": "#/components/schemas/SearchRequest"}}}
                    },
                    "responses": {"200": {"description": "The results."}}
                }}
            },
            "components": {"schemas": {
                "SearchRequest": {
                    "type": "object",
                    "description": "A search request.",
                    "required": ["query"],
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The query.",
                            "example": "rust"
                        },
                        "limit": {
                            "type": ["integer", "null"],
                            "description": "Max results.",
                            "default": 10
                        },
                        "timeout": {
                            "type": ["integer", "null"],
                            "description": "Timeout.",
                            "minimum": 1,
                            "maximum": 30
                        }
                    }
                }
            }}
        })
    }

    #[test]
    fn builds_request_body_fields() {
        let page = build_page(&body_spec());
        let body = page.endpoints[0].body.as_ref().expect("a request body");

        assert_eq!(body.media_type, "application/json");
        assert_eq!(body.description_html, "The search request.");
        assert_eq!(body.fields.len(), 3);

        assert_eq!(body.fields[0].name, "query");
        assert!(body.fields[0].required);
        assert_eq!(body.fields[0].schema_type.as_deref(), Some("string"));
        assert_eq!(body.fields[0].seed, "rust");
        assert!(
            body.fields[0]
                .anns
                .iter()
                .any(|annotation| annotation.key == "example" && annotation.value == "rust")
        );

        assert_eq!(body.fields[1].name, "limit");
        assert!(!body.fields[1].required);
        assert_eq!(body.fields[1].schema_type.as_deref(), Some("integer"));
        assert_eq!(body.fields[1].seed, "10");

        assert_eq!(body.fields[2].name, "timeout");
        assert!(
            body.fields[2]
                .anns
                .iter()
                .any(|annotation| annotation.key == "max" && annotation.value == "30")
        );
    }

    #[test]
    fn truncates_long_request_body_example_annotation_but_not_seed() {
        let example = long_example();
        let spec = json!({
            "openapi": "3.1.0",
            "info": {"title": "test", "version": "1.2.3"},
            "paths": {"/search": {"post": {
                "tags": ["search"],
                "summary": "Search things.",
                "requestBody": {
                    "content": {"application/json": {"schema":
                        {"$ref": "#/components/schemas/SearchRequest"}}}
                },
                "responses": {"200": {"description": "The results."}}
            }}},
            "components": {"schemas": {
                "SearchRequest": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "example": example}
                    }
                }
            }}
        });
        let page = build_page(&spec);
        let body = page.endpoints[0].body.as_ref().expect("a request body");
        let field = &body.fields[0];
        let annotation = field
            .anns
            .iter()
            .find(|annotation| annotation.key == "example")
            .expect("an example annotation");

        assert!(annotation.value.contains("truncated for display"));
        // Only whole repeats fit under the cap: the cut backed off from the incomplete
        // `é` at byte 20,000 to the previous character boundary.
        assert_eq!(
            annotation.value.matches('é').count(),
            highlight::EXAMPLE_CAP / "é<\"&ab".len()
        );
        // The request builder's input keeps the full example.
        assert_eq!(field.seed, example);
        assert!(field.seed.len() > highlight::EXAMPLE_CAP);
        assert!(!field.seed.contains("truncated for display"));
    }

    #[test]
    fn links_responses_to_the_schema_appendix() {
        let page = build_page(&spec());
        let responses = &page.endpoints[0].responses;

        assert_eq!(responses.len(), 2);
        assert!(responses[0].ok);
        assert_eq!(responses[0].refs, vec!["Thing"]);
        assert!(!responses[1].ok);
        assert_eq!(responses[1].refs, Vec::<String>::new());
        // The referenced schema lands in the appendix with its anchor id.
        assert_eq!(page.schemas[0].anchor_id, "schema-Thing");
    }

    #[test]
    fn collects_refs_structurally_not_by_text() {
        let response = json!({
            "content": {"application/json": {"schema": {
                "allOf": [
                    {"$ref": "#/components/schemas/Alpha"},
                    {"type": "array", "items": {"$ref": "#/components/schemas/Beta"}},
                    {"properties": {"first": {"$ref": "#/components/schemas/Alpha"}}},
                    {"not": "a description about #/components/schemas/Gamma"}
                ]
            }}}
        });

        // Nested `$ref`s are collected in first-mention order; pointer-shaped text in
        // a string value is not a reference.
        assert_eq!(collect_schema_refs(&response), vec!["Alpha", "Beta"]);

        let dedup = json!({
            "content": {"application/json": {"schema": {
                "allOf": [
                    {"$ref": "#/components/schemas/Alpha"},
                    {"$ref": "#/components/schemas/Alpha"}
                ]
            }}}
        });
        assert_eq!(collect_schema_refs(&dedup), vec!["Alpha"]);
    }
}
