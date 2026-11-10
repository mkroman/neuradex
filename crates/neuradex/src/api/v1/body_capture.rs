//! Bounded capture of request bodies for audit and future content wiring.

use axum::body::{Body, Bytes};
use futures::StreamExt;

/// The largest JSON request body the service captures.
///
/// Request bodies are small, so the JSON extractor never buffers more than this even when
/// `CONTENT_MAX_BODY_BYTES` is configured for the much larger fetched page bodies.
pub(crate) const JSON_MAX_BODY_BYTES: usize = 1024 * 1024;

/// The capture limit installed by the request audit middleware.
///
/// Handlers and extractors that need the configured content body limit read it from the request
/// extensions, so they do not need to know the concrete router state type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BodyCaptureLimit(pub(crate) usize);

/// A request body captured up to a byte limit.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CapturedBody {
    /// The captured bytes.
    pub(crate) bytes: Bytes,
    /// Whether bytes beyond the limit were left unread.
    pub(crate) truncated: bool,
}

impl CapturedBody {
    /// Returns the number of captured bytes.
    pub(crate) const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the body was cut short by the capture limit.
    pub(crate) const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Reads `body` up to `max_bytes`, marking the result when more data was left unread.
///
/// The stream is dropped as soon as the limit is reached, so an oversized body does not have to
/// be buffered or downloaded in full.
///
/// # Errors
///
/// Returns an error when the body stream fails while reading.
pub(crate) async fn capture_body(
    body: Body,
    max_bytes: usize,
) -> Result<CapturedBody, axum::Error> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    let mut truncated = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let remaining = max_bytes.saturating_sub(bytes.len());

        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }

        bytes.extend_from_slice(&chunk);
    }

    Ok(CapturedBody {
        bytes: Bytes::from(bytes),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_bodies_up_to_the_limit() {
        let captured = capture_body(Body::from("hello world"), 5)
            .await
            .expect("the body is readable");

        assert_eq!(&captured.bytes[..], b"hello");
        assert_eq!(captured.len(), 5);
        assert!(captured.truncated());
    }

    #[tokio::test]
    async fn leaves_bodies_within_the_limit_intact() {
        let captured = capture_body(Body::from("hello"), 5)
            .await
            .expect("the body is readable");

        assert_eq!(&captured.bytes[..], b"hello");
        assert_eq!(captured.len(), 5);
        assert!(!captured.truncated());
    }

    #[test]
    fn empty_bodies_are_not_truncated() {
        let captured = CapturedBody::default();

        assert!(captured.bytes.is_empty());
        assert_eq!(captured.len(), 0);
        assert!(!captured.truncated());
    }
}
