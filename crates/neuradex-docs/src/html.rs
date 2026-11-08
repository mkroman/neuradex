//! HTML and JSON helpers shared by the page renderer.

use serde_json::Value;

/// Appends `s` to `out`, escaping it for HTML text nodes and attribute values.
pub fn push_escaped(out: &mut String, s: &str) {
    out.push_str(&htmlize::escape_all_quotes(s));
}

/// Renders description prose: backtick-delimited spans become inline `<code>` elements,
/// everything else stays escaped text — the same contract the behavior script uses for
/// runtime-rendered text.
#[must_use]
pub fn prose(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        push_escaped(&mut out, &rest[..open]);
        let body = &rest[open + 1..];
        if let Some(close) = body.find('`') {
            out.push_str("<code>");
            push_escaped(&mut out, &body[..close]);
            out.push_str("</code>");
            rest = &body[close + 1..];
        } else {
            // An unpaired backtick stays literal text, like the script's split does.
            out.push('`');
            rest = body;
        }
    }
    push_escaped(&mut out, rest);
    out
}

/// Slugs a path into a DOM id, mirroring the behavior script's expectations:
/// lowercase, runs of non-alphanumeric characters collapsed to one dash, dashes
/// trimmed from both ends.
#[must_use]
pub fn slug(path: &str) -> String {
    let mut out = String::new();
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "endpoint".to_owned()
    } else {
        out
    }
}

/// Returns the last path segment of a `$ref` pointer.
pub fn ref_name(reference: &str) -> &str {
    reference
        .rsplit_once('/')
        .map_or(reference, |(_, name)| name)
}

/// Returns a field of a JSON object, treating explicit nulls as absent.
pub fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|value| !value.is_null())
}

/// Returns a string field of a JSON object.
pub fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    field(value, key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup() {
        let mut out = String::new();
        push_escaped(&mut out, "a<b>&\"'");

        assert_eq!(out, "a&lt;b&gt;&amp;&quot;&apos;");

        let mut plain = String::new();
        push_escaped(&mut plain, "plain");

        assert_eq!(plain, "plain");
    }

    #[test]
    fn renders_backticks_as_code() {
        assert_eq!(
            prose("uses `redirects` here"),
            "uses <code>redirects</code> here"
        );
        assert_eq!(prose("a `b` c `d`"), "a <code>b</code> c <code>d</code>");
    }

    #[test]
    fn escapes_around_and_inside_code_spans() {
        assert_eq!(
            prose("through `</head>` or `<body>`"),
            "through <code>&lt;/head&gt;</code> or <code>&lt;body&gt;</code>"
        );
    }

    #[test]
    fn keeps_unpaired_backticks_literal() {
        assert_eq!(prose("a `b"), "a `b");
        assert_eq!(prose("`"), "`");
    }

    #[test]
    fn slugs_paths_like_the_script() {
        assert_eq!(slug("/v1/fetch"), "v1-fetch");
        assert_eq!(slug("/healthz"), "healthz");
        assert_eq!(slug("/a--b/c/"), "a-b-c");
        assert_eq!(slug("///"), "endpoint");
    }

    #[test]
    fn resolves_json_pointer_escapes() {
        let spec = serde_json::json!({
            "components": {"schemas": {"a/b~c": "resolved"}}
        });
        let resolved = spec.pointer("/components/schemas/a~1b~0c");
        assert_eq!(resolved.and_then(Value::as_str), Some("resolved"));
        assert_eq!(spec.pointer("/components/schemas/missing"), None);
    }
}
