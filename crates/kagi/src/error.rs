//! Error types for the Kagi client.

use std::error::Error as StdError;
use std::fmt;
use std::fmt::Write as _;

use reqwest::StatusCode;

/// A request URL rendered for logs and error messages, without its query string, fragment, or
/// userinfo.
///
/// Kagi puts its login token in the query string, and which parameter carries a credential
/// cannot be recognized reliably, so the query is dropped outright; the scheme, host, port and
/// path still say which resource was requested. The raw URL is never stored, and both
/// [`fmt::Display`] and [`fmt::Debug`] render the redacted form.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RedactedUrl(String);

impl RedactedUrl {
    /// Returns `url` as it may be logged: without its query string, fragment, or userinfo.
    fn from_url(url: &reqwest::Url) -> Self {
        // A URL that cannot be a base (e.g. `mailto:`) has no authority to redact and no `//`
        // after the scheme, and a URL without a host serializes its path differently — compose
        // those the generic way: clone, strip the query and fragment, and let the userinfo
        // calls fail (there is no authority to take it from).
        if url.cannot_be_a_base() || !url.has_host() {
            let mut redacted = url.clone();

            redacted.set_query(None);
            redacted.set_fragment(None);
            let _ = redacted.set_username("");
            let _ = redacted.set_password(None);

            return Self(redacted.to_string());
        }

        // The userinfo is left out rather than removed, the port only when it is non-default
        // (`Url::port` answers `None` for the scheme's default), and the path is the encoded
        // form the URL serializes with.
        let mut redacted = String::with_capacity(url.as_str().len());
        redacted.push_str(url.scheme());
        redacted.push_str("://");
        redacted.push_str(url.host_str().unwrap_or_default());

        if let Some(port) = url.port() {
            let _ = write!(redacted, ":{port}");
        }

        redacted.push_str(url.path());

        Self(redacted)
    }

    /// Returns the redacted URL as a string.
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RedactedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `reqwest::Error` whose URL has been redacted and whose message is rendered once.
///
/// Kagi puts its login token in the query string, and reqwest prints the request URL in both
/// `Display` and `Debug`, so a wrapped error would put the token into every log line, span
/// attribute or channel reply that formats it. The URL is stripped from the error — which is
/// dropped — and kept here in its redacted form, so every formatted output is free of it by
/// construction.
///
/// [`Display`](std::fmt::Display) is the short rendering a channel reply needs (`could not
/// connect (kagi.com)`), while [`RequestError::full`] is what a log line gets: error kind,
/// source chain and redacted URL. [`StdError::source`] reports no source — the chain is
/// already rendered by `full()`, and returning it would print it twice.
#[derive(Debug)]
pub struct RequestError {
    /// The transport error's own message, with its URL stripped ("error sending request").
    kind: String,
    /// [`kind`] with the source chain and the redacted URL appended — what a log line gets.
    detail: String,
    /// The request URL without its query string, fragment, or userinfo.
    url: Option<RedactedUrl>,
    /// The host the request went to, for the short rendering.
    host: Option<String>,
    /// The HTTP status, if the error came from an error response status.
    status: Option<StatusCode>,
    /// The classified transport failure.
    failure: Failure,
}

impl RequestError {
    /// Returns the request URL without its query string, fragment, or userinfo, if the error
    /// carries one.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        self.url.as_ref().map(RedactedUrl::as_str)
    }

    /// Returns the full rendering — error kind, source chain and redacted URL — for a log line,
    /// where naming the request that failed matters more than staying short.
    #[must_use]
    pub fn full(&self) -> &str {
        &self.detail
    }

    /// Returns the HTTP status of the response that produced the error, if any.
    #[must_use]
    pub const fn status(&self) -> Option<StatusCode> {
        self.status
    }

    /// Returns the [`error.type`] value for this failure, for a span or log field.
    ///
    /// [`error.type`]: https://opentelemetry.io/docs/specs/semconv/registry/attributes/error/
    #[must_use]
    pub const fn error_type(&self) -> &'static str {
        self.failure.error_type()
    }
}

impl From<reqwest::Error> for RequestError {
    fn from(error: reqwest::Error) -> Self {
        let url = error.url().map(RedactedUrl::from_url);
        let host = error
            .url()
            .and_then(|url| url.host_str().map(str::to_string));
        let status = error.status();
        let failure = Failure::from_flags(error.is_timeout(), error.is_connect());

        let error = error.without_url();
        let kind = error.to_string();
        let detail = detail(&kind, &error, url.as_ref().map(RedactedUrl::as_str));

        Self {
            kind,
            detail,
            url,
            host,
            status,
            failure,
        }
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failure {
            Failure::TimedOut => f.write_str("request timed out")?,
            Failure::Connect => f.write_str("could not connect")?,
            Failure::Other => f.write_str(&self.kind)?,
        }

        self.host
            .as_deref()
            .map_or(Ok(()), |host| write!(f, " ({host})"))
    }
}

impl StdError for RequestError {}

/// Renders the message a log line gets: `kind`, its source chain and its redacted URL.
///
/// A link whose own [`Display`](std::fmt::Display) already ends the message with its source is
/// not appended a second time.
fn detail(kind: &str, error: &(dyn StdError + 'static), url: Option<&str>) -> String {
    let mut message = String::from(kind);
    let mut cause = error.source();

    while let Some(link) = cause {
        let text = link.to_string();

        if !text.is_empty() && !message.ends_with(&text) {
            let _ = write!(message, ": {text}");
        }

        cause = link.source();
    }

    if let Some(url) = url {
        let _ = write!(message, " for url ({url})");
    }

    message
}

/// The classified transport failure behind a [`RequestError`].
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// The request timed out.
    TimedOut,
    /// The request failed before it could be sent.
    Connect,
    /// Any other transport failure.
    Other,
}

impl Failure {
    /// Classifies the `timeout` and `connect` flags a client reports for an error.
    const fn from_flags(timeout: bool, connect: bool) -> Self {
        if timeout {
            Self::TimedOut
        } else if connect {
            Self::Connect
        } else {
            Self::Other
        }
    }

    /// The [`error.type`] value for this failure, for a span or log field.
    ///
    /// [`error.type`]: https://opentelemetry.io/docs/specs/semconv/registry/attributes/error/
    const fn error_type(self) -> &'static str {
        match self {
            Self::TimedOut => "timeout",
            Self::Connect => "connection_error",
            Self::Other => "other",
        }
    }
}

/// Errors that can occur while searching with Kagi.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The stream request could not be sent.
    #[error("could not send stream request")]
    StreamRequest(#[source] RequestError),
    /// The stream request returned an error HTTP status.
    #[error("stream request returned an error status")]
    StreamStatus(#[source] RequestError),
    /// The response body of the stream request could not be read.
    #[error("could not read response body of stream request")]
    StreamRequestBody(#[source] RequestError),
    /// The stream response could not be processed on a blocking task.
    #[error("could not process the response")]
    ParseTask(#[source] tokio::task::JoinError),
    /// The nonce request could not be sent.
    #[error("could not send nonce request")]
    RequestNonce(#[source] RequestError),
    /// The nonce response could not be read.
    #[error("could not read nonce response")]
    ReadNonce(#[source] RequestError),
    /// The session request could not be sent.
    #[error("could not send session request")]
    RequestSession(#[source] RequestError),
    /// The response did not include the `kagi_session` cookie, indicating an invalid login token.
    #[error("response did not include a valid kagi_session cookie - is the login token valid?")]
    SessionCookies,
    /// The response did not include a nonce.
    #[error("response did not include a nonce")]
    Nonce,
    /// A configured header value is invalid.
    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),
    /// A configured base URL is invalid.
    #[error("invalid base url: {0}")]
    InvalidBaseUrl(String),
    /// The HTTP client could not be built.
    #[error("could not build http client")]
    BuildClient(#[source] RequestError),
}

#[cfg(test)]
mod tests {
    use test_support::httpmock::MockServer;

    use super::*;

    /// The credential-looking query parameter placed in every test URL.
    const QUERY_SECRET: &str = "query-secret";
    /// The credential-looking password placed in the refused URL's userinfo.
    const PASSWORD_SECRET: &str = "password-secret";
    /// The credential-looking fragment placed in the refused URL.
    const FRAGMENT_SECRET: &str = "fragment-secret";

    /// Returns a URL whose connection is always refused: TCP port 0 is reserved and can never
    /// have a listener.
    fn refused_url() -> String {
        format!(
            "http://user:{PASSWORD_SECRET}@127.0.0.1:0/path?login_token={QUERY_SECRET}#\
             {FRAGMENT_SECRET}"
        )
    }

    /// Asserts that no rendering leaks one of the test credentials.
    fn assert_no_secrets(rendered: &str) {
        for secret in [QUERY_SECRET, PASSWORD_SECRET, FRAGMENT_SECRET] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }

    /// The redacted form keeps the scheme, host, port and path, and drops the query, fragment
    /// and userinfo from both display renderings.
    #[test]
    fn redacted_urls_drop_the_query_fragment_and_userinfo() {
        let url = reqwest::Url::parse(&refused_url()).expect("the test URL parses");
        let redacted = RedactedUrl::from_url(&url);

        assert_eq!(redacted.as_str(), "http://127.0.0.1:0/path");
        assert_no_secrets(&redacted.to_string());
        assert_no_secrets(&format!("{redacted:?}"));
    }

    /// The wrapper's guarantees hold for a real connection failure: no rendering leaks the
    /// query string, the short form is classified and host-only, the full form carries the
    /// redacted URL and the cause, and the chain ends at the wrapper.
    #[tokio::test]
    async fn request_errors_redact_the_url_but_keep_the_cause() {
        let error = reqwest::Client::new()
            .get(refused_url())
            .send()
            .await
            .expect_err("port zero has no listener");

        // reqwest attaches the request URL, credentials included, to the error.
        let error = RequestError::from(error);
        let message = error.to_string();
        let full = error.full();
        let debug = format!("{error:?}");

        for rendered in [message.as_str(), full, debug.as_str()] {
            assert_no_secrets(rendered);
        }

        assert_eq!(error.url(), Some("http://127.0.0.1:0/path"));
        assert!(full.contains("http://127.0.0.1:0/path"), "{full}");
        assert_eq!(error.error_type(), "connection_error");
        assert!(std::error::Error::source(&error).is_none(), "{debug}");
    }

    /// A response status is preserved for callers that invalidate a session on it, while the
    /// URL remains redacted.
    #[tokio::test]
    async fn request_errors_keep_the_response_status() {
        let server = MockServer::start();
        let response = server.mock(|when, then| {
            when.method("GET").path("/status");
            then.status(StatusCode::UNAUTHORIZED.as_u16());
        });
        let url = format!("{}/status?token={QUERY_SECRET}", server.base_url());

        let error = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect("the mock server responds")
            .error_for_status()
            .expect_err("the status is an error");
        let error = RequestError::from(error);
        let full = error.full();
        let debug = format!("{error:?}");

        assert_eq!(error.status(), Some(StatusCode::UNAUTHORIZED));
        assert_eq!(error.error_type(), "other");
        assert_eq!(
            error.url(),
            Some(format!("{}/status", server.base_url()).as_str())
        );
        assert_no_secrets(&error.to_string());
        assert_no_secrets(full);
        assert_no_secrets(&debug);
        response.assert();
    }
}
