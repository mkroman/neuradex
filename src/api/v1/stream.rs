//! Streaming of response bodies through the document head parser.
//!
//! The HTML tokenizer is not `Send`, so the response body is streamed through a bounded channel
//! and parsed on a blocking thread. The channel is bounded so that the reader stalls — and the
//! download is throttled — once the parser falls behind. Parsing stops as soon as the document
//! head has been received, aborting the rest of the download.

use std::collections::{BTreeMap, btree_map::Entry};

use futures::StreamExt;
use tokio::sync::mpsc;
use wreq::header::{CONTENT_TYPE, HeaderMap};

use crate::api::v1::error::ApiError;
use crate::api::v1::redirect::media_type;
use crate::metadata::{HeadParser, PageMetadata};

/// The maximum size of a response consumed while extracting document head metadata.
pub(crate) const HEAD_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The media types that can be returned as text.
const TEXT_MEDIA_TYPES: &[&str] = &[
    "application/atom+xml",
    "application/ecmascript",
    "application/graphql",
    "javascript",
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
pub(crate) enum Mode {
    /// Stop as soon as the document head has been parsed.
    Head,
    /// Read the full body, up to a maximum number of bytes.
    Full(u64),
}

/// The outcome of streaming a response through the head parser.
pub(crate) struct Streamed {
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
/// # Errors
///
/// Returns an error when the response body cannot be read or the parsing task fails.
pub(crate) async fn read(response: wreq::Response, mode: Mode) -> Result<Streamed, ApiError> {
    let (collect_body, max_bytes) = match mode {
        Mode::Head => (false, HEAD_MAX_BYTES),
        Mode::Full(max_bytes) => (true, max_bytes),
    };

    let (sender, mut receiver) = mpsc::channel::<Result<Vec<u8>, wreq::Error>>(4);

    let reader = tokio::spawn(async move {
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            // Backpressure from a full channel awaits here instead of blocking a worker thread.
            let sent = match chunk {
                Ok(chunk) => sender.send(Ok(chunk.to_vec())).await.is_ok(),
                Err(error) => sender.send(Err(error)).await.is_ok(),
            };

            if !sent {
                break;
            }
        }
    });

    let read = tokio::task::spawn_blocking(move || {
        let chunks = std::iter::from_fn(|| receiver.blocking_recv());

        consume(chunks, collect_body, max_bytes)
    })
    .await??;

    // The reader finishes once the parser stops reading from the channel, aborting the rest of
    // the response.
    if let Err(error) = reader.await {
        tracing::warn!(%error, "reader task failed");
    }

    Ok(read)
}

/// Consumes `chunks` through the head parser, collecting the body when `collect_body` is set.
///
/// # Errors
///
/// Returns an error when a chunk failed to read from the upstream response.
fn consume(
    chunks: impl Iterator<Item = Result<Vec<u8>, wreq::Error>>,
    collect_body: bool,
    max_bytes: u64,
) -> Result<Streamed, ApiError> {
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

        let chunk = chunk.map_err(|error| ApiError::upstream(error.to_string()))?;
        bytes_read = bytes_read.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));

        parser.feed(&chunk);

        if let Some(body) = body.as_mut() {
            if body.len() < cap {
                let remaining = cap - body.len();
                let take = remaining.min(chunk.len());
                body.extend_from_slice(&chunk[..take]);
            }

            if body.len() >= cap {
                at_limit = true;

                continue;
            }
        }

        if parser.is_done() {
            break;
        }

        if !collect_body && bytes_read >= max_bytes {
            at_limit = true;
        }
    }

    parser.finish();

    Ok(Streamed {
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
/// Responses without a content type are allowed: too many servers omit the header for the
/// response to be discarded.
///
/// # Errors
///
/// Returns [`ApiError::UnsupportedMediaType`] for binary content types.
pub(crate) fn ensure_text_content(response: &wreq::Response) -> Result<(), ApiError> {
    let Some(content_type) = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(());
    };

    let media = media_type(content_type);

    if media.is_empty() || is_text_media_type(&media) {
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
        let name = name.as_str().to_ascii_lowercase();
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
    use wreq::header::CONTENT_LENGTH;

    use super::*;

    #[test]
    fn classifies_text_media_types() {
        assert!(is_text_media_type("text/html"));
        assert!(is_text_media_type("text/plain"));
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
    fn joins_repeated_header_values() {
        let mut headers = HeaderMap::new();
        headers.insert("set-cookie", "a=1".parse().unwrap());
        headers.append("set-cookie", "b=2".parse().unwrap());
        headers.insert(CONTENT_LENGTH, "42".parse().unwrap());

        let json = headers_to_json(&headers);

        assert_eq!(json.get("set-cookie").map(String::as_str), Some("a=1, b=2"));
        assert_eq!(json.get("content-length").map(String::as_str), Some("42"));
    }

    #[test]
    fn reports_body_truncation_only_past_the_cap() {
        // A body that exactly fits the cap is complete, not truncated.
        let read = consume(std::iter::once(Ok(vec![b'x'; 8])), true, 8).expect("reads");

        assert_eq!(read.bytes_read, 8);
        assert_eq!(read.body.as_deref().map(<[u8]>::len), Some(8));
        assert!(!read.truncated);

        // Data after the cap proves the body was cut short; the overflowing chunk is discarded.
        let read =
            consume([Ok(vec![b'x'; 8]), Ok(vec![b'y'; 4])].into_iter(), true, 8).expect("reads");

        assert_eq!(read.bytes_read, 8);
        assert_eq!(read.body.as_deref().map(<[u8]>::len), Some(8));
        assert!(read.truncated);
    }

    #[test]
    fn reports_head_truncation_only_past_the_cap() {
        let html = b"<html><head><title>Hello</title></head></html>";

        // A head that exactly fits the cap parses completely.
        let read = consume(
            std::iter::once(Ok(html.to_vec())),
            false,
            u64::try_from(html.len()).expect("fits"),
        )
        .expect("reads");

        assert_eq!(read.metadata.title.as_deref(), Some("Hello"));
        assert!(!read.truncated);

        // More bytes after the cap prove the head was cut short; the overflowing chunk is
        // discarded.
        let read = consume(
            [Ok(vec![b'<'; 16]), Ok(vec![b'x'; 8])].into_iter(),
            false,
            16,
        )
        .expect("reads");

        assert_eq!(read.bytes_read, 16);
        assert!(read.truncated);
    }

    #[test]
    fn stops_reading_when_the_head_parses() {
        let read = consume(
            [
                Ok(b"<html><head><title>Hello</title></head>".to_vec()),
                Ok(vec![b'x'; 8]),
            ]
            .into_iter(),
            true,
            HEAD_MAX_BYTES,
        )
        .expect("reads");

        assert_eq!(read.metadata.title.as_deref(), Some("Hello"));
        assert_eq!(read.bytes_read, 39);
        assert!(!read.truncated);
    }
}
