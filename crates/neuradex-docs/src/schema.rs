//! Schema-shape helpers shared by the page builder and the schema trees.
//!
//! [`primary_type`] returns the single type the page builder uses for a parameter or
//! request-body row, ignoring the `null` member of an OpenAPI 3.1 type union. The schema
//! trees deliberately keep a fuller type label (see [`crate::tree`]): their type pill also
//! renders unions, `$ref` target names, and `any`. Only the shape decisions that are the
//! same in both places live here.

use serde_json::Value;

use crate::html;
use crate::model::Annotation;

/// Returns the schema's primary type, ignoring OpenAPI 3.1's `null` union member.
pub fn primary_type(schema: &Value) -> Option<&str> {
    match schema.get("type")? {
        Value::String(kind) => Some(kind),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|kind| *kind != "null"),
        _ => None,
    }
}

/// Returns the schema's `items` when `kind` is `array`.
///
/// Callers pass the type they interpreted themselves: the page builder passes
/// [`primary_type`] (so a `["array", "null"]` union counts as an array), while the schema
/// trees pass the literal `type` string to preserve their existing rendering.
pub fn array_items<'a>(schema: &'a Value, kind: Option<&str>) -> Option<&'a Value> {
    if kind == Some("array") {
        html::field(schema, "items")
    } else {
        None
    }
}

/// Appends the annotations shared by query parameters, request-body fields, and schema
/// properties: `format`, `default`, and the numeric bounds.
pub fn push_common_annotations(anns: &mut Vec<Annotation>, schema: &Value) {
    if let Some(format) = html::str_field(schema, "format") {
        anns.push(Annotation::new("format", format.to_owned()));
    }
    if let Some(default) = html::field(schema, "default") {
        anns.push(Annotation::new("default", default.to_string()));
    }
    if let Some(minimum) = html::field(schema, "minimum") {
        anns.push(Annotation::new("min", minimum.to_string()));
    }
    if let Some(maximum) = html::field(schema, "maximum") {
        anns.push(Annotation::new("max", maximum.to_string()));
    }
}

/// Appends the `type` annotation for a parameter or request-body field, expanding arrays to
/// `array<item>` when the element type is known.
pub fn push_type_annotation(
    anns: &mut Vec<Annotation>,
    schema: Option<&Value>,
    kind: Option<&str>,
) {
    if kind == Some("array") {
        let items_type = schema
            .and_then(|schema| array_items(schema, kind))
            .and_then(primary_type);
        anns.push(Annotation::new(
            "type",
            items_type.map_or_else(
                || "array".to_owned(),
                |items_type| format!("array<{items_type}>"),
            ),
        ));
    } else if let Some(kind) = kind {
        anns.push(Annotation::new("type", kind.to_owned()));
    }
}

/// Appends the `example` annotation when an example value is documented.
pub fn push_example_annotation(anns: &mut Vec<Annotation>, example: Option<&Value>) {
    if let Some(example) = example {
        anns.push(Annotation::new("example", flat_value(example)));
    }
}

/// Returns the first example documented on a schema: the singular `example`, or the first
/// entry of the `examples` collection (array or named-object form).
pub fn example(schema: &Value) -> Option<&Value> {
    html::field(schema, "example").or_else(|| example_values(schema).into_iter().next())
}

/// Returns the request-body field example, preserving that path's earlier behavior: the
/// singular `example` or the first **array-form** `examples` entry.
///
/// Named Example Objects are not unwrapped here, so parameter and request-body rendering
/// stays unchanged; schema rows use [`example`], which also understands the object form.
pub fn body_example(schema: &Value) -> Option<&Value> {
    html::field(schema, "example").or_else(|| {
        html::field(schema, "examples")
            .and_then(Value::as_array)
            .and_then(|examples| examples.first())
    })
}

/// The schema's `examples` as a list, mirroring the behavior script's handling.
///
/// Named Example Objects (the object form of `examples`) are unwrapped to their `value`
/// payload when present, so the summary fields of the wrapper are not shown.
pub fn example_values(schema: &Value) -> Vec<&Value> {
    match html::field(schema, "examples") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(map)) => map
            .values()
            .map(|example| html::field(example, "value").unwrap_or(example))
            .collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

/// Formats a JSON value for an annotation column: arrays are comma-joined, strings are
/// shown raw, everything else compact JSON.
pub fn flat_value(value: &Value) -> String {
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
