//! A client and parser for Kagi Search.
//!
//! This crate provides an asynchronous [`Client`] for querying [Kagi](https://kagi.com) with a
//! session token and parsing the streamed responses into [`SearchResult`]s and [`ImageResult`]s.
//!
//! Searches can also use anonymous Privacy Pass tokens via [`Client::with_privacy_pass`] or
//! [`ClientOptions::privacy_pass`]. The session token is then used only to issue tokens.
//!
//! ## Features
//!
//! - **`default-tls`** *(enabled by default)*: selects the default TLS backend (**`rustls`**).
//!   Disable default features and choose a TLS backend explicitly with **`rustls`**,
//!   **`native-tls`** or **`native-tls-vendored`**.
//! - **`gzip`**, **`brotli`**, **`deflate`**, **`zstd`** *(all enabled by default)*:
//!   decompression of compressed responses.
//!
//! ## Quick Start
//!
//! To get started, add this crate to your `Cargo.toml`. The main entry points for searching are
//! the [`Client::search`] and [`Client::images`] methods.
//!
//! ```rust,no_run
//! use kagi::{Client, Error};
//!
//! # async fn example() -> Result<(), Error> {
//! // Create a new client with a Kagi session token.
//! let client = Client::with_token("kagi session token");
//!
//! // Search for the given query.
//! let results = client.search("rust programming language").await?;
//!
//! if let Some(result) = results.first() {
//!     println!("{} - {}", result.title, result.url);
//! }
//!
//! // Or search for images.
//! let images = client.images("ferrous wheel").await?;
//!
//! if let Some(image) = images.first() {
//!     println!("{} ({}x{}) {}", image.title, image.width, image.height, image.image_url);
//! }
//! # Ok(())
//! # }
//! ```

// Allow repetition of structure name instead of replacing with self as the output from
// rust-analyzer becomes more readable
#![allow(clippy::use_self)]

use std::time::Duration;

mod client;
mod error;
mod model;
mod parse;
mod privacy_pass;

pub use client::Client;
pub use error::{Error, RequestError};
pub use model::{ImageResult, SearchResult};
pub use privacy_pass::{
    MAX_TOKENS_PER_BATCH, PrivacyPassClient, PrivacyPassError, PrivacyPassOptions, PrivacyToken,
    TokenStore,
};

/// Kagi base URL.
pub const BASE_URL: &str = "https://kagi.com";
/// A realistic browser user agent sent with requests, as Kagi serves an altered experience to
/// clients it identifies as non-browser.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:159.0) Gecko/20100101 Firefox/159.0";
/// The duration before a HTTP request times out.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// The duration of a single session. Once this duration has passed, the session is refreshed
/// with a new nonce.
pub const SESSION_DURATION: Duration = Duration::from_mins(15);
/// The default `Accept-Language` header sent with requests.
pub const LANGUAGE: &str = "en-US,en;q=0.9";
/// The default maximum number of simultaneous sessions.
pub const DEFAULT_MAX_SESSIONS: usize = 2;

/// Options for configuring a Kagi [`Client`].
#[derive(Clone, Debug)]
pub struct ClientOptions {
    /// The base URL of the Kagi service.
    ///
    /// Defaults to [`BASE_URL`]. Overriding it is useful for testing against a local mock
    /// server or for pointing the client at a compatible deployment. The URL must use `http`
    /// or `https` and must not carry a query or fragment; a path prefix is preserved and
    /// treated as a directory.
    pub base_url: String,
    /// The duration before an HTTP request times out.
    pub timeout: Duration,
    /// The `User-Agent` header sent with requests.
    pub user_agent: String,
    /// The duration of a single session.
    pub session_duration: Duration,
    /// The `Accept-Language` header sent with requests.
    pub language: String,
    /// The maximum number of simultaneous searches.
    ///
    /// In session mode, the client keeps a pool of persistent sessions, each with its own nonce
    /// and refresh cycle. Every in-flight search uses its own session; an inrush of requests
    /// grows the pool by establishing sessions one at a time, and requests beyond the pool
    /// capacity wait for a session to free up.
    ///
    /// In Privacy Pass mode there is no session pool; the same value bounds the number of
    /// in-flight searches directly. Values below 1 are treated as 1.
    pub max_sessions: usize,
    /// Privacy Pass authentication settings.
    ///
    /// When set, searches redeem anonymous Privacy Pass tokens instead of sending the session
    /// token, session cookies, or `X-Kagi-Authorization`. The configured session token is still
    /// used to authorize token issuance.
    pub privacy_pass: Option<PrivacyPassOptions>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            base_url: BASE_URL.to_string(),
            timeout: HTTP_TIMEOUT,
            user_agent: USER_AGENT.to_string(),
            session_duration: SESSION_DURATION,
            language: LANGUAGE.to_string(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            privacy_pass: None,
        }
    }
}
