//! Builds OpenAPI schemas as the page's collapsible `<details>` trees.
//!
//! The trees recurse over arbitrary schema shapes, so they render through their own
//! askama partials (`templates/partials/schema-node.html` and `schema-row.html`).
//! Plain-text fields (names, labels, annotations) are escaped by the template; the
//! pre-rendered HTML fields (`*_html`, the highlighted examples from [`crate::highlight`],
//! and the recursive children) are interpolated with `|safe`. The markup and class names
//! are the ones the theme's stylesheet and behavior script expect.

use serde_json::Value;

use askama::Template;

use crate::highlight;
use crate::html;
use crate::model::Annotation;
use crate::schema;

/// The maximum nesting depth expanded inline; deeper branches point at the schema appendix.
const MAX_DEPTH: usize = 4;

/// One `<details>` node of a schema tree; see `templates/partials/schema-node.html`.
pub struct SchemaNode {
    /// Whether the node is expanded by default (only the root is).
    pub open: bool,
    /// The node label.
    pub name: String,
    /// The type label for the type pill, e.g. `object` or a `$ref` target name.
    pub type_label: String,
    /// Whether the type pill marks a `$ref` to another schema.
    pub type_is_ref: bool,
    /// Whether the node is a required property of its parent.
    pub required: bool,
    /// The first line of the target's description, backticks stripped.
    pub brief: Option<String>,
    /// What the node's body resolves to.
    pub body: SchemaBody,
}

/// The body of a schema node: one of the three truncated render modes, or the full
/// resolved body.
pub enum SchemaBody {
    /// The `$ref` could not be resolved; the payload is the reference or node label.
    Unresolved(String),
    /// The `$ref` resolves to a node already on the stack; the payload is the target
    /// name.
    Cycle(String),
    /// The depth guard cut the expansion; the appendix has the rest.
    Deep,
    /// The fully expanded body.
    Resolved(ResolvedBody),
}

/// The resolved body of a schema node; the `*_html` fields are pre-rendered HTML.
pub struct ResolvedBody {
    /// The description, as HTML.
    pub description_html: String,
    /// The constraint annotations, in document order.
    pub anns: Vec<Annotation>,
    /// The properties, in document order.
    pub props: Vec<SchemaEntry>,
    /// The element schema of an array, if any.
    pub items: Option<Box<SchemaNode>>,
    /// `example` or `examples`, by count.
    pub examples_label: String,
    /// The examples, as highlighted HTML for a `<code>` element each.
    pub examples: Vec<String>,
}

/// One child of a resolved body: either an expandable node or a flat row.
pub enum SchemaEntry {
    /// An expandable branch, rendered by `schema-node.html`.
    Node(SchemaNode),
    /// A flat property row, rendered by `schema-row.html`.
    Row(SchemaRow),
}

/// A flat (non-expandable) property row; see `templates/partials/schema-row.html`.
pub struct SchemaRow {
    /// The property name.
    pub name: String,
    /// The type label for the type pill, e.g. `string` or a `$ref` target name.
    pub type_label: String,
    /// Whether the type pill marks a `$ref` to another schema.
    pub type_is_ref: bool,
    /// Whether the property is required.
    pub required: bool,
    /// The description, as HTML.
    pub description_html: String,
    /// The constraint annotations, in document order.
    pub anns: Vec<Annotation>,
}

/// Renders `schema` as a tree rooted at `name`, expanded by default.
pub fn root(spec: &Value, name: &str, schema: &Value) -> SchemaNode {
    let mut stack = Vec::new();
    build_node(spec, name, schema, 0, &mut stack, true, false)
}

/// Builds one `<details>` node and, recursively, its children.
fn build_node(
    spec: &Value,
    name: &str,
    schema: &Value,
    depth: usize,
    stack: &mut Vec<String>,
    open: bool,
    required: bool,
) -> SchemaNode {
    let reference = html::str_field(schema, "$ref");
    let ref_target = reference.map(|reference| html::ref_name(reference).to_owned());
    let target = reference.map_or(Some(schema), |reference| {
        // Only local `#/...` references are resolved.
        reference
            .strip_prefix('#')
            .filter(|pointer| pointer.starts_with('/'))
            .and_then(|pointer| spec.pointer(pointer))
    });

    let body = target.map_or_else(
        || SchemaBody::Unresolved(reference.unwrap_or(name).to_owned()),
        |target| match ref_target.as_deref() {
            Some(target_name) if stack.iter().any(|key| key == target_name) => {
                SchemaBody::Cycle(target_name.to_owned())
            }
            _ if depth > MAX_DEPTH && is_branch(target) => SchemaBody::Deep,
            _ => SchemaBody::Resolved(build_resolved_body(
                spec,
                name,
                ref_target.as_deref(),
                target,
                depth,
                stack,
            )),
        },
    );

    let (type_label, type_is_ref) = type_pill(schema);
    SchemaNode {
        open,
        name: name.to_owned(),
        type_label,
        type_is_ref,
        required,
        brief: brief(target),
        body,
    }
}

/// The first line of the target's description, backticks stripped — the one-line
/// summary shown in the node's summary row.
fn brief(target: Option<&Value>) -> Option<String> {
    let text = html::str_field(target?, "description")?;
    let line = text.lines().next().unwrap_or_default().replace('`', "");
    (!line.is_empty()).then_some(line)
}

/// Builds the fully expanded body of one node.
fn build_resolved_body(
    spec: &Value,
    name: &str,
    ref_target: Option<&str>,
    target: &Value,
    depth: usize,
    stack: &mut Vec<String>,
) -> ResolvedBody {
    let description_html =
        html::str_field(target, "description").map_or_else(String::new, html::prose);
    let anns = schema_annotations(target);

    let required_names: Vec<&str> = html::field(target, "required")
        .and_then(Value::as_array)
        .map_or_default(|names| names.iter().filter_map(Value::as_str).collect());

    let properties = html::field(target, "properties").and_then(Value::as_object);
    let items = schema::array_items(target, html::str_field(target, "type"));

    // The recursion-guard key is the `$ref` target when the node has one, its name otherwise.
    let needs_key =
        properties.is_some_and(|properties| properties.values().any(is_branch)) || items.is_some();
    if needs_key {
        stack.push(ref_target.unwrap_or(name).to_owned());
    }

    let mut props = Vec::new();
    if let Some(properties) = properties {
        for (property_name, property) in properties {
            if is_branch(property) {
                props.push(SchemaEntry::Node(build_node(
                    spec,
                    property_name,
                    property,
                    depth + 1,
                    stack,
                    false,
                    required_names.contains(&property_name.as_str()),
                )));
            } else {
                props.push(SchemaEntry::Row(build_property_row(
                    property_name,
                    property,
                    &required_names,
                )));
            }
        }
    }

    let items = items.map(|items| {
        Box::new(build_node(
            spec,
            "items[]",
            items,
            depth + 1,
            stack,
            false,
            false,
        ))
    });

    if needs_key {
        stack.pop();
    }

    let examples = schema::example_values(target);
    let examples_label = if examples.len() > 1 {
        "examples"
    } else {
        "example"
    };
    let examples = examples
        .iter()
        .map(|example| highlight::highlight_json(example, highlight::EXAMPLE_CAP))
        .collect();

    ResolvedBody {
        description_html,
        anns,
        props,
        items,
        examples_label: examples_label.to_owned(),
        examples,
    }
}

/// Builds a flat (non-expandable) property row, including its documented example.
fn build_property_row(name: &str, property: &Value, required_names: &[&str]) -> SchemaRow {
    let (type_label, type_is_ref) = type_pill(property);
    let mut anns = schema_annotations(property);
    schema::push_example_annotation(
        &mut anns,
        schema::example(property),
        Some(highlight::EXAMPLE_CAP),
    );
    SchemaRow {
        name: name.to_owned(),
        type_label,
        type_is_ref,
        required: required_names.contains(&name),
        description_html: html::str_field(property, "description")
            .map_or_else(String::new, html::prose),
        anns,
    }
}

/// The `(label, is_ref)` of a schema's type pill.
///
/// This is intentionally fuller than [`schema::primary_type`]: the pill shows every union
/// member (including `null`), `$ref` target names, `array<...>` item labels, and `any` when
/// no type is documented.
fn type_pill(schema: &Value) -> (String, bool) {
    if let Some(reference) = html::str_field(schema, "$ref") {
        return (html::ref_name(reference).to_owned(), true);
    }
    match html::field(schema, "type") {
        Some(Value::String(t)) if t == "array" => {
            let (items, _) =
                type_pill(schema::array_items(schema, Some("array")).unwrap_or(&Value::Null));
            (format!("array<{items}>"), false)
        }
        Some(Value::String(t)) => (t.clone(), false),
        Some(Value::Array(types)) => (
            types
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" | "),
            false,
        ),
        _ if html::field(schema, "properties").is_some() => ("object".to_owned(), false),
        _ => ("any".to_owned(), false),
    }
}

/// Whether a schema renders as an expandable branch rather than a flat row.
fn is_branch(schema: &Value) -> bool {
    if schema.get("$ref").is_some() {
        return true;
    }
    if html::str_field(schema, "type") == Some("object")
        && html::field(schema, "properties").is_some()
    {
        return true;
    }
    if html::str_field(schema, "type") == Some("array")
        && let Some(items) = schema::array_items(schema, Some("array"))
    {
        return items.get("$ref").is_some()
            || html::field(items, "properties").is_some()
            || html::str_field(items, "type") == Some("array");
    }
    false
}

/// The constraint annotations of a schema: `format`, `default`, the numeric bounds,
/// `min length`, `max length`, `pattern`, and `enum`, in that order.
fn schema_annotations(schema: &Value) -> Vec<Annotation> {
    let mut anns = Vec::new();
    schema::push_common_annotations(&mut anns, schema);

    if let Some(min_length) = html::field(schema, "minLength") {
        anns.push(Annotation::new("min length", min_length.to_string()));
    }
    if let Some(max_length) = html::field(schema, "maxLength") {
        anns.push(Annotation::new("max length", max_length.to_string()));
    }
    if let Some(pattern) = html::str_field(schema, "pattern") {
        anns.push(Annotation::new("pattern", pattern.to_owned()));
    }
    if let Some(Value::Array(values)) = html::field(schema, "enum") {
        anns.push(Annotation::new(
            "enum",
            values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(" | "),
        ));
    }
    anns
}

/// The askama wrapper for `templates/partials/schema-node.html`. Escaping stays on:
/// the plain-text fields (`name`, `type_label`, `brief`, `Unresolved`/`Cycle`
/// payloads, annotations) are escaped by the template, while the pre-rendered HTML
/// fields (`description_html`, highlighted examples) and the recursive children are
/// interpolated with `|safe` in the partial.
#[derive(Template)]
#[template(path = "partials/schema-node.html")]
struct SchemaNodeTemplate<'a> {
    /// The node to render.
    node: &'a SchemaNode,
}

/// The askama wrapper for `templates/partials/schema-row.html`; see
/// [`SchemaNodeTemplate`] for the escaping contract.
#[derive(Template)]
#[template(path = "partials/schema-row.html")]
struct SchemaRowTemplate<'a> {
    /// The row to render.
    row: &'a SchemaRow,
}

impl SchemaNode {
    /// Renders the node and its children as HTML. The template partials call this to
    /// recurse.
    pub fn render(&self) -> askama::Result<String> {
        SchemaNodeTemplate { node: self }.render()
    }
}

impl SchemaRow {
    /// Renders the row as HTML. The template partials call this from property lists.
    pub fn render(&self) -> askama::Result<String> {
        SchemaRowTemplate { row: self }.render()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn renders_object_schemas_as_expandable_trees() {
        let spec = json!({
            "components": {"schemas": {
                "Thing": {"type": "object", "description": "A `thing`.", "properties": {
                    "name": {"type": "string", "description": "The name."},
                }, "required": ["name"]}
            }}
        });
        let schema = &spec["components"]["schemas"]["Thing"];
        let html = root(&spec, "Thing", schema).render().expect("renders");

        assert!(html.starts_with(r#"<details class="schema-node" open>"#));
        assert!(html.contains(r#"<span class="schema-name">Thing</span>"#));
        assert!(html.contains(r#"<span class="type-pill">object</span>"#));
        assert!(html.contains("<code>thing</code>"));
        assert!(html.contains(r#"<span class="req">*</span>"#));
        assert!(!html.contains(r#"<span class="ann-k">min</span>"#));
    }

    #[test]
    fn type_pill_keeps_null_union_members() {
        let schema = json!({"type": ["string", "null"]});

        // The tree's pill is deliberately fuller than the page builder's primary type.
        assert_eq!(type_pill(&schema), ("string | null".to_owned(), false));
        assert_eq!(schema::primary_type(&schema), Some("string"));
    }

    #[test]
    fn resolves_references_and_marks_cycles() {
        let spec = json!({
            "components": {"schemas": {
                "Node": {"type": "object", "properties": {
                    "name": {"type": "string"},
                    "next": {"$ref": "#/components/schemas/Node"},
                }}
            }}
        });
        let schema = &spec["components"]["schemas"]["Node"];
        let html = root(&spec, "Node", schema).render().expect("renders");

        assert!(html.contains(r#"<span class="type-pill ref">Node</span>"#));
        assert!(html.contains("↺ recursive reference to Node"));
    }

    #[test]
    fn escapes_hostile_names_and_annotations() {
        // The schema partials render every plain-text field through askama's escaper,
        // so a hostile document cannot inject markup through names, descriptions,
        // briefs, or annotation values.
        let spec = json!({
            "components": {"schemas": {
                "Bad": {"type": "object",
                        "description": "</summary> in a description.",
                        "properties": {
                    "</details><script>alert(1)</script>": {
                        "type": "string",
                        "description": "The `</script>` property.",
                        "pattern": "a\"b<c>&d"
                    },
                    "branch": {"type": "object", "properties": {
                        "<em>inner</em>": {"type": "string"}
                    }}
                }}
            }}
        });
        let schema = &spec["components"]["schemas"]["Bad"];
        let html = root(&spec, "Bad", schema).render().expect("renders");

        // Property names and the branch's inner name are template-escaped.
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&#60;/details&#62;&#60;script&#62;alert(1)&#60;/script&#62;"));
        assert!(html.contains("&#60;em&#62;inner&#60;/em&#62;"));
        // The brief strips the backticks and escapes the rest.
        assert!(html.contains("&#60;/summary&#62; in a description."));
        // Prose renders its own `<code>` and escapes the text around it.
        assert!(html.contains("<code>&lt;/script&gt;</code>"));
        // Annotation values are template-escaped too.
        assert!(!html.contains(r#"a"b<c>&d"#));
        assert!(html.contains("a&#34;b&#60;c&#62;&#38;d"));
    }

    #[test]
    fn reports_unresolved_references() {
        let spec = json!({});
        let schema = json!({"$ref": "#/components/schemas/Missing"});
        let html = root(&spec, "response", &schema).render().expect("renders");

        assert!(html.contains("Unresolved reference #/components/schemas/Missing."));
    }

    #[test]
    fn stops_expanding_below_the_depth_guard() {
        let spec = json!({
            "components": {"schemas": {
                "Deep": {"type": "object", "properties": {
                    "a": {"type": "object", "properties": {
                        "b": {"type": "object", "properties": {
                            "c": {"type": "object", "properties": {
                                "d": {"type": "object", "properties": {
                                    "e": {"type": "object", "properties": {
                                        "f": {"type": "string"}
                                    }}
                                }}
                            }}
                        }}
                    }}
                }}
            }}
        });
        let schema = &spec["components"]["schemas"]["Deep"];
        let html = root(&spec, "Deep", schema).render().expect("renders");

        assert!(html.contains("Deeper structure is expanded in the schema appendix below."));
    }

    #[test]
    fn renders_examples_as_highlighted_json() {
        let spec = json!({
            "components": {"schemas": {"Shaped": {
                "type": "object",
                "examples": [{"name": "x", "n": 2, "ok": true, "none": null}]
            }}}
        });
        let schema = &spec["components"]["schemas"]["Shaped"];
        let html = root(&spec, "Shaped", schema).render().expect("renders");

        assert!(html.contains(r#"<p class="ex-label">example</p>"#));
        assert!(html.contains(r#"<span class="j-key">&quot;name&quot;</span>"#));
        assert!(html.contains(r#"<span class="j-str">&quot;x&quot;</span>"#));
        assert!(html.contains(r#"<span class="j-num">2</span>"#));
        assert!(html.contains(r#"<span class="j-bool">true</span>"#));
        assert!(html.contains(r#"<span class="j-bool">null</span>"#));
    }

    #[test]
    fn pluralizes_multiple_examples() {
        let spec = json!({
            "components": {"schemas": {"Shaped": {"type": "string", "examples": ["a", "b"]}}}
        });
        let schema = &spec["components"]["schemas"]["Shaped"];
        let html = root(&spec, "Shaped", schema).render().expect("renders");

        assert!(html.contains(r#"<p class="ex-label">examples</p>"#));
        assert_eq!(html.matches("<pre class=\"json-view small\">").count(), 2);
    }

    #[test]
    fn unwraps_named_example_objects() {
        // The object form of `examples` (named Example Objects): each entry carries the
        // payload in `value`, alongside display metadata that must not be rendered.
        let spec = json!({
            "components": {"schemas": {"Shaped": {"type": "object", "examples": {
                "plain": {"summary": "A plain thing.", "value": {"name": "plain"}},
                "wrapper": {"externalValue": "https://example.com/x.json"}
            }}}}
        });
        let schema = &spec["components"]["schemas"]["Shaped"];
        let html = root(&spec, "Shaped", schema).render().expect("renders");

        // The `value` payload is unwrapped; the wrapper's `summary` is not rendered,
        // and an entry without `value` falls back to the raw entry.
        assert!(html.contains(r#"<span class="j-key">&quot;name&quot;</span>"#));
        assert!(html.contains("plain"));
        // The wrapper's `summary` metadata is not rendered (the word "summary" also
        // appears in the <summary> element, so match the metadata text itself).
        assert!(!html.contains("A plain thing."));
        // An entry without `value` falls back to the raw entry.
        assert!(html.contains("&quot;externalValue&quot;"));
    }

    #[test]
    fn truncates_long_examples_at_a_char_boundary() {
        // Longer than the 20k example cap, multibyte: the cut lands inside a character
        // and must back off to a boundary.
        let spec = json!({
            "components": {"schemas": {"Shaped": {"type": "string",
                "examples": ["é".repeat(12_000)]}}}
        });
        let schema = &spec["components"]["schemas"]["Shaped"];
        let html = root(&spec, "Shaped", schema).render().expect("renders");

        assert!(html.contains("truncated for display"));
        // The multibyte characters must survive intact.
        assert!(html.contains("é"));
    }

    #[test]
    fn annotates_schema_rows_with_examples() {
        let spec = json!({
            "components": {"schemas": {"Thing": {"type": "object", "properties": {
                "name": {"type": "string", "example": "widget"},
                "kind": {"type": "string", "examples": ["big", "small"]},
                "named": {"type": "string", "examples": {
                    "first": {"value": "one"},
                    "second": {"value": "two"}
                }},
                "count": {"type": "integer"}
            }}}}
        });
        let schema = &spec["components"]["schemas"]["Thing"];
        let html = root(&spec, "Thing", schema).render().expect("renders");

        // Field-level examples are annotations on the flat property rows, like
        // request-body fields; only the first `examples` entry is shown.
        assert!(
            html.contains(
                r#"<span class="ann-k">example</span> <span class="ann-v">widget</span>"#
            )
        );
        assert!(
            html.contains(r#"<span class="ann-k">example</span> <span class="ann-v">big</span>"#)
        );
        assert!(
            html.contains(r#"<span class="ann-k">example</span> <span class="ann-v">one</span>"#)
        );
        assert!(!html.contains("small"));
        assert!(!html.contains("two"));
        // No annotation is invented for a property without an example.
        assert_eq!(
            html.matches(r#"<span class="ann-k">example</span>"#)
                .count(),
            3
        );
    }

    #[test]
    fn truncates_long_row_example_annotations_at_a_char_boundary() {
        // 21,000 bytes, multibyte and escape-sensitive. The 20,000-byte cut lands inside
        // the leading `é` of a 7-byte repeat and must back off to a character boundary.
        let pattern = "é<\"&ab";
        let example = pattern.repeat(3_000);
        assert!(example.len() > highlight::EXAMPLE_CAP);
        let spec = json!({
            "components": {"schemas": {"Shaped": {"type": "object", "properties": {
                "long": {"type": "string", "example": example}
            }}}}
        });
        let schema = &spec["components"]["schemas"]["Shaped"];
        let html = root(&spec, "Shaped", schema).render().expect("renders");

        let marker = r#"<span class="ann-k">example</span> <span class="ann-v">"#;
        let start = html.find(marker).expect("example annotation") + marker.len();
        let end = start + html[start..].find("</span>").expect("annotation end");
        let value = &html[start..end];

        assert!(value.contains("truncated for display"));
        // The cut backs off to the boundary before the incomplete `é`, keeping exactly the
        // whole repeats that fit under the cap.
        assert_eq!(
            value.matches('é').count(),
            highlight::EXAMPLE_CAP / pattern.len()
        );
        // The value stays escaped.
        assert!(!value.contains('<'));
        assert!(value.contains("&#60;"));
        assert!(value.contains("&#38;"));
        assert!(value.contains("&#34;"));
    }
}
