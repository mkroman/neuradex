//! Error types for the Kagi client.

/// Errors that can occur while searching with Kagi.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The stream request could not be sent.
    #[error("could not send stream request")]
    StreamRequest(#[source] reqwest::Error),
    /// The stream request returned an error HTTP status.
    #[error("stream request returned an error status")]
    StreamStatus(#[source] reqwest::Error),
    /// The response body of the stream request could not be read.
    #[error("could not read response body of stream request")]
    StreamRequestBody(#[source] reqwest::Error),
    /// The nonce request could not be sent.
    #[error("could not send nonce request")]
    RequestNonce(#[source] reqwest::Error),
    /// The nonce response could not be read.
    #[error("could not read nonce response")]
    ReadNonce(#[source] reqwest::Error),
    /// The session request could not be sent.
    #[error("could not send session request")]
    RequestSession(#[source] reqwest::Error),
    /// The response did not include session cookies, indicating an invalid login token.
    #[error("response did not include session valid cookies - is the login token valid?")]
    SessionCookies,
    /// The response did not include a nonce.
    #[error("response did not include a nonce")]
    Nonce,
    /// A configured header value is invalid.
    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),
    /// The HTTP client could not be built.
    #[error("could not build http client")]
    BuildClient(#[source] reqwest::Error),
}
