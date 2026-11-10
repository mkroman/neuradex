//! Kagi Privacy Pass authentication.
//!
//! Kagi issues batched VOPRF tokens (token type `0xF91A`) over the same HTTP endpoints the
//! browser extension uses. Token issuance needs an authenticated Kagi session, but redemption
//! is anonymous: a search redeems a token through the `Authorization` header instead of
//! sending cookies or the session token.
//!
//! The wire formats are fixed-size and mirror the reference `kagippcore` implementation:
//!
//! - a token request is `type:u16 || key_id:u8 || len:u16 || blinded element[32]...`,
//! - a token response is `len:u16 || evaluated element[32]... || proof[64]`,
//! - a token is `type:u16 || nonce[32] || challenge digest[32] || key id[32] || auth[64]`.
//!
//! Tokens are consumed once. They are kept in memory by default; an optional state file makes
//! unconsumed tokens survive restarts. See [`PrivacyPassOptions::state_path`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT_LANGUAGE, AUTHORIZATION, CONTENT_TYPE, HeaderValue, WWW_AUTHENTICATE,
};
use secrecy::{ExposeSecret, SecretString};
use time::{Month, OffsetDateTime, Time};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{Instrument, debug, warn};

use self::wire::{
    Challenge, finalize_tokens, parse_challenge, parse_token_response, prepare_request,
    serialize_token_request,
};
use crate::{Error, RequestError};

mod persist;
mod token;
mod wire;

pub use token::{PrivacyToken, TokenStore};

/// The default number of tokens requested in one issuance batch.
pub const DEFAULT_TOKENS_TO_STASH: usize = 300;
/// The default maximum number of unspent tokens kept in memory.
pub const DEFAULT_MAX_TOKENS: usize = 2000;
/// The default low-water mark that starts a background refill.
pub const DEFAULT_LOW_TOKEN_COUNT: usize = 50;

/// The Kagi Privacy Pass token type (batched Ristretto255 tokens).
const TOKEN_TYPE: u16 = 0xf91a;
/// The length of a token nonce.
const NONCE_LEN: usize = 32;
/// The length of a challenge digest.
const CHALLENGE_DIGEST_LEN: usize = 32;
/// The length of a full token key ID.
const TOKEN_KEY_ID_LEN: usize = 32;
/// The length of a token authenticator.
const AUTHENTICATOR_LEN: usize = 64;
/// The length of a blinded or evaluated group element.
const ELEMENT_LEN: usize = 32;
/// The maximum number of tokens that fit in one issuance request.
///
/// The request's 16-bit length field counts the serialized blinded elements, which are 32
/// bytes each.
pub const MAX_TOKENS_PER_BATCH: usize = u16::MAX as usize / ELEMENT_LEN;
/// The length of a VOPRF proof.
const PROOF_LEN: usize = 64;
/// The serialized length of a token.
const TOKEN_LEN: usize =
    2 + NONCE_LEN + CHALLENGE_DIGEST_LEN + TOKEN_KEY_ID_LEN + AUTHENTICATOR_LEN;

/// The challenge endpoint.
const CHALLENGE_PATH: &str = "pp/wwwa";
/// The token issuance endpoint.
const ISSUANCE_PATH: &str = "pp/gettokens";
/// The header announcing a Privacy Pass capable client.
const CLIENT_HEADER: &str = "X-Kagi-PrivacyPass-Client";
/// The value of [`CLIENT_HEADER`].
const CLIENT_HEADER_VALUE: HeaderValue = HeaderValue::from_static("true");
/// The content type of a token request.
const TOKEN_REQUEST_CONTENT_TYPE: HeaderValue =
    HeaderValue::from_static("application/private-token-request");

/// Configuration for Privacy Pass token issuance and redemption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivacyPassOptions {
    /// The number of tokens requested in one issuance batch.
    ///
    /// Zero is treated as one; values above [`MAX_TOKENS_PER_BATCH`] are clamped.
    pub tokens_to_stash: usize,
    /// The maximum number of unspent tokens kept in memory.
    ///
    /// The value is raised to at least [`PrivacyPassOptions::tokens_to_stash`] and one.
    pub max_tokens: usize,
    /// The number of remaining tokens below which a background refill starts.
    ///
    /// A value of zero disables low-water refills; an empty store still issues on demand.
    pub low_token_count: usize,
    /// The optional file used to persist unspent tokens across restarts.
    ///
    /// `None` keeps tokens in memory only. When set, tokens are loaded from the file before
    /// any issuance and written back atomically after every issuance, consumption, and clear.
    /// The file and any directory created for it are restricted to the current user on Unix
    /// (`0600` and `0700`). A missing, unreadable, or corrupt file does not fail construction:
    /// the store starts empty, logs a warning, and replaces the file on the next successful
    /// write.
    ///
    /// File access is synchronous and happens while the token store lock is held. The state
    /// file is small and local, so the brief block is acceptable.
    pub state_path: Option<PathBuf>,
    /// Whether searches fall back to session authentication when Privacy Pass fails.
    ///
    /// A quota rejection (429) degrades Privacy Pass until the start of the next calendar
    /// month, when Kagi's monthly generation epoch resets, and a subscription rejection (403)
    /// until the client is dropped. A search that fails while degraded — and, for a rejected
    /// fresh batch or a broken issuer response, only that search — is retried with the session
    /// token and its cookies instead.
    ///
    /// The fallback trades away Privacy Pass's unlinkability: those searches are linked to the
    /// account, so it is off by default.
    pub fallback: bool,
}

impl Default for PrivacyPassOptions {
    fn default() -> Self {
        Self {
            tokens_to_stash: DEFAULT_TOKENS_TO_STASH,
            max_tokens: DEFAULT_MAX_TOKENS,
            low_token_count: DEFAULT_LOW_TOKEN_COUNT,
            state_path: None,
            fallback: false,
        }
    }
}

impl PrivacyPassOptions {
    /// Returns the normalized batch size, store capacity, and low-water mark.
    fn normalized(&self) -> (u16, usize, usize) {
        let batch = self.tokens_to_stash.clamp(1, MAX_TOKENS_PER_BATCH);
        let batch =
            u16::try_from(batch).expect("the maximum batch size fits in a 16-bit length field");
        let capacity = self.max_tokens.max(usize::from(batch)).max(1);
        let low = self.low_token_count.min(capacity);

        (batch, capacity, low)
    }
}

/// Errors that can occur while issuing or redeeming Privacy Pass tokens.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrivacyPassError {
    /// The challenge request could not be sent.
    #[error("could not send Privacy Pass challenge request")]
    ChallengeRequest(#[source] RequestError),
    /// The challenge response did not include a `WWW-Authenticate` header.
    #[error("Privacy Pass challenge response did not include a WWW-Authenticate header")]
    MissingChallenge,
    /// The header did not carry exactly one Privacy Pass challenge.
    #[error("expected exactly one Privacy Pass challenge, found {0}")]
    ChallengeCount(usize),
    /// The challenge or its token key was invalid.
    #[error("invalid Privacy Pass challenge")]
    InvalidChallenge,
    /// A token nonce could not be generated.
    #[error("could not generate a Privacy Pass token nonce")]
    Random,
    /// A token could not be blinded.
    #[error("could not blind a Privacy Pass token")]
    Blinding,
    /// A request or token could not be serialized.
    #[error("could not serialize a Privacy Pass request")]
    Serialize,
    /// A Privacy Pass endpoint URL could not be built from the configured base URL.
    #[error("could not build a Privacy Pass endpoint URL")]
    InvalidEndpointUrl,
    /// The token request could not be sent.
    #[error("could not send Privacy Pass token request")]
    TokenRequest(#[source] RequestError),
    /// The token endpoint returned an error status.
    #[error("Privacy Pass token request returned an error status")]
    TokenStatus(#[source] RequestError),
    /// The token response body could not be read.
    #[error("could not read Privacy Pass token response")]
    TokenBody(#[source] RequestError),
    /// The token response was not valid, or its proof did not verify.
    #[error("invalid Privacy Pass token response")]
    InvalidTokenResponse,
    /// The issuer returned an empty batch.
    #[error("the Privacy Pass issuer returned no tokens")]
    EmptyBatch,
    /// No unspent token was available for redemption.
    #[error("no Privacy Pass token is available")]
    NoTokens,
}

impl PrivacyPassError {
    /// Returns whether the error means Kagi refused Privacy Pass issuance or served a response
    /// that cannot be used, so session authentication may succeed where Privacy Pass cannot.
    ///
    /// Transport failures are excluded: they say nothing about Privacy Pass specifically, and
    /// a session fallback fails just the same. A 401 issuance rejection means the session
    /// token itself was refused, which session authentication depends on too.
    #[must_use]
    pub fn is_issuer_failure(&self) -> bool {
        match self {
            Self::TokenStatus(error) => matches!(
                error.status(),
                Some(StatusCode::TOO_MANY_REQUESTS | StatusCode::FORBIDDEN)
            ),
            Self::MissingChallenge
            | Self::ChallengeCount(_)
            | Self::InvalidChallenge
            | Self::InvalidTokenResponse
            | Self::EmptyBatch => true,
            Self::ChallengeRequest(_)
            | Self::Random
            | Self::Blinding
            | Self::Serialize
            | Self::InvalidEndpointUrl
            | Self::TokenRequest(_)
            | Self::TokenBody(_)
            | Self::NoTokens => false,
        }
    }
}

/// Issues, stores, and redeems Kagi Privacy Pass tokens.
///
/// A client holds a cookie-free HTTP client, an authenticated session token used only for
/// issuance, and a [`TokenStore`] shared with all search requests.
pub struct PrivacyPassClient {
    /// The cookie-free HTTP client used for issuance and redemption requests.
    http: reqwest::Client,
    /// The Kagi base URL.
    base_url: reqwest::Url,
    /// The session token used to authorize token issuance.
    token: Arc<SecretString>,
    /// The `Accept-Language` header sent with issuance and redemption requests.
    language: HeaderValue,
    /// The unspent tokens.
    store: Arc<TokenStore>,
    /// Serializes issuance so concurrent requests do not overdraw the issuer.
    issuance: AsyncMutex<()>,
    /// The normalized batch size.
    batch_size: u16,
    /// Whether a background refill is already running.
    replenishing: AtomicBool,
    /// Whether failed issuance falls back to session authentication.
    fallback: bool,
    /// Whether Privacy Pass is degraded; a cheap mirror of `degraded_until`.
    is_degraded: AtomicBool,
    /// When issuance may be retried again, or `None` for the rest of the client's lifetime.
    ///
    /// Guarded by a synchronous lock that is never held across an await point.
    degraded_until: Mutex<Option<Instant>>,
}

impl PrivacyPassClient {
    /// Creates a Privacy Pass client.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BuildClient`] if the cookie-free HTTP client cannot be built.
    pub(crate) fn new(
        base_url: reqwest::Url,
        token: Arc<SecretString>,
        language: HeaderValue,
        timeout: Duration,
        user_agent: HeaderValue,
        options: &PrivacyPassOptions,
    ) -> Result<Self, Error> {
        let http = reqwest::ClientBuilder::new()
            .cookie_store(false)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent(user_agent)
            .build()
            .map_err(|error| Error::BuildClient(crate::client::log_request_error(error)))?;
        let (batch_size, _, _) = options.normalized();

        Ok(Self {
            http,
            base_url,
            token,
            language,
            store: Arc::new(TokenStore::new(options)),
            issuance: AsyncMutex::new(()),
            batch_size,
            replenishing: AtomicBool::new(false),
            fallback: options.fallback,
            is_degraded: AtomicBool::new(false),
            degraded_until: Mutex::new(None),
        })
    }

    /// Returns the store of unspent tokens.
    #[must_use]
    pub fn store(&self) -> &TokenStore {
        &self.store
    }

    /// Returns whether Privacy Pass is degraded: searches use session authentication instead.
    ///
    /// A quota rejection (429) degrades until the start of the next calendar month, when
    /// Kagi's monthly generation epoch resets, and a subscription rejection (403) for the rest
    /// of the client's lifetime. Once the deadline has passed the state resets here, so the
    /// next search attempts issuance again and re-degrades if the issuer still refuses.
    #[must_use]
    pub fn degraded(&self) -> bool {
        if !self.is_degraded.load(Ordering::Acquire) {
            return false;
        }

        let mut deadline = self.lock_degraded();
        if let Some(until) = *deadline
            && until <= Instant::now()
        {
            *deadline = None;
            drop(deadline);
            self.is_degraded.store(false, Ordering::Release);
            debug!("the Privacy Pass degradation window passed; issuing tokens again");

            return false;
        }

        true
    }

    /// Returns whether failed Privacy Pass searches fall back to session authentication.
    #[must_use]
    pub const fn fallback_enabled(&self) -> bool {
        self.fallback
    }

    /// Degrades Privacy Pass after an issuance failure that session authentication covers.
    ///
    /// Only quota (429) and subscription (403) rejections degrade, and only when the fallback
    /// is enabled: without a fallback the state would silently switch searches to session
    /// authentication. Transport failures and broken issuer responses fail the one request.
    fn consider_degrade(&self, error: &PrivacyPassError) {
        if !self.fallback {
            return;
        }

        let (until, window) = match degrade_deadline(error, SystemTime::now()) {
            DegradeDeadline::Keep => return,
            DegradeDeadline::Forever => (None, " for the client's lifetime"),
            DegradeDeadline::Until(duration) => (
                Some(Instant::now() + duration),
                " until the next monthly epoch",
            ),
        };

        *self.lock_degraded() = until;
        self.is_degraded.store(true, Ordering::Release);
        warn!(
            "error.type" = "privacy_pass",
            "exception.message" = %error,
            "Privacy Pass is degraded; searches fall back to session authentication{}",
            window,
        );
    }

    /// Locks the degradation deadline, recovering it if a previous holder panicked.
    fn lock_degraded(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.degraded_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Issues a fresh batch and adds it to the store, returning the number of tokens added.
    ///
    /// This is the explicit replenishment hook; searches call it automatically when the store
    /// is empty and start a background refill below the low-water mark.
    ///
    /// # Errors
    ///
    /// Returns a [`PrivacyPassError`] if the challenge cannot be fetched or parsed, the issuer
    /// request fails, or the token response is invalid.
    pub async fn replenish(&self) -> Result<usize, PrivacyPassError> {
        let _guard = self.issuance.lock().await;

        self.issue_locked().await
    }

    /// Removes every unspent token, so the next redemption issues a fresh batch.
    pub fn clear(&self) {
        self.store.clear();
    }

    /// Clears the store and issues a fresh batch while holding the issuance lock.
    ///
    /// Serializing the clear with issuance keeps a concurrent [`Self::acquire_token`] from
    /// losing a batch that was issued between the clear and the take.
    ///
    /// # Errors
    ///
    /// Returns a [`PrivacyPassError`] if the challenge cannot be fetched or parsed, the issuer
    /// request fails, or the token response is invalid.
    pub(crate) async fn reset_and_replenish(&self) -> Result<usize, PrivacyPassError> {
        let _guard = self.issuance.lock().await;

        self.store.clear();
        self.issue_locked().await
    }

    /// Returns the cookie-free HTTP client used for Privacy Pass requests.
    pub(crate) const fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Attaches the Privacy Pass authorization headers to a request.
    ///
    /// # Errors
    ///
    /// Returns [`PrivacyPassError::Serialize`] if the token header cannot be built.
    pub(crate) fn authorize(
        request: reqwest::RequestBuilder,
        token: &PrivacyToken,
    ) -> Result<reqwest::RequestBuilder, PrivacyPassError> {
        Ok(request
            .header(CLIENT_HEADER, CLIENT_HEADER_VALUE)
            .header(AUTHORIZATION, token.authorization_header()?))
    }

    /// Takes an unspent token, issuing a batch first if the store is empty.
    ///
    /// Taking a token below the low-water mark starts a best-effort background refill.
    ///
    /// # Errors
    ///
    /// Returns a [`PrivacyPassError`] if the store is empty and issuance fails.
    pub(crate) async fn acquire_token(self: &Arc<Self>) -> Result<PrivacyToken, PrivacyPassError> {
        let mut retried = false;

        loop {
            if let Some(token) = self.store.take() {
                self.maybe_replenish();

                return Ok(token);
            }

            let guard = self.issuance.lock().await;

            // Another request may have issued or cleared the store while this request waited
            // for the issuance lock, so re-check before issuing.
            if let Some(token) = self.store.take() {
                drop(guard);
                self.maybe_replenish();

                return Ok(token);
            }

            self.issue_locked().await?;
            // Take the token before releasing the issuance lock: a waiter must not steal the
            // batch this request just issued.
            let token = self.store.take();
            drop(guard);

            if let Some(token) = token {
                self.maybe_replenish();

                return Ok(token);
            }

            // A concurrent `clear` discarded the batch. Issue another one once; a store that
            // keeps getting cleared is not making progress.
            if retried {
                return Err(PrivacyPassError::NoTokens);
            }

            retried = true;
        }
    }

    /// Issues a batch while the issuance lock is held.
    ///
    /// An issuance failure that session authentication can cover degrades Privacy Pass when
    /// the fallback is enabled.
    ///
    /// # Errors
    ///
    /// Returns a [`PrivacyPassError`] if the challenge cannot be fetched or parsed, the issuer
    /// request fails, or the token response is invalid.
    async fn issue_locked(&self) -> Result<usize, PrivacyPassError> {
        let result = self.issue_batch().await;

        if let Err(error) = &result {
            self.consider_degrade(error);
        }

        result
    }

    /// Performs the challenge and issuance requests for one batch.
    ///
    /// # Errors
    ///
    /// Returns a [`PrivacyPassError`] if the challenge cannot be fetched or parsed, the issuer
    /// request fails, or the token response is invalid.
    async fn issue_batch(&self) -> Result<usize, PrivacyPassError> {
        let challenge = self.fetch_challenge().await?;
        let (request, pending) = prepare_request(&challenge, self.batch_size)?;
        let request_body = URL_SAFE.encode(serialize_token_request(&request)?);
        let url = self.endpoint(ISSUANCE_PATH)?;
        let span = crate::client::http_client_span(
            "POST",
            &url,
            "/pp/gettokens",
            "privacy_pass_tokens",
            0,
        );

        let response = self
            .http
            .post(url)
            .header(CLIENT_HEADER, CLIENT_HEADER_VALUE)
            .header(CONTENT_TYPE, TOKEN_REQUEST_CONTENT_TYPE)
            .header(ACCEPT_LANGUAGE, self.language.clone())
            .header(AUTHORIZATION, self.token.expose_secret())
            .body(request_body)
            .send()
            .instrument(span.clone())
            .await
            .map_err(|error| {
                let error = crate::client::log_request_error(error);
                crate::client::record_request_error(&span, &error);

                PrivacyPassError::TokenRequest(error)
            })?;
        crate::client::record_response(&span, &response);
        let response = response.error_for_status().map_err(|error| {
            let error = crate::client::log_request_error(error);
            crate::client::record_request_error(&span, &error);

            PrivacyPassError::TokenStatus(error)
        })?;
        let body = response
            .text()
            .instrument(span.clone())
            .await
            .map_err(|error| {
                let error = crate::client::log_request_error(error);
                crate::client::record_request_error(&span, &error);

                PrivacyPassError::TokenBody(error)
            })?;
        let response_bytes = URL_SAFE
            .decode(body.trim())
            .map_err(|_| PrivacyPassError::InvalidTokenResponse)?;
        let response = parse_token_response(&response_bytes)?;
        let tokens = finalize_tokens(&response, pending, challenge.public_key)?;
        let count = tokens.len();

        self.store.add(tokens);
        debug!(
            issued = count,
            remaining = self.store.len(),
            "issued Privacy Pass tokens"
        );

        Ok(count)
    }

    /// Fetches and parses the issuer challenge.
    async fn fetch_challenge(&self) -> Result<Challenge, PrivacyPassError> {
        let url = self.endpoint(CHALLENGE_PATH)?;
        let span =
            crate::client::http_client_span("GET", &url, "/pp/wwwa", "privacy_pass_challenge", 0);
        let response = self
            .http
            .get(url)
            .header(CLIENT_HEADER, CLIENT_HEADER_VALUE)
            .header(ACCEPT_LANGUAGE, self.language.clone())
            .send()
            .instrument(span.clone())
            .await
            .map_err(|error| {
                let error = crate::client::log_request_error(error);
                crate::client::record_request_error(&span, &error);

                PrivacyPassError::ChallengeRequest(error)
            })?;
        crate::client::record_response(&span, &response);
        let Some(header) = response.headers().get(WWW_AUTHENTICATE) else {
            span.record("error.type", "missing_challenge");
            span.record("otel.status_code", "ERROR");

            return Err(PrivacyPassError::MissingChallenge);
        };

        parse_challenge(header)
    }

    /// Builds an endpoint URL beneath the configured base URL.
    fn endpoint(&self, path: &str) -> Result<reqwest::Url, PrivacyPassError> {
        self.base_url
            .join(path)
            .map_err(|_| PrivacyPassError::InvalidEndpointUrl)
    }

    /// Starts a background refill when the store is below the low-water mark.
    fn maybe_replenish(self: &Arc<Self>) {
        if !self.store.is_low() || self.replenishing.swap(true, Ordering::AcqRel) {
            return;
        }

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.replenishing.store(false, Ordering::Release);

            return;
        };
        let client = Arc::clone(self);

        handle.spawn(async move {
            let guard = client.issuance.lock().await;

            if client.store.is_low()
                && let Err(error) = client.issue_locked().await
            {
                // The refill is best-effort, but a failure leaves the store low and the next
                // search may fail; this is degraded state rather than routine diagnostics.
                warn!(
                    "error.type" = "privacy_pass",
                    "exception.message" = %error,
                    "could not replenish Privacy Pass tokens"
                );
            }

            drop(guard);
            client.replenishing.store(false, Ordering::Release);
        });
    }
}

/// The retry deadline Privacy Pass earns from an issuance failure.
enum DegradeDeadline {
    /// The failure does not degrade Privacy Pass.
    Keep,
    /// Degrade until the end of the client's lifetime.
    Forever,
    /// Degrade until this duration from `now` has passed.
    Until(Duration),
}

/// Classifies an issuance failure for degradation.
fn degrade_deadline(error: &PrivacyPassError, now: SystemTime) -> DegradeDeadline {
    let PrivacyPassError::TokenStatus(error) = error else {
        return DegradeDeadline::Keep;
    };

    match error.status() {
        Some(StatusCode::TOO_MANY_REQUESTS) => {
            DegradeDeadline::Until(duration_until_next_month(now))
        }
        Some(StatusCode::FORBIDDEN) => DegradeDeadline::Forever,
        _ => DegradeDeadline::Keep,
    }
}

/// Returns the duration from `now` until the start of the next calendar month, in UTC.
///
/// Kagi's issuance quota is per calendar month ("epoch"), so a quota rejection is retried at
/// the next month boundary; the exact reset time is not documented, so the client assumes the
/// calendar month. A clock before the Unix epoch is treated as the epoch itself, and one
/// beyond the calendar crate's representable range counts as already past the boundary.
fn duration_until_next_month(now: SystemTime) -> Duration {
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let Ok(current) = OffsetDateTime::from_unix_timestamp(secs) else {
        return Duration::ZERO;
    };
    let (year, month, _) = current.date().to_calendar_date();
    let is_december = month == Month::December;
    let boundary = time::Date::from_calendar_date(
        if is_december { year + 1 } else { year },
        if is_december {
            Month::January
        } else {
            Month::try_from(u8::from(month) + 1).expect("the month after 1..=11 is in 2..=12")
        },
        1,
    )
    .expect("the first day of a month is a valid date")
    .with_time(Time::MIDNIGHT)
    .assume_utc();

    // The next month's boundary is strictly after `now`, so the difference is never negative.
    (boundary - current).unsigned_abs()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use reqwest::header::HeaderValue;
    use secrecy::SecretString;
    use time::{Month, OffsetDateTime, Time};

    use super::{
        MAX_TOKENS_PER_BATCH, PrivacyPassClient, PrivacyPassOptions, SystemTime,
        duration_until_next_month,
    };

    #[test]
    fn batch_size_is_clamped_to_the_request_limit() {
        let options = PrivacyPassOptions {
            tokens_to_stash: usize::MAX,
            max_tokens: 0,
            low_token_count: 0,
            state_path: None,
            fallback: false,
        };
        let (batch, capacity, low) = options.normalized();

        assert_eq!(usize::from(batch), MAX_TOKENS_PER_BATCH);
        assert_eq!(capacity, MAX_TOKENS_PER_BATCH);
        assert_eq!(low, 0);

        let options = PrivacyPassOptions {
            tokens_to_stash: 0,
            max_tokens: 0,
            low_token_count: 0,
            state_path: None,
            fallback: false,
        };

        assert_eq!(options.normalized().0, 1);
    }

    /// Builds a Privacy Pass client for degradation tests.
    fn test_client(fallback: bool) -> PrivacyPassClient {
        PrivacyPassClient::new(
            reqwest::Url::parse("https://kagi.com/").expect("the base url parses"),
            Arc::new(SecretString::from("test-token")),
            HeaderValue::from_static("en-US"),
            Duration::from_secs(5),
            HeaderValue::from_static("kagi/1.0"),
            &PrivacyPassOptions {
                fallback,
                ..PrivacyPassOptions::default()
            },
        )
        .expect("the test client builds")
    }

    #[test]
    fn degradation_reports_and_clears_itself() {
        let client = test_client(true);

        assert!(!client.degraded());

        // A future deadline is degraded; once it passes, `degraded` clears the state.
        client.set_degraded(Some(Instant::now() + Duration::from_secs(60)));
        assert!(client.degraded());

        client.set_degraded(Some(
            Instant::now()
                .checked_sub(Duration::from_secs(60))
                .expect("the instant has a predecessor"),
        ));
        assert!(!client.degraded());
        assert!(!client.degraded(), "the recovered state is cleared");

        // A deadline without an instant degrades for the client's lifetime.
        client.set_degraded(None);
        assert!(client.degraded());
    }

    impl PrivacyPassClient {
        /// Sets the degradation state directly, so the read path is testable without a real
        /// quota rejection from the issuer. A `None` deadline degrades forever.
        fn set_degraded(&self, until: Option<Instant>) {
            *self.lock_degraded() = until;
            self.is_degraded.store(true, Ordering::Release);
        }
    }

    #[test]
    fn the_month_boundary_duration_lands_on_the_first_of_the_next_month() {
        for &(year, month, day, second_of_day) in &[
            (2026, 1, 31, 86_399), // the last second of a 31-day month
            (2026, 2, 1, 0),       // the first second of a month
            (2024, 2, 29, 43_200), // leap-day noon, rolling into March
            (2026, 4, 30, 86_399), // the last second before a 31-day month
            (2026, 12, 31, 0),     // the year rollover
            (1970, 1, 1, 1),
        ] {
            let month = Month::try_from(month).expect("a valid month");
            let date =
                time::Date::from_calendar_date(year, month, day).expect("a valid sample date");
            let timestamp = date.with_time(Time::MIDNIGHT).assume_utc().unix_timestamp()
                + i64::from(second_of_day);
            let now = SystemTime::UNIX_EPOCH
                + Duration::from_secs(u64::try_from(timestamp).expect("the sample is post-epoch"));

            let duration = duration_until_next_month(now);
            let boundary = OffsetDateTime::from_unix_timestamp(
                timestamp + i64::try_from(duration.as_secs()).expect("the boundary is in range"),
            )
            .expect("the boundary is representable");

            assert_eq!(boundary.time(), Time::MIDNIGHT, "the boundary is midnight");
            assert_eq!(boundary.day(), 1, "the boundary is the first of the month");

            let (expected_year, expected_month) = if month == Month::December {
                (year + 1, 1)
            } else {
                (year, u8::from(month) + 1)
            };

            assert_eq!(boundary.year(), expected_year, "{date}");
            assert_eq!(u8::from(boundary.month()), expected_month, "{date}");
        }
    }

    #[test]
    fn the_epoch_month_has_31_days_of_quota_left() {
        assert_eq!(
            duration_until_next_month(SystemTime::UNIX_EPOCH),
            Duration::from_hours(31 * 24)
        );
    }
}
