//! Token storage and redemption headers.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use reqwest::header::HeaderValue;

use super::{
    AUTHENTICATOR_LEN, CHALLENGE_DIGEST_LEN, NONCE_LEN, PrivacyPassError, PrivacyPassOptions,
    TOKEN_KEY_ID_LEN, TOKEN_LEN, TOKEN_TYPE,
};

/// A thread-safe, in-memory store of unspent Privacy Pass tokens.
#[derive(Debug)]
pub struct TokenStore {
    /// The unspent tokens, with the next token at the front.
    tokens: Mutex<VecDeque<PrivacyToken>>,
    /// The maximum number of tokens kept.
    capacity: usize,
    /// The low-water mark for background refills.
    low: usize,
}

impl TokenStore {
    /// Creates an empty store from the given options.
    #[must_use]
    pub fn new(options: &PrivacyPassOptions) -> Self {
        let (_, capacity, low) = options.normalized();

        Self {
            tokens: Mutex::new(VecDeque::new()),
            capacity,
            low,
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

    /// Removes and returns the next unspent token.
    #[must_use]
    pub fn take(&self) -> Option<PrivacyToken> {
        self.lock().pop_front()
    }

    /// Adds freshly issued tokens, dropping the oldest tokens beyond the capacity.
    pub fn add(&self, tokens: impl IntoIterator<Item = PrivacyToken>) {
        let mut store = self.lock();

        for token in tokens {
            while store.len() >= self.capacity {
                let _ = store.pop_back();
            }

            store.push_front(token);
        }
    }

    /// Removes every unspent token.
    pub fn clear(&self) {
        self.lock().clear();
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
    fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(TOKEN_LEN);
        bytes.extend_from_slice(&TOKEN_TYPE.to_be_bytes());
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.challenge_digest);
        bytes.extend_from_slice(&self.token_key_id);
        bytes.extend_from_slice(&self.authenticator);

        bytes
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

    #[test]
    fn store_consumes_tokens_in_order_and_caps_its_capacity() {
        let options = PrivacyPassOptions {
            tokens_to_stash: 3,
            max_tokens: 3,
            low_token_count: 1,
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
}
