//! Streaming of response bodies through the document head parser.
//!
//! The HTML tokenizer is not `Send`, so the response body is streamed through a bounded channel
//! and parsed by the parser pool. The channel is bounded so that the reader stalls — and the
//! download is throttled — once the parser falls behind. In head-only mode, parsing stops as
//! soon as the document head has been received, aborting the rest of the download; in full-body
//! mode, the parser stops capturing at the head but the body keeps streaming up to its cap.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use axum::body::Bytes;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use wreq::header::HeaderMap;

use crate::api::v1::error::ApiError;
use crate::api::v1::parser_pool;
use crate::metadata::PageMetadata;

/// The maximum size of a response consumed while extracting document head metadata.
pub(crate) const HEAD_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The largest body buffer reserved from a `Content-Length` hint.
///
/// `Content-Length` may describe compressed bytes and is controlled by the upstream, so it is
/// only a hint: reserve at most this much up front and let the buffer grow past it when needed.
const MAX_RESERVED_BODY_BYTES: u64 = 1024 * 1024;

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

/// The channel capacity for each body mode.
///
/// Head-only mode stops at the end of the head, so a single buffered chunk already keeps the
/// parser fed while bounding how much of the body a `peek` prefetches past the head. Full mode
/// needs a deeper channel to keep the parser busy while the async reader is scheduled.
const fn channel_capacity(mode: BodyMode) -> usize {
    match mode {
        BodyMode::Head => 1,
        BodyMode::Full(_) => 4,
    }
}

/// Returns the body buffer capacity to reserve from a `Content-Length` hint.
///
/// The hint is clamped to both the body cap and [`MAX_RESERVED_BODY_BYTES`] because it can
/// describe compressed bytes or lie outright.
fn initial_body_capacity(content_length: Option<u64>, max_bytes: u64) -> usize {
    let hint = content_length
        .unwrap_or(0)
        .min(max_bytes)
        .min(MAX_RESERVED_BODY_BYTES);

    usize::try_from(hint).unwrap_or(usize::MAX)
}

/// Streams `response` through the head parser according to `mode`.
///
/// In [`BodyMode::Full`] the head parser stops capturing once the document head ends, but the
/// body keeps streaming up to its cap. `content_length` is the final response's
/// `Content-Length` hint, used only to pre-size the collected body.
///
/// # Errors
///
/// Returns an error when the response body cannot be read or the parsing task fails.
pub(crate) async fn read_body(
    response: wreq::Response,
    mode: BodyMode,
    content_length: Option<u64>,
) -> Result<StreamedBody, ApiError> {
    let (collect_body, max_bytes) = match mode {
        BodyMode::Head => (false, HEAD_MAX_BYTES),
        BodyMode::Full(max_bytes) => (true, max_bytes),
    };

    let mut stream = response.bytes_stream();

    // Await the first chunk before spawning the blocking parser: no OS thread is parked while
    // the upstream is still sending its first body byte.
    let first = stream.next().await;

    let (sender, receiver) = mpsc::channel::<Result<Bytes, wreq::Error>>(channel_capacity(mode));

    // The reader lives in a `JoinSet` so dropping this future aborts it: the sender is dropped,
    // the parser's channel closes, and the parser task exits instead of downloading on.
    let mut reader = JoinSet::new();

    match first {
        None => {
            // Empty body: the parser sees a closed channel and returns an empty result.
            drop(sender);
        }
        Some(Ok(chunk)) => {
            // Queue the first chunk before the parser starts. The channel has spare capacity
            // and no other sender, so this cannot fail or block.
            let _ = sender.send(Ok(chunk)).await;
            reader.spawn(async move {
                while let Some(chunk) = stream.next().await {
                    // Backpressure from a full channel awaits here instead of blocking a
                    // worker thread.
                    if sender.send(chunk).await.is_err() {
                        break;
                    }
                }
            });
        }
        Some(Err(error)) => {
            // Hand the error to the parser without spawning a reader that would keep polling
            // the failed stream.
            let _ = sender.send(Err(error)).await;
            drop(sender);
        }
    }

    let body_capacity = initial_body_capacity(content_length, max_bytes);
    let read = parser_pool::parse(receiver, collect_body, max_bytes, body_capacity).await;

    // The parser is done reading, so the reader has no remaining consumer. Abort it even on
    // success: it may be parked in `bytes_stream().next()`, which dropping the receiver does
    // not interrupt, and a stalled upstream must not hold the response until the client
    // timeout. On a failed read this also prevents a stalled body from delaying the error.
    reader.abort_all();

    while let Some(result) = reader.join_next().await {
        let Err(error) = result else {
            continue;
        };

        if error.is_cancelled() {
            continue;
        }

        // A failing read already becomes an API error and is logged once at error level;
        // only a reader failure on an otherwise successful read needs a warning.
        if read.is_ok() {
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

    read
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

    #[test]
    fn bounds_the_channel_by_body_mode() {
        assert_eq!(channel_capacity(BodyMode::Head), 1);
        assert_eq!(channel_capacity(BodyMode::Full(HEAD_MAX_BYTES)), 4);
    }

    #[test]
    fn clamps_body_capacity_hints() {
        assert_eq!(initial_body_capacity(None, 25 * 1024 * 1024), 0);
        assert_eq!(initial_body_capacity(Some(4096), 25 * 1024 * 1024), 4096);
        assert_eq!(initial_body_capacity(Some(64 * 1024), 16 * 1024), 16 * 1024);
        assert_eq!(
            initial_body_capacity(Some(64 * 1024 * 1024), 64 * 1024 * 1024),
            1024 * 1024
        );
        assert_eq!(initial_body_capacity(Some(u64::MAX), u64::MAX), 1024 * 1024);
    }
}
