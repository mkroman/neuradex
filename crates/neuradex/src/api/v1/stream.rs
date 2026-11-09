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

    // Await the reader on every path — including a failed read — so it is never left detached.
    // It finishes once the parser stops reading from the channel, aborting the rest of the
    // response.
    if let Err(error) = reader.await {
        tracing::warn!(%error, "reader task failed");
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

        let chunk = chunk.map_err(ApiError::upstream)?;
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

    #[test]
    fn reports_body_truncation_only_past_the_cap() {
        // A body that exactly fits the cap is complete, not truncated.
        let read = consume(std::iter::once(Ok(vec![b'x'; 8].into())), true, 8).expect("reads");

        assert_eq!(read.bytes_read, 8);
        assert_eq!(read.body.as_deref().map(<[u8]>::len), Some(8));
        assert!(!read.truncated);

        // Data after the cap proves the body was cut short; the overflowing chunk is discarded.
        let read = consume(
            [Ok(vec![b'x'; 8].into()), Ok(vec![b'y'; 4].into())].into_iter(),
            true,
            8,
        )
        .expect("reads");

        assert_eq!(read.bytes_read, 8);
        assert_eq!(read.body.as_deref().map(<[u8]>::len), Some(8));
        assert!(read.truncated);
    }

    #[test]
    fn reports_truncation_when_the_only_chunk_crosses_the_cap() {
        let read = consume(std::iter::once(Ok(vec![b'x'; 12].into())), true, 8).expect("reads");

        assert_eq!(read.bytes_read, 12);
        assert_eq!(read.body.as_deref().map(<[u8]>::len), Some(8));
        assert!(read.truncated);
    }

    #[test]
    fn does_not_report_truncation_when_the_only_chunk_fits_exactly() {
        let read = consume(std::iter::once(Ok(vec![b'x'; 8].into())), true, 8).expect("reads");

        assert_eq!(read.bytes_read, 8);
        assert_eq!(read.body.as_deref().map(<[u8]>::len), Some(8));
        assert!(!read.truncated);
    }

    #[test]
    fn reports_truncation_when_the_final_chunk_crosses_the_cap() {
        let read = consume(
            [Ok(vec![b'x'; 4].into()), Ok(vec![b'y'; 8].into())].into_iter(),
            true,
            8,
        )
        .expect("reads");

        assert_eq!(read.bytes_read, 12);
        assert_eq!(read.body.as_deref(), Some(&b"xxxxyyyy"[..]));
        assert!(read.truncated);
    }

    #[test]
    fn reports_head_truncation_only_past_the_cap() {
        let html = b"<html><head><title>Hello</title></head></html>";

        // A head that exactly fits the cap parses completely.
        let read = consume(
            std::iter::once(Ok(html.to_vec().into())),
            false,
            u64::try_from(html.len()).expect("fits"),
        )
        .expect("reads");

        assert_eq!(read.metadata.title.as_deref(), Some("Hello"));
        assert!(!read.truncated);

        // More bytes after the cap prove the head was cut short; the overflowing chunk is
        // discarded.
        let read = consume(
            [Ok(vec![b'<'; 16].into()), Ok(vec![b'x'; 8].into())].into_iter(),
            false,
            16,
        )
        .expect("reads");

        assert_eq!(read.bytes_read, 16);
        assert!(read.truncated);
    }

    #[test]
    fn keeps_reading_past_the_head_in_full_mode() {
        let head = b"<html><head><title>Hello</title></head>".to_vec();
        let tail = b"<body><p>Hello</p></body></html>".to_vec();
        let mut expected = head.clone();
        expected.extend_from_slice(&tail);
        let expected_len = u64::try_from(expected.len()).expect("fits");

        let read = consume(
            [Ok(head.into()), Ok(tail.into())].into_iter(),
            true,
            HEAD_MAX_BYTES,
        )
        .expect("reads");

        assert_eq!(read.metadata.title.as_deref(), Some("Hello"));
        assert_eq!(read.body.as_deref(), Some(expected.as_slice()));
        assert_eq!(read.bytes_read, expected_len);
        assert!(!read.truncated);
    }

    #[test]
    fn stops_reading_at_the_head_in_head_mode() {
        let read = consume(
            [
                Ok(b"<html><head><title>Hello</title></head>".to_vec().into()),
                Ok(vec![b'x'; 8].into()),
            ]
            .into_iter(),
            false,
            HEAD_MAX_BYTES,
        )
        .expect("reads");

        assert_eq!(read.metadata.title.as_deref(), Some("Hello"));
        assert_eq!(read.bytes_read, 39);
        assert!(read.body.is_none());
        assert!(!read.truncated);
    }
}
