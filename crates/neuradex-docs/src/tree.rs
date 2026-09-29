//! Builds OpenAPI schemas as the page's collapsible `<details>` trees.
//!
//! The trees recurse over arbitrary schema shapes, so they render through their own
//! askama partials (`templates/partials/schema-node.html` and `schema-row.html`).
//! Plain-text fields (names, labels, annotations) are escaped by the template; the
//! pre-rendered HTML fields (`*_html`, the highlighted examples, and the recursive
//! children) are interpolated with `|safe`. The markup and class names are the ones
//! the theme's stylesheet and behavior script expect.

use serde_json::Value;

use askama::Template;

use crate::html::{esc, field, prose, ref_name, resolve_ref, str_field};
use crate::model::Anno;

/// The maximum nesting depth expanded inline; deeper branches point at the schema appendix.
const MAX_DEPTH: usize = 4;

/// The character cap for rendered schema examples.
const EXAMPLE_CAP: usize = 20_000;

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
    pub anns: Vec<Anno>,
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
    pub anns: Vec<Anno>,
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
        || SchemaBody::Unresolved(reference.unwrap_or(name).to_owned()),
        |target| match ref_target.as_deref() {
            Some(target_name) if stack.iter().any(|key| key == target_name) => {
                SchemaBody::Cycle(target_name.to_owned())
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

    let (type_label, type_is_ref) = type_info(schema);
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
    let text = str_field(target?, "description")?;
    let line = text.lines().next().unwrap_or_default().replace('`', "");
    (!line.is_empty()).then_some(line)
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
    let anns = schema_anns(target);

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
    let examples = examples
        .iter()
        .map(|example| highlight_json(example, EXAMPLE_CAP))
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

/// The recursion-guard key a node contributes to the stack: its `$ref` target when it has
/// one, its name otherwise.
fn stack_key(ref_target: Option<&str>, name: &str) -> String {
    ref_target.unwrap_or(name).to_owned()
}

/// Builds a flat (non-expandable) property row.
fn property_row(name: &str, property: &Value, required_names: &[&str]) -> SchemaRow {
    let (type_label, type_is_ref) = type_info(property);
    SchemaRow {
        name: name.to_owned(),
        type_label,
        type_is_ref,
        required: required_names.contains(&name),
        description_html: str_field(property, "description").map_or_else(String::new, prose),
        anns: schema_anns(property),
    }
}

/// The `(label, is_ref)` of a schema's type pill.
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

/// The constraint annotations of a schema: `format`, `default`, the numeric bounds,
/// `pattern`, and `enum`, in that order.
fn schema_anns(schema: &Value) -> Vec<Anno> {
    let mut anns = Vec::new();
    let mut push = |key: &str, value: String| {
        anns.push(Anno {
            key: key.to_owned(),
            value,
        });
    };
    if let Some(format) = str_field(schema, "format") {
        push("format", format.to_owned());
    }
    if let Some(default) = field(schema, "default") {
        push("default", default.to_string());
    }
    if let Some(minimum) = field(schema, "minimum") {
        push("min", minimum.to_string());
    }
    if let Some(maximum) = field(schema, "maximum") {
        push("max", maximum.to_string());
    }
    if let Some(min_length) = field(schema, "minLength") {
        push("min length", min_length.to_string());
    }
    if let Some(max_length) = field(schema, "maxLength") {
        push("max length", max_length.to_string());
    }
    if let Some(pattern) = str_field(schema, "pattern") {
        push("pattern", pattern.to_owned());
    }
    if let Some(Value::Array(values)) = field(schema, "enum") {
        push(
            "enum",
            values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    anns
}

/// The schema's `examples` as a list, mirroring the behavior script's handling.
///
/// Named Example Objects (the object form of `examples`) are unwrapped to their
/// `value` payload when present, so the summary fields of the wrapper are not shown.
fn example_values(schema: &Value) -> Vec<&Value> {
    match field(schema, "examples") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(map)) => map.values().map(example_payload).collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

/// The example payload of one entry of an object-valued `examples`.
fn example_payload(example: &Value) -> &Value {
    field(example, "value").unwrap_or(example)
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
                    out.push_str("<span class=\"j-key\">");
                    out.push_str(&esc(&slice[i..j]));
                    out.push_str("</span>");
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
        out.push_str("<span class=\"");
        out.push_str(class);
        out.push_str("\">");
        out.push_str(&esc(&slice[i..end]));
        out.push_str("</span>");
        i = end;
        last = i;
    }
    out.push_str(&esc(&slice[last..]));
    if truncated {
        out.push_str("\n… truncated for display");
    }
    out
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
    fn truncates_small_caps_directly() {
        let value = json!("ééééééééééééééééééééééééééééééééééé");
        let html = highlight_json(&value, 10);

        assert!(html.contains("truncated for display"));
        assert!(html.contains("é"));
    }
}
