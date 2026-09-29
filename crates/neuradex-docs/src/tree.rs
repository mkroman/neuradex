//! Builds OpenAPI schemas as the page's collapsible `<details>` trees.
//!
//! The trees recurse over arbitrary schema shapes, so they render through their own
//! askama partials (`templates/partials/schema-node.html` and `schema-row.html`) with
//! escaping disabled: every interpolated field is pre-escaped here. The markup and
//! class names are the ones the theme's stylesheet and behavior script expect.

use std::fmt::Write as _;

use askama::Template;
use serde_json::Value;

use crate::html::{esc, field, prose, ref_name, resolve_ref, str_field};

/// The maximum nesting depth expanded inline; deeper branches point at the schema appendix.
const MAX_DEPTH: usize = 4;

/// The character cap for rendered schema examples.
const EXAMPLE_CAP: usize = 20_000;

/// One `<details>` node of a schema tree; see `templates/partials/schema-node.html`.
pub struct SchemaNode {
    /// Whether the node is expanded by default (only the root is).
    pub open: bool,
    /// The node label, pre-escaped.
    pub name: String,
    /// The type pill, as HTML.
    pub type_pill: String,
    /// Whether the node is a required property of its parent.
    pub required: bool,
    /// The first line of the target's description, backticks stripped, pre-escaped.
    pub brief: Option<String>,
    /// What the node's body resolves to.
    pub body: SchemaBody,
}

/// The body of a schema node: one of the three truncated render modes, or the full
/// resolved body.
pub enum SchemaBody {
    /// The `$ref` could not be resolved; the payload is the reference or node label,
    /// pre-escaped.
    Unresolved(String),
    /// The `$ref` resolves to a node already on the stack; the payload is the target
    /// name, pre-escaped.
    Cycle(String),
    /// The depth guard cut the expansion; the appendix has the rest.
    Deep,
    /// The fully expanded body.
    Resolved(ResolvedBody),
}

/// The resolved body of a schema node; all HTML fields are pre-rendered.
pub struct ResolvedBody {
    /// The description, as HTML.
    pub description_html: String,
    /// The constraint annotations, as HTML.
    pub anns_html: String,
    /// The properties, in document order.
    pub props: Vec<SchemaEntry>,
    /// The element schema of an array, if any.
    pub items: Option<Box<SchemaNode>>,
    /// `example` or `examples`, by count.
    pub examples_label: String,
    /// The examples, as HTML.
    pub examples_html: String,
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
    /// The property name, pre-escaped.
    pub name: String,
    /// The type pill, as HTML.
    pub type_pill: String,
    /// Whether the property is required.
    pub required: bool,
    /// The description, as HTML.
    pub description_html: String,
    /// The constraint annotations, as HTML.
    pub anns_html: String,
}

/// Renders `schema` as a tree rooted at `name`, expanded by default.
pub fn root(spec: &Value, name: &str, schema: &Value) -> SchemaNode {
    let mut stack = Vec::new();
    node(spec, name, schema, 0, &mut stack, true, false)
}

/// Builds one `<details>` node and, recursively, its children.
fn node(
    spec: &Value,
    name: &str,
    schema: &Value,
    depth: usize,
    stack: &mut Vec<String>,
    open: bool,
    required: bool,
) -> SchemaNode {
    let reference = str_field(schema, "$ref");
    let ref_target = reference.map(|r| ref_name(r).to_owned());
    let target = reference.map_or(Some(schema), |r| resolve_ref(spec, r));

    let body = target.map_or_else(
        || SchemaBody::Unresolved(esc(reference.unwrap_or(name))),
        |target| match ref_target.as_deref() {
            Some(target_name) if stack.iter().any(|key| key == target_name) => {
                SchemaBody::Cycle(esc(target_name))
            }
            _ if depth > MAX_DEPTH && is_branch(target) => SchemaBody::Deep,
            _ => SchemaBody::Resolved(resolved_body(
                spec,
                name,
                ref_target.as_deref(),
                target,
                depth,
                stack,
            )),
        },
    );

    SchemaNode {
        open,
        name: esc(name),
        type_pill: type_pill(schema),
        required,
        brief: brief(target),
        body,
    }
}

/// The first line of the target's description, backticks stripped, pre-escaped — the
/// one-line summary shown in the node's summary row.
fn brief(target: Option<&Value>) -> Option<String> {
    let text = str_field(target?, "description")?;
    let line = text.lines().next().unwrap_or_default().replace('`', "");
    (!line.is_empty()).then(|| esc(&line))
}

/// Builds the fully expanded body of one node.
fn resolved_body(
    spec: &Value,
    name: &str,
    ref_target: Option<&str>,
    target: &Value,
    depth: usize,
    stack: &mut Vec<String>,
) -> ResolvedBody {
    let description_html = str_field(target, "description").map_or_else(String::new, prose);
    let anns_html = schema_anns(target);

    let required_names: Vec<&str> = field(target, "required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut props = Vec::new();
    if let Some(properties) = field(target, "properties").and_then(Value::as_object) {
        for (property_name, property) in properties {
            if is_branch(property) {
                stack.push(stack_key(ref_target, name));
                props.push(SchemaEntry::Node(node(
                    spec,
                    property_name,
                    property,
                    depth + 1,
                    stack,
                    false,
                    required_names.contains(&property_name.as_str()),
                )));
                stack.pop();
            } else {
                props.push(SchemaEntry::Row(property_row(
                    property_name,
                    property,
                    &required_names,
                )));
            }
        }
    }

    let items = if str_field(target, "type") == Some("array") {
        field(target, "items").map(|items| {
            stack.push(stack_key(ref_target, name));
            let child = node(spec, "items[]", items, depth + 1, stack, false, false);
            stack.pop();
            Box::new(child)
        })
    } else {
        None
    };

    let examples = example_values(target);
    let examples_label = if examples.len() > 1 {
        "examples"
    } else {
        "example"
    };
    let examples_html = examples.iter().fold(String::new(), |mut out, example| {
        let _ = write!(
            out,
            "<pre class=\"json-view small\"><code>{}</code></pre>",
            highlight_json(example, EXAMPLE_CAP)
        );
        out
    });

    ResolvedBody {
        description_html,
        anns_html,
        props,
        items,
        examples_label: examples_label.to_owned(),
        examples_html,
    }
}

/// The recursion-guard key a node contributes to the stack: its `$ref` target when it has
/// one, its name otherwise.
fn stack_key(ref_target: Option<&str>, name: &str) -> String {
    ref_target.unwrap_or(name).to_owned()
}

/// Builds a flat (non-expandable) property row.
fn property_row(name: &str, property: &Value, required_names: &[&str]) -> SchemaRow {
    SchemaRow {
        name: esc(name),
        type_pill: type_pill(property),
        required: required_names.contains(&name),
        description_html: str_field(property, "description").map_or_else(String::new, prose),
        anns_html: schema_anns(property),
    }
}

/// The `(label, css)` of a schema's type pill.
fn type_info(schema: &Value) -> (String, bool) {
    if let Some(reference) = str_field(schema, "$ref") {
        return (ref_name(reference).to_owned(), true);
    }
    match field(schema, "type") {
        Some(Value::String(t)) if t == "array" => {
            let (items, _) = type_info(field(schema, "items").unwrap_or(&Value::Null));
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
        _ if field(schema, "properties").is_some() => ("object".to_owned(), false),
        _ => ("any".to_owned(), false),
    }
}

fn type_pill(schema: &Value) -> String {
    let (label, is_ref) = type_info(schema);
    let mut out = String::from("<span class=\"type-pill");
    let _ = write!(out, "{}", if is_ref { " ref" } else { "" });
    let _ = write!(out, "\">{}</span>", esc(&label));
    out
}

/// Whether a schema renders as an expandable branch rather than a flat row.
fn is_branch(schema: &Value) -> bool {
    if schema.get("$ref").is_some() {
        return true;
    }
    if str_field(schema, "type") == Some("object") && field(schema, "properties").is_some() {
        return true;
    }
    if str_field(schema, "type") == Some("array")
        && let Some(items) = field(schema, "items")
    {
        return items.get("$ref").is_some()
            || field(items, "properties").is_some()
            || str_field(items, "type") == Some("array");
    }
    false
}

/// The constraint annotations of a schema, as `key:value` HTML pairs.
fn schema_anns(schema: &Value) -> String {
    let mut out = String::new();
    let mut pair = |key: &str, value: String| {
        let _ = write!(
            out,
            "<span class=\"ann\"><span class=\"ann-k\">{}</span> <span class=\"ann-v\">{}</span></span>",
            esc(key),
            esc(&value)
        );
    };
    if let Some(format) = str_field(schema, "format") {
        pair("format", format.to_owned());
    }
    if let Some(default) = field(schema, "default") {
        pair("default", default.to_string());
    }
    if let Some(minimum) = field(schema, "minimum") {
        pair("min", minimum.to_string());
    }
    if let Some(maximum) = field(schema, "maximum") {
        pair("max", maximum.to_string());
    }
    if let Some(min_length) = field(schema, "minLength") {
        pair("min length", min_length.to_string());
    }
    if let Some(max_length) = field(schema, "maxLength") {
        pair("max length", max_length.to_string());
    }
    if let Some(pattern) = str_field(schema, "pattern") {
        pair("pattern", pattern.to_owned());
    }
    if let Some(Value::Array(values)) = field(schema, "enum") {
        pair(
            "enum",
            values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    out
}

/// The schema's `examples` as a list, mirroring the behavior script's handling.
fn example_values(schema: &Value) -> Vec<&Value> {
    match field(schema, "examples") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(map)) => map.values().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

/// Renders a JSON value as syntax-highlighted HTML, mirroring the behavior script's
/// runtime highlighter (keys, strings, numbers, booleans/null).
fn highlight_json(value: &Value, cap: usize) -> String {
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return esc(&value.to_string());
    };
    highlight_text(&text, cap)
}

fn highlight_text(text: &str, cap: usize) -> String {
    let truncated = text.len() > cap;
    let slice = if truncated {
        let mut cut = cap;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        &text[..cut]
    } else {
        text
    };
    let bytes = slice.as_bytes();

    let mut out = String::new();
    let mut last = 0;
    let mut i = 0;
    while i < bytes.len() {
        let (class, end) = match bytes[i] {
            b'"' => {
                let mut j = i + 1;
                while j < bytes.len() {
                    match bytes[j] {
                        b'\\' => j += 2,
                        b'"' => {
                            j += 1;
                            break;
                        }
                        _ => j += 1,
                    }
                }
                let j = j.min(bytes.len());
                let mut k = j;
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b':' {
                    out.push_str(&esc(&slice[last..i]));
                    let _ = write!(out, "<span class=\"j-key\">{}</span>", esc(&slice[i..j]));
                    out.push_str(&esc(&slice[j..=k]));
                    i = k + 1;
                    last = i;
                    continue;
                }
                ("j-str", j)
            }
            b't' if slice[i..].starts_with("true") => ("j-bool", i + 4),
            b'f' if slice[i..].starts_with("false") => ("j-bool", i + 5),
            b'n' if slice[i..].starts_with("null") => ("j-bool", i + 4),
            b'-' | b'0'..=b'9' => {
                let mut j = i + 1;
                while j < bytes.len()
                    && matches!(bytes[j], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                {
                    j += 1;
                }
                ("j-num", j)
            }
            _ => {
                i += 1;
                continue;
            }
        };
        out.push_str(&esc(&slice[last..i]));
        let _ = write!(
            out,
            "<span class=\"{class}\">{}</span>",
            esc(&slice[i..end])
        );
        i = end;
        last = i;
    }
    out.push_str(&esc(&slice[last..]));
    if truncated {
        out.push_str("\n… truncated for display");
    }
    out
}

/// The askama wrapper for `templates/partials/schema-node.html`; escaping is off because
/// every `SchemaNode` field is pre-escaped above.
#[derive(Template)]
#[template(path = "partials/schema-node.html", escape = "none")]
struct SchemaNodeTemplate<'a> {
    /// The node to render.
    node: &'a SchemaNode,
}

/// The askama wrapper for `templates/partials/schema-row.html`; escaping is off because
/// every `SchemaRow` field is pre-escaped above.
#[derive(Template)]
#[template(path = "partials/schema-row.html", escape = "none")]
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
    fn highlights_json_examples() {
        let spec = json!({
            "components": {"schemas": {"Shaped": {
                "type": "object",
                "examples": [{"name": "x", "n": 2, "ok": true, "none": null}]
            }}}
        });
        let schema = &spec["components"]["schemas"]["Shaped"];
        let html = root(&spec, "Shaped", schema).render().expect("renders");

        assert!(html.contains(r#"<p class="ex-label">example</p>"#));
        // The highlighter escapes the tokens it wraps (the old string renderer did too).
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
    fn truncates_small_caps_directly() {
        let value = json!("ééééééééééééééééééééééééééééééééééé");
        let html = highlight_json(&value, 10);

        assert!(html.contains("truncated for display"));
        assert!(html.contains("é"));
    }
}
