//! Page metadata extraction from document heads.
//!
//! Responses are streamed through a push-based HTML tokenizer that captures the title and the
//! meta/link metadata of a document head. Tokenization stops once the head section has ended —
//! through `</head>` or the start of `<body>` — so the rest of the response can be aborted.
//! The tokenizer unwinds mid-buffer at that tag; any body markup that shares the chunk with the
//! head boundary is discarded without being tokenized.
//!
//! Response bodies are decoded as UTF-8; byte sequences that are not valid UTF-8 — including
//! entire pages served in another encoding, e.g. ISO-8859-1 — are decoded lossily.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, EndTag, StartTag, Tag, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use serde::Serialize;

/// The prefix of the OpenGraph meta properties.
const OG_PREFIX: &str = "og:";

/// The prefix of the Twitter Card meta names.
const TWITTER_PREFIX: &str = "twitter:";

/// Page metadata extracted from a document head.
///
/// All meta tags are captured generically — keyed by their `property` or `name` attribute — so
/// additional extractors can be layered on without extending the tokenizer.
#[derive(Debug, Default, Serialize, utoipa::ToSchema)]
#[schema(examples(json!({
    "title": "Maero",
    "canonical": "https://example.com/",
    "description": "A small API service implementing tools for LLM agents.",
    "og": {"site_name": "Maero", "title": "Hello"},
    "twitter": {"card": "summary"},
    "other": {"viewport": "width=device-width"}
})))]
pub struct PageMetadata {
    /// The contents of the first `<title>` element.
    pub title: Option<String>,
    /// The `href` of the first `<link rel="canonical">` element.
    #[schema(format = "uri")]
    pub canonical: Option<String>,
    /// The `content` of the first `<meta name="description">` tag.
    pub description: Option<String>,
    /// The OpenGraph meta properties, without the `og:` prefix.
    pub og: BTreeMap<String, String>,
    /// The Twitter Card meta names, without the `twitter:` prefix.
    pub twitter: BTreeMap<String, String>,
    /// All other named or proprietary meta tags, keyed by their property or name.
    pub other: BTreeMap<String, String>,
}

/// A streaming parser that captures the metadata of a document head.
pub struct HeadParser {
    /// The HTML tokenizer driving the metadata sink.
    tokenizer: Tokenizer<HeadSink>,
    /// The buffer queue the tokenizer reads from.
    queue: BufferQueue,
    /// Bytes of an incomplete UTF-8 sequence carried over from the previous chunk.
    pending: Vec<u8>,
}

impl Default for HeadParser {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadParser {
    /// Constructs a new, empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tokenizer: Tokenizer::new(HeadSink::new(), TokenizerOpts::default()),
            queue: BufferQueue::default(),
            pending: Vec::new(),
        }
    }

    /// Feeds a chunk of response bytes into the parser.
    ///
    /// Incomplete multi-byte UTF-8 sequences are carried over to the next chunk; invalid
    /// sequences are replaced with `U+FFFD`.
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.is_done() {
            return;
        }

        // The common case: a self-contained, valid UTF-8 chunk needs neither decoding nor
        // carry-over, so it can go straight to the tokenizer.
        if self.pending.is_empty()
            && let Ok(text) = std::str::from_utf8(chunk)
        {
            if !text.is_empty() {
                self.queue.push_back(StrTendril::from_slice(text));
                let _ = self.tokenizer.feed(&self.queue);
                self.release_buffered_input();
            }

            return;
        }

        let text = decode_chunk(&mut self.pending, chunk);

        if !text.is_empty() {
            self.queue.push_back(StrTendril::from_slice(&text));
            let _ = self.tokenizer.feed(&self.queue);
            self.release_buffered_input();
        }
    }

    /// Releases the input the tokenizer left queued when it stopped at the head boundary.
    ///
    /// The tokenizer unwinds at the tag that ends the head, so the remainder of the chunk stays
    /// in the queue. Dropping it promptly matters for callers that keep the parser alive to
    /// collect the body.
    fn release_buffered_input(&self) {
        if self.is_done() {
            while self.queue.pop_front().is_some() {}
        }
    }

    /// Flushes any bytes carried over from the previous chunk through the parser.
    pub fn finish(&mut self) {
        if self.is_done() {
            return;
        }

        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            let text = String::from_utf8_lossy(&pending);

            if !text.is_empty() {
                self.queue.push_back(StrTendril::from_slice(&text));
                let _ = self.tokenizer.feed(&self.queue);
            }
        }

        // Signal the end of the input so the tokenizer flushes its state and the sink can commit
        // a title that was still open when the input ended.
        self.tokenizer.end();
    }

    /// Whether the head section has ended and parsing can stop.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.tokenizer.sink.is_done()
    }

    /// Returns the metadata captured so far.
    #[must_use]
    pub fn into_metadata(self) -> PageMetadata {
        self.tokenizer.sink.into_metadata()
    }
}

/// Converts `chunk` to UTF-8 text for the parser, carrying incomplete multi-byte sequences in
/// `pending` over to the next chunk.
///
/// Invalid sequences are replaced with `U+FFFD`, as if the bytes were decoded lossily.
#[must_use]
fn decode_chunk(pending: &mut Vec<u8>, chunk: &[u8]) -> String {
    pending.extend_from_slice(chunk);
    let mut text = String::with_capacity(pending.len());
    let mut consumed = 0;

    while consumed < pending.len() {
        match std::str::from_utf8(&pending[consumed..]) {
            Ok(valid) => {
                text.push_str(valid);
                consumed = pending.len();
            }
            Err(error) => {
                let valid = error.valid_up_to();

                // The prefix is valid UTF-8 by construction.
                if let Ok(prefix) = std::str::from_utf8(&pending[consumed..consumed + valid]) {
                    text.push_str(prefix);
                }

                match error.error_len() {
                    // An incomplete sequence at the end of the chunk is carried over as-is.
                    None => {
                        consumed += valid;

                        break;
                    }
                    Some(invalid_len) => {
                        text.push('\u{fffd}');
                        consumed += valid + invalid_len;
                    }
                }
            }
        }
    }

    pending.drain(..consumed);
    text
}

/// Token sink that captures the title and metadata of a document head.
///
/// Tokenization is done — see [`HeadSink::is_done`] — once the head section has ended, either
/// through `</head>` or the start of `<body>`.
struct HeadSink {
    /// The metadata captured so far.
    metadata: RefCell<PageMetadata>,
    /// The buffer for the contents of the current `<title>` element, when inside one.
    title: RefCell<Option<String>>,
    /// Whether the head section has ended and tokenization can stop.
    done: Cell<bool>,
}

impl HeadSink {
    /// Constructs a new, empty sink.
    fn new() -> Self {
        Self {
            metadata: RefCell::new(PageMetadata::default()),
            title: RefCell::new(None),
            done: Cell::new(false),
        }
    }

    /// Whether the head section has ended and tokenization can stop.
    const fn is_done(&self) -> bool {
        self.done.get()
    }

    /// Returns the metadata captured so far.
    fn into_metadata(self) -> PageMetadata {
        self.metadata.into_inner()
    }

    /// Commits the title collected so far, if any.
    fn flush_title(&self) {
        if let Some(title) = self.title.borrow_mut().take() {
            let title = title.trim();

            if !title.is_empty() {
                self.metadata.borrow_mut().title = Some(title.to_owned());
            }
        }
    }

    /// Processes a start tag.
    fn start_tag(&self, tag: &Tag) {
        match &*tag.name {
            "title" => {
                let mut title = self.title.borrow_mut();

                // Only the first title element is captured.
                if title.is_none() && self.metadata.borrow().title.is_none() {
                    *title = Some(String::new());
                }
            }
            "meta" => self.capture_meta(tag),
            "link" => self.capture_link(tag),
            "body" => self.done.set(true),
            _ => {}
        }
    }

    /// Processes an end tag.
    fn end_tag(&self, tag: &Tag) {
        match &*tag.name {
            "title" => self.flush_title(),
            "head" => self.done.set(true),
            _ => {}
        }
    }

    /// Captures the `content` of a meta tag under its property or name.
    fn capture_meta(&self, tag: &Tag) {
        let mut property = None;
        let mut name = None;
        let mut content = None;

        for attribute in &tag.attrs {
            match &*attribute.name.local {
                "property" => {
                    property.get_or_insert(&*attribute.value);
                }
                "name" => {
                    name.get_or_insert(&*attribute.value);
                }
                "content" => {
                    content.get_or_insert(&*attribute.value);
                }
                _ => {}
            }
        }

        let Some(key) = property.or(name) else {
            return;
        };

        let Some(content) = content.map(str::trim).filter(|content| !content.is_empty()) else {
            return;
        };

        let mut metadata = self.metadata.borrow_mut();

        if let Some(key) = key.strip_prefix(OG_PREFIX) {
            metadata
                .og
                .entry(key.to_owned())
                .or_insert_with(|| content.to_owned());
        } else if let Some(key) = key.strip_prefix(TWITTER_PREFIX) {
            metadata
                .twitter
                .entry(key.to_owned())
                .or_insert_with(|| content.to_owned());
        } else {
            metadata
                .other
                .entry(key.to_owned())
                .or_insert_with(|| content.to_owned());

            if key == "description" {
                metadata
                    .description
                    .get_or_insert_with(|| content.to_owned());
            }
        }
    }

    /// Captures the `href` of the first `<link rel="canonical">` element.
    fn capture_link(&self, tag: &Tag) {
        let mut rel = None;
        let mut href = None;

        for attribute in &tag.attrs {
            match &*attribute.name.local {
                "rel" => {
                    rel.get_or_insert(&*attribute.value);
                }
                "href" => {
                    href.get_or_insert(&*attribute.value);
                }
                _ => {}
            }
        }

        let Some(rel) = rel.map(str::trim) else {
            return;
        };

        let Some(href) = href.map(str::trim).filter(|href| !href.is_empty()) else {
            return;
        };

        if rel
            .split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case("canonical"))
        {
            self.metadata
                .borrow_mut()
                .canonical
                .get_or_insert_with(|| href.to_owned());
        }
    }
}

impl TokenSink for HeadSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<Self::Handle> {
        if self.is_done() {
            return TokenSinkResult::Continue;
        }

        match token {
            Token::TagToken(tag) => {
                match tag.kind {
                    StartTag => self.start_tag(&tag),
                    EndTag => self.end_tag(&tag),
                }

                // `TokenSinkResult` has no `Stop`, so unwind the tokenizer with `Script` once the
                // tag ends the head. This is not a real script execution: `HeadParser` never
                // resumes tokenization after `is_done()`, so the tokenizer's post-`Script` state
                // reset is unobservable. It stops the tokenizer mid-buffer instead of letting it
                // tokenize the remainder of a potentially large chunk.
                if self.is_done() {
                    return TokenSinkResult::Script(());
                }

                // Elements whose contents are text rather than markup must be switched to the
                // tokenizer's corresponding raw-text state. Otherwise markup-like text in a
                // script or style body — e.g. the string `"</head>"` — would be tokenized as a
                // real tag and stop the parser before the actual head end.
                if tag.kind == StartTag {
                    return raw_text_state(&tag.name);
                }
            }
            Token::CharacterTokens(text) => {
                if let Some(title) = self.title.borrow_mut().as_mut() {
                    title.push_str(&text);
                }
            }
            _ => {}
        }

        TokenSinkResult::Continue
    }

    fn end(&self) {
        self.flush_title();
    }
}

/// Returns the tokenizer's raw-text state for a start tag, if the element's contents are text
/// rather than markup.
fn raw_text_state(name: &str) -> TokenSinkResult<()> {
    use html5ever::tokenizer::states::{Rawtext, Rcdata, ScriptData};

    match name {
        "title" | "textarea" => TokenSinkResult::RawData(Rcdata),
        "style" | "xmp" | "iframe" | "noembed" | "noframes" => TokenSinkResult::RawData(Rawtext),
        "script" => TokenSinkResult::RawData(ScriptData),
        _ => TokenSinkResult::Continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses `html` in a single chunk and returns the captured metadata.
    fn parse_metadata(html: &str) -> PageMetadata {
        parse_metadata_chunked(html, usize::MAX)
    }

    /// Parses `html` in chunks of `chunk_size` characters and returns the captured metadata.
    fn parse_metadata_chunked(html: &str, chunk_size: usize) -> PageMetadata {
        let mut parser = HeadParser::new();
        let chars: Vec<char> = html.chars().collect();

        for chunk in chars.chunks(chunk_size.max(1)) {
            let text: String = chunk.iter().collect();
            parser.feed(text.as_bytes());
        }

        parser.finish();
        parser.into_metadata()
    }

    #[test]
    fn extracts_title() {
        let metadata = parse_metadata(
            "<!DOCTYPE html><html><head><title>Hello world</title></head><body></body></html>",
        );

        assert_eq!(metadata.title.as_deref(), Some("Hello world"));
    }

    #[test]
    fn trims_the_title() {
        let metadata = parse_metadata("<html><head><title>\n  Hello  \n</title></head></html>");

        assert_eq!(metadata.title.as_deref(), Some("Hello"));
    }

    #[test]
    fn captures_a_title_that_is_still_open_at_end_of_input() {
        let metadata = parse_metadata("<html><head><title>Hello");

        assert_eq!(metadata.title.as_deref(), Some("Hello"));
    }

    #[test]
    fn captures_a_title_split_across_chunks_at_end_of_input() {
        let mut parser = HeadParser::new();

        parser.feed(b"<html><head><title>Hel");
        parser.feed(b"lo");
        parser.finish();

        assert_eq!(parser.into_metadata().title.as_deref(), Some("Hello"));
    }

    #[test]
    fn extracts_open_graph_metadata() {
        let metadata = parse_metadata(
            r#"<head><meta property="og:site_name" content="Maero">
            <meta property="og:title" content="Hello">
            <meta property="og:description" content="A description">
            <title>Ignored</title></head>"#,
        );

        assert_eq!(
            metadata.og.get("site_name").map(String::as_str),
            Some("Maero")
        );
        assert_eq!(metadata.og.get("title").map(String::as_str), Some("Hello"));
        assert_eq!(
            metadata.og.get("description").map(String::as_str),
            Some("A description")
        );
        assert!(!metadata.og.is_empty());
        assert_eq!(metadata.title.as_deref(), Some("Ignored"));
    }

    #[test]
    fn extracts_open_graph_metadata_by_name() {
        let metadata =
            parse_metadata(r#"<head><meta name="og:description" content="A description"></head>"#);

        assert_eq!(
            metadata.og.get("description").map(String::as_str),
            Some("A description")
        );
    }

    #[test]
    fn extracts_twitter_metadata() {
        let metadata = parse_metadata(
            r#"<head><meta name="twitter:card" content="summary">
            <meta name="twitter:title" content="Hello"></head>"#,
        );

        assert_eq!(
            metadata.twitter.get("card").map(String::as_str),
            Some("summary")
        );
        assert_eq!(
            metadata.twitter.get("title").map(String::as_str),
            Some("Hello")
        );
    }

    #[test]
    fn captures_other_meta_tags() {
        let metadata = parse_metadata(
            r#"<head><meta name="description" content="A description">
            <meta name="keywords" content="a, b">
            <meta name="viewport" content="width=device-width"></head>"#,
        );

        assert_eq!(metadata.description.as_deref(), Some("A description"));
        assert_eq!(
            metadata.other.get("description").map(String::as_str),
            Some("A description")
        );
        assert_eq!(
            metadata.other.get("keywords").map(String::as_str),
            Some("a, b")
        );
        assert_eq!(
            metadata.other.get("viewport").map(String::as_str),
            Some("width=device-width")
        );
    }

    #[test]
    fn extracts_canonical_link() {
        let metadata = parse_metadata(
            r#"<head><link rel="canonical" href="https://example.com/page">
            <link rel="stylesheet" href="style.css"></head>"#,
        );

        assert_eq!(
            metadata.canonical.as_deref(),
            Some("https://example.com/page")
        );
    }

    #[test]
    fn recognizes_canonical_in_a_rel_token_list() {
        let metadata = parse_metadata(
            r#"<head><link rel="nofollow canonical" href="https://example.com/page"></head>"#,
        );

        assert_eq!(
            metadata.canonical.as_deref(),
            Some("https://example.com/page")
        );
    }

    #[test]
    fn keeps_only_the_first_of_each_open_graph_field() {
        let metadata = parse_metadata(
            r#"<head><meta property="og:title" content="First">
            <meta property="og:title" content="Second"></head>"#,
        );

        assert_eq!(metadata.og.get("title").map(String::as_str), Some("First"));
    }

    #[test]
    fn ignores_empty_open_graph_content() {
        let metadata =
            parse_metadata(r#"<head><meta property="og:description" content=" "> </head>"#);

        assert!(metadata.og.is_empty());
    }

    #[test]
    fn decodes_entities() {
        let metadata = parse_metadata(
            r#"<head><title>Fish &amp; Chips</title>
            <meta property="og:description" content="a &quot;quoted&quot; word"></head>"#,
        );

        assert_eq!(metadata.title.as_deref(), Some("Fish & Chips"));
        assert_eq!(
            metadata.og.get("description").map(String::as_str),
            Some("a \"quoted\" word")
        );
    }

    #[test]
    fn stops_at_the_end_of_head() {
        let metadata = parse_metadata(
            "<html><head><title>Hello</title></head><body><title>Ignored</title></body></html>",
        );

        assert_eq!(metadata.title.as_deref(), Some("Hello"));
    }

    #[test]
    fn stops_at_the_start_of_body() {
        let metadata = parse_metadata(
            "<html><head><title>Hello</title><body><title>Ignored</title></body></html>",
        );

        assert_eq!(metadata.title.as_deref(), Some("Hello"));
    }

    #[test]
    fn ignores_head_end_inside_a_script() {
        let html = r#"<html><head><title>Hello</title>
            <script>const closing = "</head><body>";</script>
            <meta name="description" content="A description"></head>
            <body><title>Ignored</title></body></html>"#;

        for chunk_size in [1, 2, 3, 5, 7, 11, 64, usize::MAX] {
            let metadata = parse_metadata_chunked(html, chunk_size);

            assert_eq!(metadata.title.as_deref(), Some("Hello"));
            assert_eq!(
                metadata.description.as_deref(),
                Some("A description"),
                "chunk size {chunk_size}"
            );
        }
    }

    #[test]
    fn ignores_metadata_after_the_head() {
        let metadata = parse_metadata(
            r#"<html><head><title>Hello</title>
            <meta name="description" content="Head description"></head>
            <body><title>Ignored</title>
            <meta property="og:title" content="Ignored">
            <meta name="description" content="Body description">
            <link rel="canonical" href="https://example.com/body"></body></html>"#,
        );

        assert_eq!(metadata.title.as_deref(), Some("Hello"));
        assert_eq!(metadata.description.as_deref(), Some("Head description"));
        assert!(metadata.og.is_empty());
        assert!(metadata.canonical.is_none());
    }

    #[test]
    fn releases_the_unparsed_tail_of_the_chunk() {
        let mut parser = HeadParser::new();

        // The body tail shares a chunk with `</head>` and is never tokenized.
        parser.feed(
            b"<html><head><title>Hello</title></head><body><title>Ignored</title></body></html>",
        );

        assert!(parser.is_done());
        assert!(parser.queue.is_empty());

        parser.feed(b"<title>Ignored</title>");
        parser.finish();

        assert!(parser.queue.is_empty());
        assert_eq!(parser.into_metadata().title.as_deref(), Some("Hello"));
    }

    #[test]
    fn captures_title_across_chunks() {
        let html = r#"<head><title>Fish &amp; Chips</title><meta property="og:description" content="A description"></head>"#;

        for chunk_size in [1, 2, 3, 5, 7, 11, 64] {
            let metadata = parse_metadata_chunked(html, chunk_size);

            assert_eq!(metadata.title.as_deref(), Some("Fish & Chips"));
            assert_eq!(
                metadata.og.get("description").map(String::as_str),
                Some("A description"),
                "chunk size {chunk_size}"
            );
        }
    }

    #[test]
    fn carries_split_utf8_sequences_across_chunks() {
        let bytes = "a—b".as_bytes(); // em-dash is three bytes
        let mut pending = Vec::new();

        assert_eq!(decode_chunk(&mut pending, &bytes[..2]), "a");
        assert_eq!(decode_chunk(&mut pending, &bytes[2..]), "—b");
        assert_eq!(pending, Vec::<u8>::new());
    }

    #[test]
    fn replaces_invalid_sequences() {
        let mut pending = Vec::new();

        assert_eq!(decode_chunk(&mut pending, &[0xff, b'a']), "\u{fffd}a");
        assert_eq!(decode_chunk(&mut pending, b"b"), "b");
        assert_eq!(decode_chunk(&mut pending, "æ".as_bytes()), "æ");
        assert_eq!(pending, Vec::<u8>::new());
    }

    #[test]
    fn replaces_long_invalid_runs() {
        let mut pending = Vec::new();
        let bytes = vec![0xff; 64 * 1024];

        let text = decode_chunk(&mut pending, &bytes);

        assert_eq!(text.chars().count(), bytes.len());
        assert!(text.chars().all(|character| character == '\u{fffd}'));
        assert_eq!(pending, Vec::<u8>::new());
    }

    #[test]
    fn stops_feeding_after_done() {
        let mut parser = HeadParser::new();
        parser.feed(b"<html><head><title>Hello</title></head>");
        assert!(parser.is_done());

        parser.feed(b"<body><title>Ignored</title></body>");
        parser.finish();

        assert_eq!(parser.into_metadata().title.as_deref(), Some("Hello"));
    }
}
