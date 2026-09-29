//! Builds the documentation-page model from the serialized OpenAPI document.
//!
//! Everything here walks the document as JSON — the exact bytes `/openapi.json` serves —
//! so the page inherits the document's own key ordering rather than imposing its own.

use std::collections::HashMap;

use serde_json::Value;

use crate::html::{bool_field, field, prose, ref_name, slug, str_field, strip_dot};
use crate::tree::{self, SchemaNode};

/// The tags pinned to the front of the page, in order; other tags follow in document tag
/// order. This is a neuradex presentation choice — the tools read top to bottom.
const TAG_PRIORITY: &[&str] = &["fetch", "peek", "search"];

/// The methods rendered as endpoint sections, in precedence order.
const HTTP_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

/// The documentation page, as data.
///
/// All prose fields (`*_html`) carry pre-rendered HTML — built with [`prose`] or plain
/// escaped text — and are embedded verbatim; every other string field is escaped by the
/// template.
pub struct Page {
    /// The service name; used for `<title>`, the masthead wordmark, and the footer.
    pub title: String,
    /// The service version, when known.
    pub version: Option<String>,
    /// The tagline under the masthead, as HTML.
    pub description_html: String,
    /// The URL the raw OpenAPI document is served from, e.g. `/openapi.json`.
    pub spec_url: String,
    /// The OpenAPI dialect version, e.g. `3.1.0`.
    pub spec_version: String,
    /// The number of documented endpoints.
    pub endpoint_count: usize,
    /// The table of contents groups, in display order.
    pub toc_groups: Vec<TocGroup>,
    /// The schema entries of the table of contents, in display order.
    pub schemas_toc: Vec<TocSchema>,
    /// The endpoint sections, in display order.
    pub endpoints: Vec<Endpoint>,
    /// The schema appendix entries, in display order.
    pub schemas: Vec<SchemaDoc>,
    /// The appendix intro line, as HTML.
    pub appendix_intro_html: String,
}

/// One tagged group of the table of contents.
pub struct TocGroup {
    /// The tag label.
    pub label: String,
    /// The tag description, shown as the label's tooltip.
    pub description: Option<String>,
    /// The group's entries, in display order.
    pub links: Vec<TocLink>,
}

/// One entry of a table-of-contents group.
pub struct TocLink {
    /// The anchor target, without the leading `#`.
    pub href: String,
    /// The zero-padded display number, e.g. `01`.
    pub num: String,
    /// The display label.
    pub label: String,
}

/// One schema entry of the table of contents.
pub struct TocSchema {
    /// The anchor target, without the leading `#`.
    pub href: String,
    /// The schema name.
    pub name: String,
}

/// One endpoint section of the page.
pub struct Endpoint {
    /// The zero-padded display number, e.g. `01`.
    pub num: String,
    /// The section's DOM id, without the leading `#`.
    pub id: String,
    /// The HTTP method, uppercased.
    pub method: String,
    /// The endpoint path.
    pub path: String,
    /// The section heading — the operation summary, or the path when unsummarized.
    pub title: String,
    /// The operation description, as HTML.
    pub description_html: String,
    /// The query parameters, in declaration order.
    pub params: Vec<Param>,
    /// The documented responses, in document order.
    pub responses: Vec<ResponseView>,
    /// The success schema tree, when the operation returns JSON.
    pub schema_tree: Option<SchemaNode>,
}

/// One query parameter of an endpoint.
pub struct Param {
    /// The parameter name.
    pub name: String,
    /// Whether the parameter is required.
    pub required: bool,
    /// The parameter description, as HTML.
    pub description_html: String,
    /// The constraint annotations, e.g. `default: 5`.
    pub anns: Vec<Anno>,
    /// The value the request builder's input is seeded with.
    pub seed: String,
    /// Whether the parameter accepts repeated/comma-separated values.
    pub is_array: bool,
}

/// One constraint annotation.
pub struct Anno {
    /// The annotation key, e.g. `default`.
    pub key: String,
    /// The annotation value, e.g. `5`.
    pub value: String,
}

/// One documented response of an endpoint.
pub struct ResponseView {
    /// The status code, as written in the document.
    pub code: String,
    /// Whether the status is a success/redirect (2xx/3xx).
    pub ok: bool,
    /// The response description, as HTML.
    pub description_html: String,
    /// The names of the schemas the response references.
    pub refs: Vec<String>,
}

/// One schema appendix entry.
pub struct SchemaDoc {
    /// The schema name.
    pub name: String,
    /// The anchor DOM id, without the leading `#`.
    pub anchor_id: String,
    /// The schema tree.
    pub tree: SchemaNode,
}

/// An operation collected from the document, before it becomes an endpoint section.
struct Op<'a> {
    path: &'a str,
    id: String,
    method: String,
    tag: &'a str,
    summary: Option<&'a str>,
    description: Option<&'a str>,
    params: Vec<&'a Value>,
    operation: &'a Value,
    num: usize,
}

/// Builds the page model from the serialized document.
pub fn page(spec: &Value) -> Page {
    let mut ops = collect_ops(spec);

    let doc_tag_order: Vec<&str> = spec
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| str_field(tag, "name"))
                .collect()
        })
        .unwrap_or_default();
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
    ops.sort_by_key(|op| group_index(op.tag));
    for (index, op) in ops.iter_mut().enumerate() {
        op.num = index + 1;
    }

    let tag_description = |tag: &str| -> Option<String> {
        spec.get("tags")
            .and_then(Value::as_array)
            .and_then(|tags| {
                tags.iter().find_map(|documented| {
                    (str_field(documented, "name") == Some(tag))
                        .then(|| str_field(documented, "description"))
                        .flatten()
                })
            })
            .map(str::to_owned)
    };

    let mut toc_groups: Vec<TocGroup> = Vec::new();
    let mut endpoints: Vec<Endpoint> = Vec::new();
    for op in &ops {
        let link = TocLink {
            href: op.id.clone(),
            num: format!("{:02}", op.num),
            label: strip_dot(op.summary.unwrap_or(op.path)).to_owned(),
        };
        let endpoint = endpoint(spec, op);
        match toc_groups.last_mut().filter(|group| group.label == op.tag) {
            Some(group) => group.links.push(link),
            None => {
                toc_groups.push(TocGroup {
                    label: op.tag.to_owned(),
                    description: tag_description(op.tag),
                    links: vec![link],
                });
            }
        }
        endpoints.push(endpoint);
    }

    let schemas: Vec<SchemaDoc> = schemas(spec);
    let errors_schema = if schemas.iter().any(|schema| schema.name == "ErrorBody") {
        "`ErrorBody`"
    } else {
        "the error schema"
    };

    Page {
        title: spec
            .get("info")
            .and_then(|info| str_field(info, "title"))
            .unwrap_or("neuradex")
            .to_owned(),
        version: spec
            .get("info")
            .and_then(|info| str_field(info, "version"))
            .map(str::to_owned),
        description_html: spec
            .get("info")
            .and_then(|info| str_field(info, "description"))
            .map_or_else(String::new, prose),
        // The service serves the raw document from its own root; this is the one
        // neuradex-specific path in the model.
        spec_url: "/openapi.json".to_owned(),
        spec_version: str_field(spec, "openapi").unwrap_or("3.1").to_owned(),
        endpoint_count: endpoints.len(),
        toc_groups,
        schemas_toc: schemas
            .iter()
            .map(|schema| TocSchema {
                href: schema.anchor_id.clone(),
                name: schema.name.clone(),
            })
            .collect(),
        endpoints,
        appendix_intro_html: prose(&format!(
            "Every schema the API references, rendered as collapsible trees — {} in total. \
             Errors resolve to {errors_schema}.",
            schemas.len()
        )),
        schemas,
    }
}

/// Builds the schema appendix: every component schema, in document order, as its own
/// anchored tree.
fn schemas(spec: &Value) -> Vec<SchemaDoc> {
    field(spec, "components")
        .and_then(|components| field(components, "schemas"))
        .and_then(Value::as_object)
        .map(|schemas| {
            schemas
                .iter()
                .map(|(name, schema)| SchemaDoc {
                    name: name.clone(),
                    anchor_id: format!("schema-{name}"),
                    tree: tree::root(spec, name, schema),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Collects every operation from the document, in registration order, with slugged and
/// deduplicated DOM ids.
fn collect_ops(spec: &Value) -> Vec<Op<'_>> {
    let mut ops = Vec::new();
    let mut used_ids: HashMap<String, usize> = HashMap::new();

    let Some(paths) = spec.get("paths").and_then(Value::as_object) else {
        return ops;
    };
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for method in HTTP_METHODS {
            let Some(op) = item.get(*method).filter(|op| op.is_object()) else {
                continue;
            };
            let base = format!("ep-{}", slug(path));
            let seen = used_ids.entry(base.clone()).or_insert(0);
            *seen += 1;
            let id = if *seen == 1 {
                base
            } else {
                format!("{base}-{seen}")
            };
            ops.push(Op {
                path,
                id,
                method: method.to_uppercase(),
                tag: op
                    .get("tags")
                    .and_then(Value::as_array)
                    .and_then(|tags| tags.first())
                    .and_then(Value::as_str)
                    .unwrap_or("other"),
                summary: str_field(op, "summary"),
                description: str_field(op, "description"),
                params: op
                    .get("parameters")
                    .and_then(Value::as_array)
                    .map(|parameters| {
                        parameters
                            .iter()
                            .filter(|p| {
                                str_field(p, "in").is_none_or(|location| location == "query")
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                operation: op,
                num: 0,
            });
        }
    }
    ops
}

/// Builds the endpoint section of one operation.
fn endpoint(spec: &Value, op: &Op<'_>) -> Endpoint {
    Endpoint {
        num: format!("{:02}", op.num),
        id: op.id.clone(),
        method: op.method.clone(),
        path: op.path.to_owned(),
        title: op.summary.unwrap_or(op.path).to_owned(),
        description_html: op.description.map_or_else(String::new, prose),
        params: op.params.iter().map(|p| param(p)).collect(),
        responses: responses(op.operation),
        schema_tree: success_schema(op.operation).map(|schema| {
            let name = str_field(schema, "$ref").map_or_else(
                || "response".to_owned(),
                |reference| ref_name(reference).to_owned(),
            );
            tree::root(spec, &name, schema)
        }),
    }
}

/// Builds one query-parameter row: description, constraint annotations, and the request
/// builder's seed value.
fn param(p: &Value) -> Param {
    let schema = field(p, "schema");
    let schema_type = schema.and_then(|s| str_field(s, "type"));

    let mut anns: Vec<Anno> = Vec::new();
    let mut push = |key: &str, value: String| {
        anns.push(Anno {
            key: key.to_owned(),
            value,
        });
    };
    if schema_type == Some("array") {
        let items_type = schema
            .and_then(|s| field(s, "items"))
            .and_then(|items| str_field(items, "type"));
        push(
            "type",
            items_type.map_or_else(
                || "array".to_owned(),
                |items_type| format!("array<{items_type}>"),
            ),
        );
    } else if let Some(schema_type) = schema_type {
        push("type", (*schema_type).to_owned());
    }
    if let Some(format) = schema.and_then(|s| str_field(s, "format")) {
        push("format", format.to_owned());
    }
    if let Some(default) = schema.and_then(|s| field(s, "default")) {
        push("default", default.to_string());
    }
    if let Some(minimum) = schema.and_then(|s| field(s, "minimum")) {
        push("min", minimum.to_string());
    }
    if let Some(maximum) = schema.and_then(|s| field(s, "maximum")) {
        push("max", maximum.to_string());
    }
    if let Some(example) = field(p, "example") {
        push("example", flat_value(example));
    }
    if schema_type == Some("array") {
        push("send as", "repeated keys".to_owned());
    }
    if p.get("style").is_some() || p.get("explode").is_some() {
        let style = str_field(p, "style").unwrap_or("form");
        let explode = if p.get("explode").and_then(Value::as_bool) == Some(false) {
            " · no explode"
        } else {
            " · explode"
        };
        push("style", format!("{style}{explode}"));
    }

    Param {
        name: str_field(p, "name").unwrap_or_default().to_owned(),
        required: bool_field(p, "required"),
        description_html: prose(str_field(p, "description").unwrap_or_default()),
        anns,
        seed: seed(p),
        is_array: schema_type == Some("array"),
    }
}

/// The value the request builder's input is seeded with: the example when documented, the
/// default otherwise.
fn seed(p: &Value) -> String {
    if let Some(example) = field(p, "example") {
        return flat_value(example);
    }
    match p.get("schema").and_then(|s| field(s, "default")) {
        Some(Value::String(default)) => default.clone(),
        Some(default) => default.to_string(),
        None => String::new(),
    }
}

/// Formats a JSON value for the annotation column: arrays are comma-joined, strings are
/// shown raw, everything else compact JSON.
fn flat_value(value: &Value) -> String {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Builds the documented-response rows of one operation.
fn responses(op: &Value) -> Vec<ResponseView> {
    let Some(responses) = op.get("responses").and_then(Value::as_object) else {
        return Vec::new();
    };
    responses
        .iter()
        .map(|(code, response)| ResponseView {
            code: code.clone(),
            ok: code.starts_with('2') || code.starts_with('3'),
            description_html: prose(str_field(response, "description").unwrap_or_default()),
            refs: find_refs(response),
        })
        .collect()
}

/// The names of the component schemas a response references, in first-mention order.
fn find_refs(response: &Value) -> Vec<String> {
    const NEEDLE: &str = "#/components/schemas/";
    let Ok(json) = serde_json::to_string(&field(response, "content").unwrap_or(&Value::Null))
    else {
        return Vec::new();
    };

    let mut names = Vec::new();
    let mut rest = json.as_str();
    while let Some(start) = rest.find(NEEDLE) {
        let after = &rest[start + NEEDLE.len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            .collect();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
        rest = after;
    }
    names
}

/// The first 2xx response's JSON schema, when the operation documents one.
fn success_schema(op: &Value) -> Option<&Value> {
    let responses = op.get("responses").and_then(Value::as_object)?;
    let (_, response) = responses.iter().find(|(code, _)| code.starts_with('2'))?;
    let content = response.get("content").and_then(Value::as_object)?;
    let media = content
        .get("application/json")
        .or_else(|| content.get("text/json"))?;
    field(media, "schema")
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

    #[test]
    fn orders_endpoints_by_tag_priority_then_document_order() {
        let page = page(&spec());

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
        let page = page(&spec());
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
                .any(|anno| anno.key == "min" && anno.value == "1")
        );
        // Array parameters are annotated as repeated keys.
        assert!(params[1].is_array);
        assert!(
            params[1]
                .anns
                .iter()
                .any(|anno| anno.key == "send as" && anno.value == "repeated keys")
        );
        assert_eq!(params[1].seed, "big");
    }

    #[test]
    fn builds_parameterless_endpoints() {
        let page = page(&spec());

        assert!(page.endpoints[1].params.is_empty());
        assert_eq!(page.endpoint_count, 2);
    }

    #[test]
    fn links_responses_to_the_schema_appendix() {
        let page = page(&spec());
        let responses = &page.endpoints[0].responses;

        assert_eq!(responses.len(), 2);
        assert!(responses[0].ok);
        assert_eq!(responses[0].refs, vec!["Thing"]);
        assert!(!responses[1].ok);
        assert!(responses[1].refs.is_empty());
        // The referenced schema lands in the appendix with its anchor id.
        assert_eq!(page.schemas_toc[0].href, "schema-Thing");
    }
}
