//! Token storage and redemption headers.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use reqwest::header::HeaderValue;
use tracing::{debug, warn};

use super::persist;
use super::{
    AUTHENTICATOR_LEN, CHALLENGE_DIGEST_LEN, NONCE_LEN, PrivacyPassError, PrivacyPassOptions,
    TOKEN_KEY_ID_LEN, TOKEN_LEN, TOKEN_TYPE,
};

/// A thread-safe store of unspent Privacy Pass tokens.
///
/// Newly issued tokens are added at the front and consumed first, so the store drains the
/// freshest batch before older ones. A batch the issuer rejects is discarded and replaced by
/// the client's retry path.
///
/// The store is in memory by default. With [`PrivacyPassOptions::state_path`] or
/// [`TokenStore::with_path`], tokens are loaded from the file on construction and written back
/// atomically after every issuance, consumption, and clear. A missing, unreadable, or corrupt
/// file leaves the store empty and is replaced on the next successful write.
#[derive(Debug)]
pub struct TokenStore {
    /// The unspent tokens, with the next token at the front.
    tokens: Mutex<VecDeque<PrivacyToken>>,
    /// The maximum number of tokens kept.
    capacity: usize,
    /// The low-water mark for background refills.
    low: usize,
    /// The optional state file path; `None` keeps tokens in memory only.
    path: Option<PathBuf>,
}

impl TokenStore {
    /// Creates a store from the given options, loading persisted tokens when configured.
    #[must_use]
    pub fn new(options: &PrivacyPassOptions) -> Self {
        Self::with_optional_path(options, options.state_path.as_deref())
    }

    /// Creates a store that persists tokens at `path`, independently of the options.
    #[must_use]
    pub fn with_path(options: &PrivacyPassOptions, path: impl Into<PathBuf>) -> Self {
        let path = path.into();

        Self::with_optional_path(options, Some(&path))
    }

    /// Creates a store and loads any tokens already persisted at `path`.
    fn with_optional_path(options: &PrivacyPassOptions, path: Option<&Path>) -> Self {
        let (_, capacity, low) = options.normalized();
        let mut tokens = VecDeque::new();

        if let Some(path) = path {
            match persist::load(path) {
                Ok(Some(loaded)) => {
                    let loaded_count = loaded.len();

                    tokens.extend(loaded.into_iter().take(capacity));

                    if tokens.len() < loaded_count {
                        debug!(
                            path = %path.display(),
                            loaded = loaded_count,
                            kept = tokens.len(),
                            "discarded persisted Privacy Pass tokens beyond the store capacity"
                        );
                    }

                    debug!(
                        path = %path.display(),
                        tokens = tokens.len(),
                        "loaded persisted Privacy Pass tokens"
                    );
                }
                Ok(None) => debug!(
                    path = %path.display(),
                    "no persisted Privacy Pass tokens found"
                ),
                Err(error) => warn!(
                    "error.type" = "privacy_pass_state",
                    "exception.message" = %error,
                    path = %path.display(),
                    "could not load persisted Privacy Pass tokens; starting with an empty store"
                ),
            }
        }

        Self {
            tokens: Mutex::new(tokens),
            capacity,
            low,
            path: path.map(Path::to_path_buf),
        }
    }

    /// Returns the number of unspent tokens.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Returns whether the store has no unspent tokens.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns whether the remaining tokens are below the configured low-water mark.
    #[must_use]
    pub fn is_low(&self) -> bool {
        self.len() < self.low
    }

    /// Removes and returns the next unspent token from the newest batch.
    ///
    /// A configured state file is rewritten before the token is returned, so a crash cannot
    /// resurrect a token that has already been handed out.
    // The lock is held through the write so concurrent takes cannot reorder the state file.
    #[allow(clippy::significant_drop_tightening)]
    #[must_use]
    pub fn take(&self) -> Option<PrivacyToken> {
        let mut tokens = self.lock();
        let token = tokens.pop_front()?;

        self.persist(&tokens);

        Some(token)
    }

    /// Adds freshly issued tokens to the front, dropping the oldest tokens beyond the capacity.
    // The lock is held through the write so concurrent takes cannot reorder the state file.
    #[allow(clippy::significant_drop_tightening)]
    pub fn add(&self, tokens: impl IntoIterator<Item = PrivacyToken>) {
        let mut store = self.lock();

        for token in tokens {
            while store.len() >= self.capacity {
                let _ = store.pop_back();
            }

            store.push_front(token);
        }

        self.persist(&store);
    }

    /// Removes every unspent token.
    // The lock is held through the write so concurrent takes cannot reorder the state file.
    #[allow(clippy::significant_drop_tightening)]
    pub fn clear(&self) {
        let mut tokens = self.lock();

        tokens.clear();
        self.persist(&tokens);
    }

    /// Writes the current queue to the configured state file, logging failures without contents.
    fn persist(&self, tokens: &VecDeque<PrivacyToken>) {
        let Some(path) = self.path.as_deref() else {
            return;
        };

        if let Err(error) = persist::save(path, tokens) {
            warn!(
                "error.type" = "privacy_pass_state",
                "exception.message" = %error,
                path = %path.display(),
                "could not persist Privacy Pass tokens"
            );
        }
    }

    /// Locks the token queue, recovering the tokens if a previous holder panicked.
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<PrivacyToken>> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A single unspent Privacy Pass token.
pub struct PrivacyToken {
    /// The token nonce.
    nonce: [u8; NONCE_LEN],
    /// The digest of the challenge the token answers.
    challenge_digest: [u8; CHALLENGE_DIGEST_LEN],
    /// The full token key ID.
    token_key_id: [u8; TOKEN_KEY_ID_LEN],
    /// The unblinded VOPRF authenticator.
    authenticator: [u8; AUTHENTICATOR_LEN],
}

impl PrivacyToken {
    /// Creates a token from its unblinded components.
    pub(super) const fn new(
        nonce: [u8; NONCE_LEN],
        challenge_digest: [u8; CHALLENGE_DIGEST_LEN],
        token_key_id: [u8; TOKEN_KEY_ID_LEN],
        authenticator: [u8; AUTHENTICATOR_LEN],
    ) -> Self {
        Self {
            nonce,
            challenge_digest,
            token_key_id,
            authenticator,
        }
    }

    /// Returns the `Authorization` header value that redeems this token.
    ///
    /// The value has the form `PrivateToken token="<url-safe base64>"`, matching the Kagi
    /// browser extension.
    ///
    /// # Errors
    ///
    /// Returns [`PrivacyPassError::Serialize`] if the resulting header value is invalid. This
    /// cannot happen for a token produced by this crate.
    pub fn authorization_header(&self) -> Result<HeaderValue, PrivacyPassError> {
        let value = format!(
            "PrivateToken token=\"{}\"",
            URL_SAFE.encode(self.serialize())
        );

        HeaderValue::from_str(&value).map_err(|_| PrivacyPassError::Serialize)
    }

    /// Serializes the token in the fixed Kagi Privacy Pass wire format.
    pub(super) fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(TOKEN_LEN);
        bytes.extend_from_slice(&TOKEN_TYPE.to_be_bytes());
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.challenge_digest);
        bytes.extend_from_slice(&self.token_key_id);
        bytes.extend_from_slice(&self.authenticator);

        bytes
    }

    /// Parses a token from the fixed Kagi Privacy Pass wire format.
    ///
    /// Returns `None` for a malformed token or one with a different token type.
    pub(super) fn deserialize(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != TOKEN_LEN {
            return None;
        }

        let token_type = u16::from_be_bytes(bytes.get(..2)?.try_into().ok()?);
        if token_type != TOKEN_TYPE {
            return None;
        }

        let nonce_start = 2;
        let challenge_start = nonce_start + NONCE_LEN;
        let key_id_start = challenge_start + CHALLENGE_DIGEST_LEN;
        let authenticator_start = key_id_start + TOKEN_KEY_ID_LEN;

        Some(Self {
            nonce: bytes.get(nonce_start..challenge_start)?.try_into().ok()?,
            challenge_digest: bytes.get(challenge_start..key_id_start)?.try_into().ok()?,
            token_key_id: bytes
                .get(key_id_start..authenticator_start)?
                .try_into()
                .ok()?,
            authenticator: bytes.get(authenticator_start..)?.try_into().ok()?,
        })
    }
}

impl fmt::Debug for PrivacyToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivacyToken(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_token(seed: u8) -> PrivacyToken {
        PrivacyToken::new(
            [seed; NONCE_LEN],
            [seed.wrapping_add(1); CHALLENGE_DIGEST_LEN],
            [seed.wrapping_add(2); TOKEN_KEY_ID_LEN],
            [seed.wrapping_add(3); AUTHENTICATOR_LEN],
        )
    }

    /// Options for a small store that never starts a background refill.
    fn options() -> PrivacyPassOptions {
        PrivacyPassOptions {
            tokens_to_stash: 4,
            max_tokens: 8,
            low_token_count: 0,
            state_path: None,
            fallback: false,
        }
    }

    /// Returns the seed byte of the next token, if any.
    fn take_seed(store: &TokenStore) -> Option<u8> {
        store.take().map(|token| token.nonce[0])
    }

    #[test]
    fn store_consumes_newest_tokens_first_and_caps_its_capacity() {
        let options = PrivacyPassOptions {
            tokens_to_stash: 3,
            max_tokens: 3,
            low_token_count: 1,
            state_path: None,
            fallback: false,
        };
        let store = TokenStore::new(&options);

        store.add([
            dummy_token(1),
            dummy_token(2),
            dummy_token(3),
            dummy_token(4),
        ]);
        assert_eq!(store.len(), 3);
        assert!(!store.is_low());

        assert_eq!(store.take().map(|token| token.nonce[0]), Some(4));
        assert_eq!(store.take().map(|token| token.nonce[0]), Some(3));
        assert!(!store.is_low());
        assert_eq!(store.take().map(|token| token.nonce[0]), Some(2));
        assert!(store.is_low());
        assert!(store.take().is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn a_zero_low_water_mark_disables_background_refills() {
        let options = PrivacyPassOptions {
            tokens_to_stash: 1,
            max_tokens: 1,
            low_token_count: 0,
            state_path: None,
            fallback: false,
        };
        let store = TokenStore::new(&options);
        store.add([dummy_token(1)]);
        let _ = store.take();

        assert!(!store.is_low());
    }

    #[test]
    fn authorization_headers_are_quoted_and_round_trip_the_token() {
        let token = dummy_token(7);
        let header = token
            .authorization_header()
            .expect("the token header is valid");
        let value = header.to_str().expect("the header is UTF-8");

        assert!(value.starts_with("PrivateToken token=\""));
        assert!(value.ends_with('"'));

        let encoded = value
            .strip_prefix("PrivateToken token=\"")
            .and_then(|value| value.strip_suffix('"'))
            .expect("the header has the expected shape");
        let bytes = URL_SAFE
            .decode(encoded)
            .expect("the token is URL-safe base64");

        assert_eq!(bytes, token.serialize());
        assert_eq!(
            u16::from_be_bytes([bytes[0], bytes[1]]),
            TOKEN_TYPE,
            "the token type is part of the wire format"
        );
    }

    #[test]
    fn token_debug_output_is_redacted() {
        let token = dummy_token(1);
        let debug = format!("{token:?}");

        assert_eq!(debug, "PrivacyToken(..)");
        assert!(!debug.contains("1, 1, 1"));
    }

    #[test]
    fn tokens_round_trip_through_the_state_file() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("neuradex/privacy-pass/tokens.json");
        let store = TokenStore::with_path(&options(), &path);

        store.add([
            dummy_token(1),
            dummy_token(2),
            dummy_token(3),
            dummy_token(4),
        ]);
        drop(store);

        let reloaded = TokenStore::with_path(&options(), &path);

        assert_eq!(reloaded.len(), 4);
        assert_eq!(take_seed(&reloaded), Some(4));
        assert_eq!(take_seed(&reloaded), Some(3));
        assert_eq!(take_seed(&reloaded), Some(2));
        assert_eq!(take_seed(&reloaded), Some(1));
        assert!(reloaded.take().is_none());
    }

    #[test]
    fn consumed_tokens_are_not_restored() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("tokens.json");
        let store = TokenStore::with_path(&options(), &path);

        store.add([dummy_token(1), dummy_token(2), dummy_token(3)]);
        assert_eq!(take_seed(&store), Some(3));
        drop(store);

        let reloaded = TokenStore::with_path(&options(), &path);

        assert_eq!(reloaded.len(), 2);
        assert_eq!(take_seed(&reloaded), Some(2));
        assert_eq!(take_seed(&reloaded), Some(1));
        assert!(reloaded.take().is_none());
    }

    #[test]
    fn clearing_the_store_is_persisted() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("tokens.json");
        let store = TokenStore::with_path(&options(), &path);

        store.add([dummy_token(1), dummy_token(2)]);
        store.clear();
        drop(store);

        let reloaded = TokenStore::with_path(&options(), &path);

        assert!(reloaded.is_empty());
    }

    #[test]
    fn a_missing_state_file_starts_empty() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("missing/tokens.json");
        let store = TokenStore::new(&PrivacyPassOptions {
            state_path: Some(path),
            ..options()
        });

        assert!(store.is_empty());
    }

    #[test]
    fn a_corrupt_state_file_starts_empty_and_is_replaced_on_write() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("tokens.json");

        std::fs::write(&path, b"{not json").expect("write the corrupt state file");

        let store = TokenStore::with_path(&options(), &path);

        assert!(store.is_empty());

        store.add([dummy_token(1)]);
        drop(store);

        let reloaded = TokenStore::with_path(&options(), &path);

        assert_eq!(take_seed(&reloaded), Some(1));
    }

    #[test]
    fn an_unreadable_state_file_leaves_the_store_usable_in_memory() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("tokens.json");

        // A directory at the state path makes both the read and the atomic rename fail.
        std::fs::create_dir(&path).expect("create a directory at the state path");

        let store = TokenStore::with_path(&options(), &path);

        assert!(store.is_empty());

        store.add([dummy_token(1)]);

        assert_eq!(take_seed(&store), Some(1));
    }

    #[cfg(unix)]
    #[test]
    fn state_files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("neuradex/tokens.json");
        let store = TokenStore::with_path(&options(), &path);

        store.add([dummy_token(1)]);

        let file_mode = std::fs::metadata(&path)
            .expect("the state file exists")
            .permissions()
            .mode()
            & 0o777;
        let directory_mode = std::fs::metadata(path.parent().expect("the state path has a parent"))
            .expect("the state directory exists")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(file_mode, 0o600);
        assert_eq!(directory_mode, 0o700);
    }

    #[test]
    fn a_store_without_a_path_stays_in_memory() {
        let options = PrivacyPassOptions::default();
        assert!(options.state_path.is_none());

        let store = TokenStore::new(&options);

        store.add([dummy_token(1), dummy_token(2)]);

        assert_eq!(store.len(), 2);
        assert_eq!(take_seed(&store), Some(2));
        assert_eq!(take_seed(&store), Some(1));
        assert!(store.take().is_none());
    }
}
