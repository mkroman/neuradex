//! Streaming of response bodies through the document head parser.
//!
//! The HTML tokenizer is not `Send`, so the response body is streamed through a bounded channel
//! and parsed on a blocking thread. The channel is bounded so that the reader stalls — and the
//! download is throttled — once the parser falls behind. In head-only mode, parsing stops as
//! soon as the document head has been received, aborting the rest of the download; in full-body
//! mode, the parser stops capturing at the head but the body keeps streaming up to its cap.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use axum::body::Bytes;
use futures::StreamExt;
use tokio::sync::mpsc;
use wreq::header::HeaderMap;

use crate::api::v1::error::ApiError;
use crate::metadata::{HeadParser, PageMetadata};

/// The maximum size of a response consumed while extracting document head metadata.
pub(crate) const HEAD_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The media types that can be returned as text.
const TEXT_MEDIA_TYPES: &[&str] = &[
    "application/atom+xml",
    "application/ecmascript",
    "application/graphql",
    "application/javascript",
    "application/json",
    "application/ld+json",
    "application/rss+xml",
    "application/toml",
    "application/x-javascript",
    "application/x-www-form-urlencoded",
    "application/xhtml+xml",
    "application/xml",
    "application/yaml",
];

/// How a response body should be consumed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum BodyMode {
    /// Stop as soon as the document head has been parsed.
    Head,
    /// Read the full body, up to a maximum number of bytes.
    Full(u64),
}

/// The outcome of streaming a response through the head parser.
pub(crate) struct StreamedBody {
    /// The metadata captured from the document head.
    pub metadata: PageMetadata,
    /// The body bytes read, when collecting the full body.
    pub body: Option<Vec<u8>>,
    /// The number of body bytes read (decompressed).
    pub bytes_read: u64,
    /// Whether the read was cut short by a size limit.
    pub truncated: bool,
}

/// Streams `response` through the head parser according to `mode`.
///
/// In [`BodyMode::Full`] the head parser stops capturing once the document head ends, but the
/// body keeps streaming up to its cap.
///
/// # Errors
///
/// Returns an error when the response body cannot be read or the parsing task fails.
pub(crate) async fn read_body(
    response: wreq::Response,
    mode: BodyMode,
) -> Result<StreamedBody, ApiError> {
    let (collect_body, max_bytes) = match mode {
        BodyMode::Head => (false, HEAD_MAX_BYTES),
        BodyMode::Full(max_bytes) => (true, max_bytes),
    };

    let (sender, mut receiver) = mpsc::channel::<Result<Bytes, wreq::Error>>(4);

    let reader = tokio::spawn(async move {
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            // Backpressure from a full channel awaits here instead of blocking a worker thread.
            if sender.send(chunk).await.is_err() {
                break;
            }
        }
    });

    let read = tokio::task::spawn_blocking(move || {
        let chunks = std::iter::from_fn(|| receiver.blocking_recv());

        consume(chunks, collect_body, max_bytes)
    })
    .await;

    // The parser is done reading, so the reader has no remaining consumer. Abort it even on
    // success: it may be parked in `bytes_stream().next()`, which dropping the receiver does
    // not interrupt, and a stalled upstream must not hold the response until the client
    // timeout. On a failed read this also prevents a stalled body from delaying the error.
    reader.abort();

    if let Err(error) = reader.await
        && !error.is_cancelled()
    {
        // A failing read already becomes an API error and is logged once at error level;
        // only a reader failure on an otherwise successful read needs a warning.
        if matches!(&read, Ok(Ok(_))) {
            tracing::warn!(
                "error.type" = "reader_task",
                "exception.message" = %error,
                "reader task failed"
            );
        } else {
            tracing::debug!(
                "error.type" = "reader_task",
                "exception.message" = %error,
                "reader task failed while the request was already failing"
            );
        }
    }

    read?
}

/// Consumes `chunks` through the head parser, collecting the body when `collect_body` is set.
///
/// # Errors
///
/// Returns an error when a chunk failed to read from the upstream response.
fn consume(
    chunks: impl Iterator<Item = Result<Bytes, wreq::Error>>,
    collect_body: bool,
    max_bytes: u64,
) -> Result<StreamedBody, ApiError> {
    let mut parser = HeadParser::new();
    let mut body = if collect_body { Some(Vec::new()) } else { None };
    let mut bytes_read: u64 = 0;
    let mut truncated = false;
    let cap = usize::try_from(max_bytes).unwrap_or(usize::MAX);

    // Whether the size limit was just reached. The loop keeps reading: a further chunk proves
    // the response was cut short, while a clean end of stream proves it fit exactly and should
    // not be reported as truncated.
    let mut at_limit = false;

    for chunk in chunks {
        if at_limit {
            truncated = true;

            break;
        }

        let chunk = chunk.map_err(|error| {
            // `wreq::Error`'s renderings include the request URI; redact it before it can reach
            // the 5xx log line.
            ApiError::upstream(error.without_uri())
        })?;
        bytes_read = bytes_read.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));

        parser.feed(&chunk);

        if let Some(body) = body.as_mut()
            && body.len() < cap
        {
            let remaining = cap - body.len();
            let take = remaining.min(chunk.len());
            body.extend_from_slice(&chunk[..take]);

            // The final chunk crossed the cap: bytes were cut even though the loop ends before
            // the next `at_limit` check.
            if take < chunk.len() {
                truncated = true;
            }
        }

        if body.as_ref().is_some_and(|body| body.len() >= cap) {
            at_limit = true;

            continue;
        }

        // Only the head-only mode can stop at the end of the head; the full mode keeps reading
        // the body until its cap or the end of the response.
        if !collect_body && parser.is_done() {
            break;
        }

        if !collect_body && bytes_read >= max_bytes {
            at_limit = true;
        }
    }

    parser.finish();

    Ok(StreamedBody {
        metadata: parser.into_metadata(),
        body,
        bytes_read,
        truncated,
    })
}

/// Whether the media type can be returned as text.
#[must_use]
fn is_text_media_type(media: &str) -> bool {
    media.starts_with("text/")
        || TEXT_MEDIA_TYPES.contains(&media)
        || media.ends_with("+json")
        || media.ends_with("+xml")
}

/// Rejects responses whose content type is binary, before their body is read.
///
/// `content_type` is the parameter-free, lowercased media type captured from the final response.
/// Responses without one are allowed: too many servers omit the header for the response to be
/// discarded.
///
/// # Errors
///
/// Returns [`ApiError::UnsupportedMediaType`] for binary content types.
pub(crate) fn ensure_text_content(content_type: Option<&str>) -> Result<(), ApiError> {
    let Some(media) = content_type else {
        return Ok(());
    };

    if media.is_empty() || is_text_media_type(media) {
        return Ok(());
    }

    Err(ApiError::unsupported_media_type(format!(
        "refusing to fetch non-text content: {media}"
    )))
}

/// Converts a header map into a JSON-friendly map, joining repeated values with `, `.
#[must_use]
pub(crate) fn headers_to_json(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();

    for (name, value) in headers {
        let name = name.as_str().to_owned();
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();

        match map.entry(name) {
            Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                existing.push_str(", ");
                existing.push_str(&value);
            }
            Entry::Vacant(entry) => {
                entry.insert(value);
            }
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use wreq::header::CONTENT_LENGTH;

    use super::*;

    #[test]
    fn classifies_text_media_types() {
        assert!(is_text_media_type("text/html"));
        assert!(is_text_media_type("text/plain"));
        assert!(is_text_media_type("application/javascript"));
        assert!(is_text_media_type("application/json"));
        assert!(is_text_media_type("application/vnd.api+json"));
        assert!(is_text_media_type("application/rss+xml"));

        assert!(!is_text_media_type("image/png"));
        assert!(!is_text_media_type("video/mp4"));
        assert!(!is_text_media_type("application/octet-stream"));
        assert!(!is_text_media_type("application/pdf"));
        assert!(!is_text_media_type("application/zip"));
    }

    #[test]
    fn rejects_binary_content_types() {
        assert!(ensure_text_content(Some("text/html")).is_ok());
        assert!(ensure_text_content(Some("application/javascript")).is_ok());
        assert!(ensure_text_content(None).is_ok());
        assert!(ensure_text_content(Some("")).is_ok());
        assert_matches!(
            ensure_text_content(Some("image/png")),
            Err(ApiError::UnsupportedMediaType(_))
        );
    }

    #[test]
    fn joins_repeated_header_values() {
        let mut headers = HeaderMap::new();
        headers.insert("set-cookie", "a=1".parse().expect("a valid header value"));
        headers.append("set-cookie", "b=2".parse().expect("a valid header value"));
        headers.insert(CONTENT_LENGTH, "42".parse().expect("a valid header value"));

        let json = headers_to_json(&headers);

        assert_eq!(json.get("set-cookie").map(String::as_str), Some("a=1, b=2"));
        assert_eq!(json.get("content-length").map(String::as_str), Some("42"));
    }

    /// Runs `consume` over chunks copied from `chunks`.
    fn read_chunks(chunks: &[&[u8]], collect_body: bool, max_bytes: u64) -> StreamedBody {
        consume(
            chunks.iter().map(|chunk| Ok(Bytes::copy_from_slice(chunk))),
            collect_body,
            max_bytes,
        )
        .expect("reads")
    }

    #[test]
    fn reads_full_bodies_up_to_the_cap() {
        struct Case {
            name: &'static str,
            chunks: &'static [&'static [u8]],
            cap: u64,
            bytes_read: u64,
            body: &'static [u8],
            truncated: bool,
        }

        let cases = [
            Case {
                name: "a body that fits the cap exactly is complete",
                chunks: &[b"xxxxxxxx"],
                cap: 8,
                bytes_read: 8,
                body: b"xxxxxxxx",
                truncated: false,
            },
            Case {
                name: "a further chunk proves the body was cut short",
                chunks: &[b"xxxxxxxx", b"yyyy"],
                cap: 8,
                bytes_read: 8,
                body: b"xxxxxxxx",
                truncated: true,
            },
            Case {
                name: "a single chunk crossing the cap is truncated",
                chunks: &[b"xxxxxxxxxxxx"],
                cap: 8,
                bytes_read: 12,
                body: b"xxxxxxxx",
                truncated: true,
            },
            Case {
                name: "a final chunk crossing the cap is truncated",
                chunks: &[b"xxxx", b"yyyyyyyy"],
                cap: 8,
                bytes_read: 12,
                body: b"xxxxyyyy",
                truncated: true,
            },
        ];

        for case in cases {
            let read = read_chunks(case.chunks, true, case.cap);

            assert_eq!(
                read.bytes_read, case.bytes_read,
                "bytes_read for {}",
                case.name
            );
            assert_eq!(
                read.body.as_deref(),
                Some(case.body),
                "body for {}",
                case.name
            );
            assert_eq!(
                read.truncated, case.truncated,
                "truncated for {}",
                case.name
            );
        }
    }

    #[test]
    fn reads_heads_up_to_the_head_or_the_cap() {
        struct Case {
            name: &'static str,
            chunks: &'static [&'static [u8]],
            collect_body: bool,
            max_bytes: u64,
            bytes_read: u64,
            body: Option<&'static [u8]>,
            truncated: bool,
            title: Option<&'static str>,
        }

        const HTML: &[u8] = b"<html><head><title>Hello</title></head></html>";
        const HEAD: &[u8] = b"<html><head><title>Hello</title></head>";
        const TAIL: &[u8] = b"<body><p>Hello</p></body></html>";
        const FULL: &[u8] =
            b"<html><head><title>Hello</title></head><body><p>Hello</p></body></html>";

        let cases = [
            Case {
                name: "a head that fits the cap exactly parses completely",
                chunks: &[HTML],
                collect_body: false,
                max_bytes: u64::try_from(HTML.len()).expect("fits"),
                bytes_read: u64::try_from(HTML.len()).expect("fits"),
                body: None,
                truncated: false,
                title: Some("Hello"),
            },
            Case {
                name: "a further chunk proves the head was cut short",
                chunks: &[b"<<<<<<<<<<<<<<<<", b"xxxxxxxx"],
                collect_body: false,
                max_bytes: 16,
                bytes_read: 16,
                body: None,
                truncated: true,
                title: None,
            },
            Case {
                name: "full mode keeps reading past the head",
                chunks: &[HEAD, TAIL],
                collect_body: true,
                max_bytes: HEAD_MAX_BYTES,
                bytes_read: u64::try_from(FULL.len()).expect("fits"),
                body: Some(FULL),
                truncated: false,
                title: Some("Hello"),
            },
            Case {
                name: "head mode stops at the end of the head",
                chunks: &[HEAD, b"xxxxxxxx"],
                collect_body: false,
                max_bytes: HEAD_MAX_BYTES,
                bytes_read: u64::try_from(HEAD.len()).expect("fits"),
                body: None,
                truncated: false,
                title: Some("Hello"),
            },
        ];

        for case in cases {
            let read = read_chunks(case.chunks, case.collect_body, case.max_bytes);

            assert_eq!(
                read.metadata.title.as_deref(),
                case.title,
                "title for {}",
                case.name
            );
            assert_eq!(
                read.bytes_read, case.bytes_read,
                "bytes_read for {}",
                case.name
            );
            assert_eq!(read.body.as_deref(), case.body, "body for {}", case.name);
            assert_eq!(
                read.truncated, case.truncated,
                "truncated for {}",
                case.name
            );
        }
    }
}
