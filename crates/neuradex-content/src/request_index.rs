//! Hourly request index encoded as newline-delimited JSON.
//!
//! One entry describes one body reference and the request that produced it.
//! The index is deliberately flat: every field is a string, number, or
//! one of the two [`ContentStorage`] names, so it can be written and consumed
//! without depending on the audit event schema.
//!
//! The writer keeps entries in memory. [`RequestIndexWriter::encode`] returns
//! plain NDJSON; a caller that wants compressed objects can zstd the encoded
//! bytes before uploading them.

use serde::{Deserialize, Serialize};

use crate::stored_content::StoredContent;
use crate::{ContentError, ContentStorage};

/// One request-index row.
///
/// Field order is stable because it is the NDJSON field order. `content_type`
/// is omitted when it is unknown.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestIndexEntry {
    /// Identifier of the inbound request.
    pub request_id: String,
    /// Identifier of the audit event that owns this reference.
    pub event_id: String,
    /// RFC 3339 timestamp of the request.
    pub timestamp: String,
    /// Stable operation name, for example `fetch` or `peek`.
    pub operation: String,
    /// Whether the body lives in a pack or as an individual object.
    pub storage: ContentStorage,
    /// Store key of the containing pack or object.
    pub key: String,
    /// Lowercase hexadecimal BLAKE3 digest of the body.
    pub blake3: String,
    /// Lowercase hexadecimal SHA-256 digest of the body.
    pub sha256: String,
    /// Plaintext body size in bytes.
    pub size: u64,
    /// Captured media type, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

impl RequestIndexEntry {
    /// Builds an index row from a stored-content reference.
    #[must_use]
    pub fn from_stored(
        request_id: impl Into<String>,
        event_id: impl Into<String>,
        timestamp: impl Into<String>,
        operation: impl Into<String>,
        content: &StoredContent,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            event_id: event_id.into(),
            timestamp: timestamp.into(),
            operation: operation.into(),
            storage: content.storage(),
            key: content.key().to_owned(),
            blake3: content.blake3_hex(),
            sha256: content.sha256_hex().to_owned(),
            size: content.size(),
            content_type: content.content_type().map(str::to_owned),
        }
    }
}

/// Buffers request-index rows for one hour.
///
/// Rows are emitted in push order. [`RequestIndexWriter::finish_hour`] returns
/// the encoded bytes and clears the buffer, which is the intended call at an
/// hour boundary.
#[derive(Debug, Default)]
pub struct RequestIndexWriter {
    buffer: String,
    entries: usize,
}

impl RequestIndexWriter {
    /// Creates an empty writer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buffer: String::new(),
            entries: 0,
        }
    }

    /// Returns the number of buffered entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries
    }

    /// Returns whether no entries are buffered.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// Serializes `entry` as one NDJSON line and appends it to the buffer.
    ///
    /// The buffer is unchanged when serialization fails.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Json`] when the entry cannot be serialized.
    pub fn push(&mut self, entry: &RequestIndexEntry) -> Result<(), ContentError> {
        let line = serde_json::to_string(entry)?;
        self.buffer.push_str(&line);
        self.buffer.push('\n');
        self.entries += 1;
        Ok(())
    }

    /// Returns the buffered NDJSON without clearing it.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        self.buffer.as_bytes().to_vec()
    }

    /// Returns the buffered NDJSON and clears the writer.
    #[must_use]
    pub fn finish_hour(&mut self) -> Vec<u8> {
        self.entries = 0;
        std::mem::take(&mut self.buffer).into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use neuradex_packfile::hash_body;

    use super::{RequestIndexEntry, RequestIndexWriter};
    use crate::{ContentStorage, StoredContent};

    fn stored() -> StoredContent {
        StoredContent::new(
            ContentStorage::Pack,
            "packs/fetch/2026-11-10T00/1.pack",
            hash_body(b"body"),
            4,
            Some("text/plain".to_owned()),
        )
    }

    #[test]
    fn entries_encode_as_deterministic_json_lines() {
        let mut writer = RequestIndexWriter::new();
        let content = stored();
        let first = RequestIndexEntry::from_stored(
            "req-1",
            "evt-1",
            "2026-11-10T00:00:00Z",
            "fetch",
            &content,
        );
        let mut second = first.clone();
        second.request_id = "req-2".to_owned();
        second.content_type = None;

        assert!(writer.is_empty());
        writer.push(&first).unwrap();
        writer.push(&second).unwrap();
        assert_eq!(writer.len(), 2);

        let encoded = writer.encode();
        let text = std::str::from_utf8(&encoded).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            format!(
                "{{\"request_id\":\"req-1\",\"event_id\":\"evt-1\",\
                 \"timestamp\":\"2026-11-10T00:00:00Z\",\"operation\":\"fetch\",\
                 \"storage\":\"pack\",\"key\":\"packs/fetch/2026-11-10T00/1.pack\",\
                 \"blake3\":\"{}\",\"sha256\":\"{}\",\"size\":4,\
                 \"content_type\":\"text/plain\"}}",
                content.blake3_hex(),
                content.sha256_hex()
            )
        );
        assert_eq!(lines[1].find("content_type"), None);
        assert!(text.ends_with('\n'));

        assert_eq!(writer.encode(), encoded, "encoding is repeatable");
    }

    #[test]
    fn finish_hour_returns_bytes_and_clears_the_buffer() {
        let mut writer = RequestIndexWriter::new();
        writer
            .push(&RequestIndexEntry::from_stored(
                "req",
                "evt",
                "2026-11-10T00:00:00Z",
                "peek",
                &stored(),
            ))
            .unwrap();

        let bytes = writer.finish_hour();
        assert_ne!(bytes, Vec::<u8>::new());
        assert!(writer.is_empty());
        assert_eq!(writer.encode(), Vec::<u8>::new());

        let entry: RequestIndexEntry =
            serde_json::from_slice(bytes.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(entry.storage, ContentStorage::Pack);
        assert_eq!(entry.operation, "peek");
        assert_eq!(entry.content_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn object_entries_use_the_object_storage_name() {
        let content = StoredContent::new(
            ContentStorage::Object,
            "objects/abc",
            hash_body(b"large"),
            5,
            None,
        );
        let entry =
            RequestIndexEntry::from_stored("req", "evt", "2026-11-10T00:00:00Z", "fetch", &content);

        assert_eq!(entry.storage, ContentStorage::Object);
        assert_eq!(entry.storage.as_str(), "object");
        assert_eq!(entry.size, 5);
        assert_eq!(entry.content_type, None);
    }
}
