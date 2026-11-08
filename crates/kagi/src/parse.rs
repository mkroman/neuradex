//! Parsing of Kagi stream payloads and result HTML fragments.

// `mod parse` is private, so `pub` would only be crate-visible in effect. `pub(crate)` states
// the intended reachability instead: the parser items used by `client` cannot leak if the
// module is ever exposed.
#![allow(clippy::redundant_pub_crate)]

use std::sync::LazyLock;

use scraper::{ElementRef, Html, Node, Selector};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use crate::model::{ImageResult, SearchResult};

/// Represents a message parsed from a Kagi socket stream.
///
/// Messages arrive either framed as standard server-sent events or as `Tag:JSON_BODY\0\n`
/// chunks; in both cases each message carries a tag and a JSON payload.
#[derive(Deserialize, Debug)]
pub(crate) struct KagiMessage {
    /// The message tag (e.g., "search", "search.info", "meta").
    /// This is extracted from the wire prefix or the JSON body.
    tag: String,
    /// The flexible payload. Using `Value` allows this struct to handle
    /// diverse message types (HTML strings, objects, or nulls) without breaking.
    payload: Option<Value>,
}

/// The structured search results payload of a `search_results_json` message.
#[derive(Deserialize)]
struct SearchResultsJson {
    #[serde(default)]
    items: Vec<JsonSearchResult>,
}

/// A single structured result within a `search_results_json` payload.
#[derive(Deserialize)]
struct JsonSearchResult {
    title: String,
    url: String,
    #[serde(default)]
    snippet: String,
}

// Extracts the `window.sse_nonce` value from the raw HTML content.
pub(crate) fn extract_nonce(html: &str) -> Option<String> {
    const MARKER: &str = "window.sse_nonce";

    let mut rest = html;

    while let Some(index) = rest.find(MARKER) {
        rest = &rest[index + MARKER.len()..];

        let Some(after) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some((nonce, _)) = quoted_string_literal(after.trim_start()) else {
            continue;
        };

        if !nonce.is_empty() {
            return Some(nonce.to_owned());
        }
    }

    None
}

/// Splits a leading double-quoted string literal from `input`.
///
/// Returns the literal's contents without the surrounding quotes and the input following the
/// closing quote. The literal ends at the first unescaped `"`, so backslash escapes such as
/// `\"` are skipped. Returns `None` if `input` does not start with a quote or the literal is
/// unterminated.
fn quoted_string_literal(input: &str) -> Option<(&str, &str)> {
    let body = input.strip_prefix('"')?;
    let mut escaped = false;

    for (index, character) in body.char_indices() {
        match (escaped, character) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '"') => return Some((&body[..index], &body[index + 1..])),
            _ => {}
        }
    }

    None
}

/// Parses a raw stream response body into a vector of `KagiMessage`s, detecting whether the
/// server responded with the standard server-sent event framing or the legacy null-delimited
/// wire format.
pub(crate) fn parse_stream(raw_body: &str) -> Vec<KagiMessage> {
    if raw_body.lines().any(|line| line.starts_with("data:")) {
        parse_sse_stream(raw_body)
    } else {
        parse_kagi_stream(raw_body)
    }
}

/// Parses a standard server-sent event stream where each `data:` line carries a JSON array of
/// messages.
///
/// Any non-data lines (such as `id:` fields or the `hi` greeting) are ignored.
fn parse_sse_stream(raw_body: &str) -> Vec<KagiMessage> {
    raw_body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .filter_map(|json| {
            serde_json::from_str::<Vec<KagiMessage>>(json)
                .inspect_err(|error| {
                    debug!(
                        event = json,
                        exception.message = %error,
                        "dropping an unparseable Kagi SSE event"
                    );
                })
                .ok()
        })
        .flatten()
        .collect()
}

/// Parses a raw stream response body in the legacy `Tag:JSON_BODY\0\n` wire format into a vector
/// of `KagiMessage`s.
///
/// This handles the specific Kagi wire format:
/// 1. Splits by `\0\n` delimiter.
/// 2. Splits each chunk at the first `:` into (`WireTag`, `JsonBody`).
/// 3. Deserializes the JSON body.
/// 4. Ensures the `tag` field is populated.
fn parse_kagi_stream(raw_body: &str) -> Vec<KagiMessage> {
    raw_body
        .split_terminator("\0\n")
        .filter_map(|chunk| {
            // Split wire format: "tag:json_data"
            let Some((wire_tag, json_str)) = chunk.split_once(':') else {
                debug!(chunk, "dropping a Kagi chunk without a wire tag");
                return None;
            };
            // Parse JSON body
            let mut message: KagiMessage = match serde_json::from_str(json_str) {
                Ok(message) => message,
                Err(error) => {
                    debug!(
                        wire_tag,
                        exception.message = %error,
                        "dropping an unparseable Kagi chunk"
                    );
                    return None;
                }
            };
            // Normalize Tag: If the JSON body didn't have a tag, use the wire tag.
            if message.tag.is_empty() {
                message.tag = wire_tag.to_string();
            }

            Some(message)
        })
        .collect()
}

/// Returns the `content` string of a message's payload, if it has one.
fn payload_content(message: &KagiMessage) -> Option<&str> {
    message.payload.as_ref()?.get("content")?.as_str()
}

pub(crate) fn parse_search_result_messages(messages: &[KagiMessage]) -> Vec<SearchResult> {
    // Prefer the structured results payload when the server provides one.
    for message in messages
        .iter()
        .filter(|message| message.tag == "search_results_json")
    {
        let Some(payload) = message.payload.as_ref().and_then(Value::as_str) else {
            continue;
        };
        let results = match serde_json::from_str::<SearchResultsJson>(payload) {
            Ok(results) => results,
            Err(error) => {
                debug!(
                    exception.message = %error,
                    "falling back to HTML results, structured payload is malformed"
                );
                continue;
            }
        };

        if !results.items.is_empty() {
            return results
                .items
                .into_iter()
                .map(|item| SearchResult {
                    title: htmlize::unescape(item.title).into_owned(),
                    url: item.url,
                    description: htmlize::unescape(item.snippet).into_owned(),
                })
                .collect();
        }
    }

    // Fall back to parsing the HTML fragments of `search` messages.
    let mut result: Vec<SearchResult> = Vec::new();

    for content in messages
        .iter()
        .filter(|message| message.tag == "search")
        .filter_map(payload_content)
    {
        result.extend(parse_search_results_html(content));
    }

    result
}

/// The CSS selectors for parsing the search and image result fragments.
struct Selectors {
    /// A search result block.
    search_result: Selector,
    /// The title link of a search result block.
    title_link: Selector,
    /// The description of a search result block.
    description: Selector,
    /// An image result block.
    image_item: Selector,
    /// The thumbnail image of an image result block.
    image_thumbnail: Selector,
}

impl Selectors {
    /// Parses the result selectors.
    ///
    /// # Panics
    ///
    /// Panics if a selector is invalid; the selectors are compile-time constants.
    fn new() -> Self {
        Self {
            search_result: Selector::parse("div.search-result").expect("search result selector"),
            title_link: Selector::parse("h3.__sri-title-box > a.__sri_title_link")
                .expect("title link selector"),
            description: Selector::parse("div.__sri-desc > div").expect("description selector"),
            image_item: Selector::parse("div._0_img-results > div.item").expect("item selector"),
            image_thumbnail: Selector::parse("img._0_img_src").expect("thumbnail selector"),
        }
    }
}

/// The pre-compiled result selectors, initialized on first use.
static SELECTORS: LazyLock<Selectors> = LazyLock::new(Selectors::new);

fn parse_search_results_html(html: &str) -> Vec<SearchResult> {
    let fragment = Html::parse_fragment(html);
    let selectors = &SELECTORS;

    let search_results = fragment.select(&selectors.search_result);

    let mut results = Vec::new();

    for result_div in search_results {
        let title = result_div.select(&selectors.title_link).next();
        let description = result_div.select(&selectors.description).next();

        if let (Some(title), Some(description)) = (title, description) {
            let url = title.attr("href").unwrap_or("").to_string();

            results.push(SearchResult {
                title: collect_trimmed_text(&title),
                url,
                description: extract_topmost_text(&description),
            });
        }
    }

    results
}

pub(crate) fn parse_image_result_messages(messages: &[KagiMessage]) -> Vec<ImageResult> {
    let mut result: Vec<ImageResult> = Vec::new();

    for content in messages
        .iter()
        .filter(|message| message.tag == "images")
        .filter_map(payload_content)
    {
        result.extend(parse_image_results_html(content));
    }

    result
}

fn parse_image_results_html(html: &str) -> Vec<ImageResult> {
    let fragment = Html::parse_fragment(html);
    let selectors = &SELECTORS;

    let mut results = Vec::new();

    for item in fragment.select(&selectors.image_item) {
        let value = item.value();
        let (Some(title), Some(page_url), Some(image_url)) = (
            value.attr("data-title"),
            value.attr("data-host_url"),
            value.attr("data-content_url"),
        ) else {
            continue;
        };
        let Some(thumbnail) = item
            .select(&selectors.image_thumbnail)
            .next()
            .and_then(|img| img.attr("src"))
        else {
            continue;
        };

        results.push(ImageResult {
            title: htmlize::unescape(title).into_owned(),
            page_url: page_url.to_string(),
            image_url: image_url.to_string(),
            thumbnail_url: thumbnail.to_string(),
            width: value
                .attr("data-width")
                .and_then(|width| width.parse().ok())
                .unwrap_or(0),
            height: value
                .attr("data-height")
                .and_then(|height| height.parse().ok())
                .unwrap_or(0),
            host: value.attr("data-host").unwrap_or_default().to_string(),
            rank: value
                .attr("data-rank")
                .and_then(|rank| rank.parse().ok())
                .unwrap_or(0),
        });
    }

    results
}

/// Collects an element's text nodes into one allocation, trimmed.
fn collect_trimmed_text(elem: &ElementRef<'_>) -> String {
    let mut text = String::with_capacity(elem.text().map(str::len).sum());

    for node in elem.text() {
        text.push_str(node);
    }

    let start = text.len() - text.trim_start().len();
    let end = start + text[start..].trim_end().len();
    text.truncate(end);
    text.drain(..start);

    text
}

fn extract_topmost_text(elem: &ElementRef<'_>) -> String {
    let mut extracted = String::new();

    for node in elem.children() {
        let Node::Text(text_node) = node.value() else {
            continue;
        };
        let text = text_node.trim();

        if text.is_empty() {
            continue;
        }

        if !extracted.is_empty() {
            extracted.push(' ');
        }

        extracted.push_str(text);
    }

    extracted
}

#[cfg(test)]
mod tests {
    use test_support::kagi::read_fixture;

    use super::*;

    #[test]
    fn parses_the_legacy_stream_format() {
        let messages = parse_stream(&read_fixture("search_stream.bin"));

        assert_eq!(messages.len(), 8);
    }

    #[test]
    fn parses_structured_search_results() {
        let messages = parse_stream(&read_fixture("search_stream_sse.bin"));

        assert!(
            messages
                .iter()
                .any(|message| message.tag == "search_results_json")
        );
        assert!(messages.iter().any(|message| message.tag == "search"));
        assert!(messages.iter().any(|message| message.tag == "search.info"));

        let results = parse_search_result_messages(&messages);

        assert_eq!(results.len(), 5);
        assert_eq!(results[0].title, "Hello, world - Wikipedia");
        assert_eq!(results[0].url, "https://en.wikipedia.org/wiki/Hello,_world");
        // Snippets arrive HTML-entity encoded and must be decoded.
        assert!(results[0].description.contains('"'));
        assert!(!results[0].description.contains("&quot;"));
    }

    #[test]
    fn unescapes_structured_search_result_titles_and_snippets() {
        let payload = r#"{"items":[{"title":"Fish &amp; Chips","url":"https://example.com/fish","snippet":"A &quot;classic&quot; dish &amp; more"}]}"#;
        let messages = vec![KagiMessage {
            tag: "search_results_json".to_string(),
            payload: Some(Value::String(payload.to_string())),
        }];

        let results = parse_search_result_messages(&messages);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Fish & Chips");
        assert_eq!(results[0].description, "A \"classic\" dish & more");
    }

    #[test]
    fn falls_back_to_html_search_results() {
        let messages = parse_stream(&read_fixture("search_stream.bin"));
        let results = parse_search_result_messages(&messages);

        assert_eq!(results.len(), 19);
        assert_eq!(
            results[0].title,
            "Vitamin D - Health Professional Fact Sheet"
        );
        assert_eq!(
            results[0].description,
            "Vitamin D (also referred to as calciferol) is a fat-soluble vitamin that is naturally present in a few foods, added to others, and available as a dietary ..."
        );
        assert_eq!(
            results[0].url,
            "https://ods.od.nih.gov/factsheets/VitaminD-HealthProfessional/"
        );
    }

    #[test]
    fn parses_image_results() {
        let messages = parse_stream(&read_fixture("images_stream_sse.bin"));
        let results = parse_image_result_messages(&messages);

        assert_eq!(results.len(), 5);

        let result = &results[0];
        assert_eq!(
            result.title,
            "How to Do a Reverse Image Search From Your Phone"
        );
        assert_eq!(
            result.page_url,
            "https://www.entrepreneur.com/business-news/how-to-do-a-reverse-image-search-from-your-phone/297541"
        );
        assert_eq!(
            result.image_url,
            "https://assets.entrepreneur.com/images/misc/1500561136_1.jpg"
        );
        assert!(
            result
                .thumbnail_url
                .starts_with("https://p.kagi.com/proxy/")
        );
        assert_eq!(result.width, 740);
        assert_eq!(result.height, 475);
        assert_eq!(result.host, "www.entrepreneur.com");
        assert_eq!(result.rank, 0);
    }

    #[test]
    fn extracts_the_nonce_from_the_landing_page() {
        let nonce = extract_nonce(&read_fixture("landing.html")).expect("could not extract nonce");

        assert_eq!(nonce, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn extracts_nonces_with_surrounding_whitespace() {
        assert_eq!(
            extract_nonce("window.sse_nonce =\n\t\"abc\""),
            Some("abc".to_owned())
        );
        assert_eq!(extract_nonce("window.sse_nonce = \"\""), None);
        // A malformed marker does not hide a later valid one.
        assert_eq!(
            extract_nonce("window.sse_nonce_nope = \"x\"; window.sse_nonce = \"y\""),
            Some("y".to_owned())
        );
    }

    #[test]
    fn extracts_nonces_with_escaped_quotes() {
        // The scan continues past a backslash-escaped quote instead of truncating there.
        assert_eq!(
            extract_nonce(r#"window.sse_nonce = "abc\"def""#),
            Some(r#"abc\"def"#.to_owned())
        );
        assert_eq!(
            extract_nonce(r#"window.sse_nonce = "abc\";def";"#),
            Some(r#"abc\";def"#.to_owned())
        );
    }

    #[test]
    fn skips_malformed_and_empty_nonces() {
        // A marker without an assignment does not hide a later valid one.
        assert_eq!(
            extract_nonce("window.sse_nonce = 42; window.sse_nonce = \"later\""),
            Some("later".to_owned())
        );
        // An empty nonce is ignored in favor of a later assignment.
        assert_eq!(
            extract_nonce("window.sse_nonce = \"\"; window.sse_nonce = \"later\""),
            Some("later".to_owned())
        );
        // An unterminated literal and a page without the marker yield no nonce.
        assert_eq!(extract_nonce("window.sse_nonce = \"unterminated"), None);
        assert_eq!(extract_nonce("no nonce here"), None);
    }

    #[test]
    fn extracts_nonces_with_whitespace_around_the_assignment() {
        assert_eq!(
            extract_nonce("window.sse_nonce\t=\n  \"abc\""),
            Some("abc".to_owned())
        );
    }
}
