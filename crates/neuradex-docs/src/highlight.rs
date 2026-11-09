//! Syntax highlighting for JSON examples.
//!
//! The markup mirrors the behavior script's runtime highlighter (`j-key`, `j-str`,
//! `j-num`, `j-bool`) so server-rendered examples look the same as client-rendered ones.
//! Every token is HTML-escaped through [`crate::html::push_escaped`]; the highlighter needs
//! no extra dependency.

use serde_json::Value;

use crate::html;

/// The byte cap for rendered example annotations.
pub const EXAMPLE_CAP: usize = 20_000;

/// Renders a JSON value as syntax-highlighted HTML, mirroring the behavior script's
/// runtime highlighter (keys, strings, numbers, booleans/null).
pub fn highlight_json(value: &Value, cap: usize) -> String {
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return htmlize::escape_all_quotes(value.to_string()).into_owned();
    };
    highlight_text(&text, cap)
}

/// Highlights JSON text, escaping every token and truncating at `cap` bytes on a character
/// boundary.
fn highlight_text(text: &str, cap: usize) -> String {
    let truncated = text.len() > cap;
    let slice = if truncated {
        &text[..html::truncate_index(text, cap)]
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
                    html::push_escaped(&mut out, &slice[last..i]);
                    out.push_str("<span class=\"j-key\">");
                    html::push_escaped(&mut out, &slice[i..j]);
                    out.push_str("</span>");
                    html::push_escaped(&mut out, &slice[j..=k]);
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
        html::push_escaped(&mut out, &slice[last..i]);
        out.push_str("<span class=\"");
        out.push_str(class);
        out.push_str("\">");
        html::push_escaped(&mut out, &slice[i..end]);
        out.push_str("</span>");
        i = end;
        last = i;
    }
    html::push_escaped(&mut out, &slice[last..]);
    if truncated {
        out.push_str("\n… truncated for display");
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn highlights_json_tokens() {
        let html = highlight_json(
            &json!({"name": "x", "n": 2, "ok": true, "none": null}),
            1_000,
        );

        // The highlighter escapes the tokens it wraps (the old string renderer did too).
        assert!(html.contains(r#"<span class="j-key">&quot;name&quot;</span>"#));
        assert!(html.contains(r#"<span class="j-str">&quot;x&quot;</span>"#));
        assert!(html.contains(r#"<span class="j-num">2</span>"#));
        assert!(html.contains(r#"<span class="j-bool">true</span>"#));
        assert!(html.contains(r#"<span class="j-bool">null</span>"#));
    }

    #[test]
    fn truncates_small_caps_directly() {
        let value = json!("ééééééééééééééééééééééééééééééééééé");
        let html = highlight_json(&value, 10);

        assert!(html.contains("truncated for display"));
        assert!(html.contains("é"));
    }

    #[test]
    fn truncates_at_a_character_boundary() {
        // The cut lands inside the two-byte `é` and must back off to a boundary.
        let html = highlight_json(&json!("ééé"), 3);

        assert!(html.contains("é"));
        assert!(html.contains("truncated for display"));
    }
}
