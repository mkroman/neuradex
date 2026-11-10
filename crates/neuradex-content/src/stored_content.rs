//! References to content that has been written to a store.

use neuradex_packfile::{ContentHash, PackSummary};
use serde::{Deserialize, Serialize};

/// Where a piece of content is stored.
///
/// This mirrors the audit event's storage class without depending on
/// `neuradex-audit`. The serde representation is the stable wire name:
/// `"pack"` or `"object"`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentStorage {
    /// The content is one frame inside a packfile.
    Pack,
    /// The content is a standalone object.
    Object,
}

impl ContentStorage {
    /// Returns the stable lowercase name used by audit and index records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pack => "pack",
            Self::Object => "object",
        }
    }
}

/// A durable reference to one body held by the content plane.
///
/// The reference is returned by
/// [`ContentWriter::append`](crate::ContentWriter::append) before the bytes are
/// uploaded. The key and content hash are final at that point; call
/// [`ContentWriter::flush`](crate::ContentWriter::flush) to make the bytes
/// durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredContent {
    storage: ContentStorage,
    key: String,
    hash: ContentHash,
    sha256_hex: String,
    size: u64,
    content_type: Option<String>,
    pack_summary: Option<PackSummary>,
}

impl StoredContent {
    /// Builds a reference.
    ///
    /// The hexadecimal SHA-256 field is derived from `hash`, so the two always
    /// agree.
    #[must_use]
    pub fn new(
        storage: ContentStorage,
        key: impl Into<String>,
        hash: ContentHash,
        size: u64,
        content_type: Option<String>,
    ) -> Self {
        Self {
            storage,
            key: key.into(),
            hash,
            sha256_hex: hash.sha256_hex(),
            size,
            content_type,
            pack_summary: None,
        }
    }

    /// Attaches the summary of the pack that contains this body.
    ///
    /// References returned directly by `append` do not have a summary because
    /// the pack has not been sealed yet. `flush` returns updated copies with
    /// the summary filled in.
    #[must_use]
    pub const fn with_pack_summary(mut self, summary: PackSummary) -> Self {
        self.pack_summary = Some(summary);
        self
    }

    /// Returns the storage class.
    #[must_use]
    pub const fn storage(&self) -> ContentStorage {
        self.storage
    }

    /// Returns the store key. This is the field audit records call `key`.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the content hashes.
    #[must_use]
    pub const fn hash(&self) -> ContentHash {
        self.hash
    }

    /// Returns the lowercase hexadecimal BLAKE3 digest for audit records.
    #[must_use]
    pub fn blake3_hex(&self) -> String {
        self.hash.blake3_hex()
    }

    /// Returns the lowercase hexadecimal SHA-256 digest for audit records.
    #[must_use]
    pub fn sha256_hex(&self) -> &str {
        &self.sha256_hex
    }

    /// Returns the plaintext body size in bytes.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns the media type captured with the body, when known.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// Returns the summary of the containing pack, once it has been sealed.
    #[must_use]
    pub const fn pack_summary(&self) -> Option<PackSummary> {
        self.pack_summary
    }
}

#[cfg(test)]
mod tests {
    use neuradex_packfile::hash_body;

    use super::{ContentStorage, StoredContent};

    #[test]
    fn references_derive_the_sha256_hex_field_from_the_hash() {
        let hash = hash_body(b"hello");
        let content = StoredContent::new(
            ContentStorage::Pack,
            "packs/fetch/2026-11-10T00/1.pack",
            hash,
            5,
            Some("text/plain".to_owned()),
        );

        assert_eq!(content.storage(), ContentStorage::Pack);
        assert_eq!(content.storage().as_str(), "pack");
        assert_eq!(content.key(), "packs/fetch/2026-11-10T00/1.pack");
        assert_eq!(content.hash(), hash);
        assert_eq!(content.blake3_hex(), hash.blake3_hex());
        assert_eq!(content.sha256_hex(), hash.sha256_hex());
        assert_eq!(content.size(), 5);
        assert_eq!(content.content_type(), Some("text/plain"));
        assert_eq!(content.pack_summary(), None);
    }

    #[test]
    fn object_storage_uses_the_object_wire_name() {
        assert_eq!(ContentStorage::Object.as_str(), "object");
        assert_eq!(
            serde_json::to_string(&ContentStorage::Object).unwrap(),
            "\"object\""
        );
        assert_eq!(
            serde_json::to_string(&ContentStorage::Pack).unwrap(),
            "\"pack\""
        );
    }
}
