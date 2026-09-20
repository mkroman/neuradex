//! A client and parser for Kagi Search.
//!
//! This crate provides an asynchronous [`Client`] for querying [Kagi](https://kagi.com) with a
//! session token and parsing the streamed responses into [`SearchResult`]s and [`ImageResult`]s.
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

pub use client::Client;
pub use error::Error;

/// Kagi base URL.
pub const BASE_URL: &str = "https://kagi.com";
/// A realistic browser user agent sent with requests, as Kagi serves an altered experience to
/// clients it identifies as non-browser.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:155.0) Gecko/20100101 Firefox/155.0";
/// The duration before a HTTP request times out.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// The duration of a single session. Once this duration has passed, a new session will be created.
pub const SESSION_DURATION: Duration = Duration::from_mins(15);
/// The default `Accept-Language` header sent with requests.
pub const LANGUAGE: &str = "en-US,en;q=0.9";

/// Options for configuring a Kagi [`Client`].
#[derive(Clone, Debug)]
pub struct ClientOptions {
    /// The duration before an HTTP request times out.
    pub timeout: Duration,
    /// The `User-Agent` header sent with requests.
    pub user_agent: String,
    /// The duration of a single session.
    pub session_duration: Duration,
    /// The `Accept-Language` header sent with requests.
    pub language: String,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            timeout: HTTP_TIMEOUT,
            user_agent: USER_AGENT.to_string(),
            session_duration: SESSION_DURATION,
            language: LANGUAGE.to_string(),
        }
    }
}

/// Represents a single search result obtained from the search operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    /// The title of the search result.
    pub title: String,
    /// The URL of the search result.
    pub url: String,
    /// The description.
    pub description: String,
}

/// Represents a single image result obtained from the image search operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageResult {
    /// The title of the image.
    pub title: String,
    /// The URL of the page the image was found on.
    pub page_url: String,
    /// The URL of the full-size image.
    pub image_url: String,
    /// The URL of the thumbnail served through Kagi's image proxy.
    pub thumbnail_url: String,
    /// The width of the image in pixels.
    pub width: u32,
    /// The height of the image in pixels.
    pub height: u32,
    /// The hostname of the page the image was found on.
    pub host: String,
    /// The rank of the image within the results.
    pub rank: u32,
}
