//! API v1: routes and handlers.

pub mod error;
pub mod extract;
pub mod fetch;
pub mod peek;
pub mod redirect;
pub mod search;
pub mod stream;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use secrecy::SecretString;
use wreq::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue, USER_AGENT};
use wreq::redirect::Policy;
use wreq_util::Emulation;

pub use error::ApiError;

/// The maximum number of redirects the fetch endpoints will follow.
pub const MAX_REDIRECTS: u32 = 5;

/// The maximum number of searches that may be in flight at once; additional requests queue.
pub const MAX_CONCURRENT_SEARCHES: usize = 2;

/// The maximum value accepted by the search endpoint's `timeout` parameter, in seconds.
pub const MAX_SEARCH_TIMEOUT_SECS: u64 = 30;

/// Errors that can occur while constructing the application state.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// A configured header value is invalid.
    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] wreq::header::InvalidHeaderValue),
    /// The HTTP client failed to build.
    #[error("could not build http client: {0}")]
    Client(#[from] wreq::Error),
    /// The Kagi client failed to build.
    #[error("could not build kagi client: {0}")]
    Kagi(#[from] kagi::Error),
}

/// The shared application state passed to the handlers.
pub struct AppState {
    /// The HTTP client used for fetching pages.
    ///
    /// Redirects are disabled: the fetch handlers follow them manually so that each hop can be
    /// counted and reported in the response metrics.
    pub client: wreq::Client,
    /// The client used for searching with Kagi.
    pub kagi_client: kagi::Client,
    /// Gates the number of searches in flight; each search acquires a permit, extra requests
    /// wait here until a slot frees up.
    pub search_gate: Arc<tokio::sync::Semaphore>,
    /// The default request headers sent with every fetch, kept for reporting.
    pub default_headers: HeaderMap,
}

impl AppState {
    /// Constructs the application state: the shared HTTP client and the Kagi client.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP or Kagi clients fail to build.
    pub fn new(
        user_agent: &str,
        kagi_token: SecretString,
        timeout: Duration,
        max_sessions: usize,
    ) -> Result<Self, BuildError> {
        let default_headers = default_headers(user_agent)?;

        let client = wreq::Client::builder()
            .emulation(Emulation::Firefox142)
            .default_headers(default_headers.clone())
            // Redirects are followed manually by the fetch handlers, which count each hop and
            // report the chain in the response metrics.
            .redirect(Policy::none())
            .timeout(timeout)
            .build()?;

        let kagi_client = kagi::Client::with_token_and_options(
            kagi_token,
            &kagi::ClientOptions {
                max_sessions,
                ..kagi::ClientOptions::default()
            },
        )?;

        Ok(Self {
            client,
            kagi_client,
            search_gate: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_SEARCHES)),
            default_headers,
        })
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
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(user_agent).map_err(BuildError::from)?,
    );

    Ok(headers)
}

/// Builds the v1 API router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/peek", get(peek::peek))
        .route("/v1/fetch", get(fetch::fetch))
        .route("/v1/search", get(search::search))
        .with_state(Arc::new(state))
}

async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}
