//! The shared application state and the clients it owns.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use wreq::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue, USER_AGENT};
use wreq::redirect::Policy;
use wreq_util::Emulation;

use crate::audit::AuditConfig;
use crate::content::ContentPipeline;

/// Errors that can occur while constructing the application state.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// A configured header value is invalid.
    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] wreq::header::InvalidHeaderValue),
    /// The HTTP client failed to build.
    #[error("could not build http client: {0}")]
    HttpClient(#[from] wreq::Error),
    /// The Kagi client failed to build.
    #[error("could not build kagi client: {0}")]
    KagiClient(#[from] kagi::Error),
}

/// The shared application state passed to the handlers.
pub struct AppState {
    /// The HTTP client used for fetching pages.
    ///
    /// Redirects are disabled: the fetch handlers follow them manually so that each hop can be
    /// counted and reported in the response metrics.
    pub(crate) client: wreq::Client,
    /// The client used for searching with Kagi.
    pub(crate) kagi_client: kagi::Client,
    /// Service-level admission gate for searches; each search acquires a permit and extra
    /// requests wait here until a slot frees up. Sized from [`kagi::Client::max_sessions`], so
    /// the reported queue wait covers service admission. The Kagi client also bounds its own
    /// in-flight searches as a library-level invariant; the two limits intentionally mirror
    /// each other.
    pub(crate) search_gate: tokio::sync::Semaphore,
    /// The default request headers sent with every fetch, kept for reporting.
    pub(crate) default_headers: HeaderMap,
    /// The audit sink, header policy, and body-capture limit for request events.
    pub(crate) audit: AuditConfig,
    /// The optional content pipeline that stores request and response bodies.
    pub(crate) content: Option<Arc<ContentPipeline>>,
}

impl AppState {
    /// Constructs the application state: the shared HTTP client and the Kagi client.
    ///
    /// `max_sessions` is clamped to the service's accepted range (1 through 64).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP or Kagi clients fail to build.
    pub fn try_new(
        user_agent: &str,
        kagi_token: SecretString,
        timeout: Duration,
        max_sessions: usize,
        privacy_pass: bool,
        privacy_pass_fallback: bool,
    ) -> Result<Self, BuildError> {
        Self::try_new_with_audit(
            user_agent,
            kagi_token,
            timeout,
            max_sessions,
            privacy_pass,
            privacy_pass_fallback,
            None,
            AuditConfig::default(),
        )
    }

    /// Constructs the application state with an explicit audit configuration.
    ///
    /// `privacy_pass_fallback` retries a search with session authentication when Privacy Pass
    /// fails; it links the retried searches to the account, so it is off by default.
    /// `privacy_pass_state` is the optional file used to persist unspent Privacy Pass tokens
    /// across restarts; `None` keeps them in memory only.
    ///
    /// `max_sessions` is clamped to the service's accepted range (1 through 64).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP or Kagi clients fail to build.
    #[expect(
        clippy::too_many_arguments,
        reason = "the explicit constructor mirrors the configuration fields its callers pass"
    )]
    pub fn try_new_with_audit(
        user_agent: &str,
        kagi_token: SecretString,
        timeout: Duration,
        max_sessions: usize,
        privacy_pass: bool,
        privacy_pass_fallback: bool,
        privacy_pass_state: Option<&Path>,
        audit: AuditConfig,
    ) -> Result<Self, BuildError> {
        let max_sessions = max_sessions.clamp(1, crate::config::MAX_SEARCH_SESSIONS);
        let kagi_client = kagi::Client::with_token_and_options(
            kagi_token,
            &kagi::ClientOptions {
                max_sessions,
                privacy_pass: privacy_pass.then(|| kagi::PrivacyPassOptions {
                    state_path: privacy_pass_state.map(Path::to_path_buf),
                    fallback: privacy_pass_fallback,
                    ..kagi::PrivacyPassOptions::default()
                }),
                // Keep the Kagi client on the same timeout as the page-fetch client.
                timeout,
                ..kagi::ClientOptions::default()
            },
        )?;

        Self::try_new_with_kagi_client_and_audit(user_agent, kagi_client, timeout, audit)
    }

    /// Constructs the application state around an existing Kagi client.
    ///
    /// The page-fetching HTTP client is built with the same production configuration as
    /// [`AppState::try_new`]. This is the injection point for a Kagi client pointed at another
    /// endpoint, such as a local test server. The audit configuration defaults to discarding
    /// events.
    ///
    /// The search gate capacity is taken from [`kagi::Client::max_sessions`], clamped to the
    /// service's accepted range (1 through 64), so the service admits no more simultaneous
    /// searches than either limit allows.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client fails to build or a configured header is invalid.
    pub fn try_new_with_kagi_client(
        user_agent: &str,
        kagi_client: kagi::Client,
        timeout: Duration,
    ) -> Result<Self, BuildError> {
        Self::try_new_with_kagi_client_and_audit(
            user_agent,
            kagi_client,
            timeout,
            AuditConfig::default(),
        )
    }

    /// Constructs the application state around an existing Kagi client and audit configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client fails to build or a configured header is invalid.
    pub fn try_new_with_kagi_client_and_audit(
        user_agent: &str,
        kagi_client: kagi::Client,
        timeout: Duration,
        audit: AuditConfig,
    ) -> Result<Self, BuildError> {
        let default_headers = default_headers(user_agent)?;
        let max_sessions = kagi_client
            .max_sessions()
            .clamp(1, crate::config::MAX_SEARCH_SESSIONS);

        let client = wreq::Client::builder()
            // The newest Firefox profile wreq-util ships; the snapshot that the Kagi client
            // mirrors was captured from Firefox 159.
            .emulation(Emulation::Firefox151)
            .default_headers(default_headers.clone())
            // Redirects are followed manually by the fetch handlers, which count each hop and
            // report the chain in the response metrics.
            .redirect(Policy::none())
            .timeout(timeout)
            .build()?;

        Ok(Self {
            client,
            kagi_client,
            search_gate: tokio::sync::Semaphore::new(max_sessions),
            default_headers,
            audit,
            content: None,
        })
    }

    /// Enables content storage for request and response bodies.
    ///
    /// Content is disabled by default, so existing callers keep the previous behavior until they
    /// opt in with this builder.
    #[must_use]
    pub fn with_content(mut self, content: Arc<ContentPipeline>) -> Self {
        self.content = Some(content);

        self
    }

    /// Returns the content pipeline, when content storage is enabled.
    pub(crate) fn content(&self) -> Option<&ContentPipeline> {
        self.content.as_deref()
    }

    /// Returns the Kagi client used for searches.
    #[must_use]
    pub const fn kagi_client(&self) -> &kagi::Client {
        &self.kagi_client
    }

    /// Returns the audit configuration for request events.
    pub(crate) const fn audit(&self) -> &AuditConfig {
        &self.audit
    }
}

/// Returns the request headers layered on top of the emulated browser profile.
///
/// `Accept-Encoding` must be set explicitly: like reqwest, wreq does not advertise the header
/// itself even though it decompresses responses — and its absence is enough to get flagged.
fn default_headers(user_agent: &str) -> Result<HeaderMap, BuildError> {
    let mut headers = HeaderMap::new();

    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br, zstd"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_str(user_agent)?);

    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a state whose Kagi client is configured for `max_sessions` sessions.
    fn state_with_max_sessions(max_sessions: usize) -> AppState {
        let kagi_client = kagi::Client::with_token_and_options(
            "test-token",
            &kagi::ClientOptions {
                max_sessions,
                ..kagi::ClientOptions::default()
            },
        )
        .expect("valid client options");

        AppState::try_new_with_kagi_client("test-agent", kagi_client, Duration::from_secs(5))
            .expect("valid state")
    }

    #[test]
    fn sizes_the_search_gate_from_the_client_limit() {
        let state = state_with_max_sessions(3);

        assert_eq!(state.kagi_client().max_sessions(), 3);
        assert_eq!(state.search_gate.available_permits(), 3);
    }

    #[test]
    fn clamps_the_search_gate_to_at_least_one() {
        // The Kagi client normalizes an invalid zero limit to one session.
        let state = state_with_max_sessions(0);

        assert_eq!(state.kagi_client().max_sessions(), 1);
        assert_eq!(state.search_gate.available_permits(), 1);
    }

    #[test]
    fn clamps_the_search_gate_to_the_service_limit() {
        let state = state_with_max_sessions(crate::config::MAX_SEARCH_SESSIONS + 1);

        assert_eq!(
            state.kagi_client().max_sessions(),
            crate::config::MAX_SEARCH_SESSIONS + 1
        );
        assert_eq!(
            state.search_gate.available_permits(),
            crate::config::MAX_SEARCH_SESSIONS
        );
    }

    #[test]
    fn switches_the_search_authentication_mode() {
        let session = AppState::try_new(
            "test-agent",
            SecretString::from("test-token"),
            Duration::from_secs(30),
            2,
            false,
            false,
        )
        .expect("valid state");
        assert!(session.kagi_client().privacy_pass().is_none());

        let privacy_pass = AppState::try_new(
            "test-agent",
            SecretString::from("test-token"),
            Duration::from_secs(30),
            2,
            true,
            false,
        )
        .expect("valid state");
        let privacy_pass = privacy_pass
            .kagi_client()
            .privacy_pass()
            .expect("privacy pass is enabled");
        assert!(
            !privacy_pass.fallback_enabled(),
            "the fallback is off unless configured"
        );

        let fallback = AppState::try_new(
            "test-agent",
            SecretString::from("test-token"),
            Duration::from_secs(30),
            2,
            true,
            true,
        )
        .expect("valid state");
        assert!(
            fallback
                .kagi_client()
                .privacy_pass()
                .expect("privacy pass is enabled")
                .fallback_enabled()
        );
    }

    #[test]
    fn builds_the_default_headers() {
        let headers = default_headers("test-agent").expect("valid user agent");

        assert_eq!(
            headers
                .get(USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("test-agent")
        );
        assert_eq!(
            headers
                .get(ACCEPT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip, deflate, br, zstd")
        );
    }

    #[test]
    fn defaults_to_the_null_audit_sink_and_default_header_policy() {
        let state = AppState::try_new(
            "test-agent",
            SecretString::from("test-token"),
            Duration::from_secs(30),
            2,
            false,
            false,
        )
        .expect("valid state");

        assert_eq!(
            state.audit().max_body_bytes(),
            crate::config::DEFAULT_CONTENT_MAX_BODY_BYTES
        );
        assert!(!state.audit().header_policy().has_hmac_key());
    }

    #[test]
    fn rejects_invalid_user_agents() {
        let error = default_headers("bad\nagent").expect_err("invalid header value");

        assert!(matches!(error, BuildError::InvalidHeader(_)));
    }
}
