//! Manual redirect following for the fetch endpoints.
//!
//! wreq's redirect policy has no per-request state, so redirects are followed by hand: each hop
//! is counted, and the chain of intermediate responses can be reported in the metrics.

use std::time::{Duration, Instant};

use tracing::Instrument;
use url::Url;
use wreq::StatusCode;
use wreq::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};

use crate::api::v1::error::ApiError;
use crate::metrics::{FetchMetrics, RedirectHop, duration_ms};
use crate::telemetry;

/// The facts of the final response, captured before the body is consumed.
pub(crate) struct ResponseInfo {
    /// The status code of the final response.
    pub status: u16,
    /// The HTTP version of the final response, e.g. `HTTP/1.1`.
    pub http_version: String,
    /// The content type of the final response, without parameters.
    pub content_type: Option<String>,
    /// The content length announced by the final response, if any.
    pub content_length: Option<u64>,
}

impl ResponseInfo {
    /// Captures the facts of `response` that survive the consumption of its body.
    #[must_use]
    pub fn capture(response: &wreq::Response) -> Self {
        let headers = response.headers();

        Self {
            status: response.status().as_u16(),
            // `http::Version` has no `Display` impl; its `Debug` impl is the canonical
            // version string, e.g. `HTTP/1.1`.
            http_version: format!("{:?}", response.version()),
            content_type: headers
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(media_type),
            content_length: headers
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok()),
        }
    }
}

/// Returns the media type — the first, lowercased parameter-free segment — of a content type.
#[must_use]
pub(crate) fn media_type(content_type: &str) -> String {
    let media = content_type
        .split_once(';')
        .map_or(content_type, |(media, _)| media);

    media.trim().to_ascii_lowercase()
}

/// The outcome of a fetch with redirects followed manually.
pub(crate) struct FetchOutcome {
    /// The URL of the final response, after following redirects.
    pub url: Url,
    /// The number of redirects followed.
    pub followed: u32,
    /// The intermediate redirect hops, in order.
    pub hops: Vec<RedirectHop>,
    /// The facts of the final response.
    pub info: ResponseInfo,
    /// The time from the first request until the headers of the final response were received.
    pub headers_elapsed: Duration,
}

impl FetchOutcome {
    /// Assembles the request metrics from this fetch outcome and the body read.
    #[must_use]
    pub(crate) fn into_metrics(
        self,
        bytes_read: u64,
        truncated: bool,
        started: Instant,
    ) -> FetchMetrics {
        FetchMetrics {
            status: self.info.status,
            final_url: self.url.to_string(),
            http_version: self.info.http_version,
            content_type: self.info.content_type,
            content_length: self.info.content_length,
            bytes_read,
            ttfb_ms: duration_ms(self.headers_elapsed),
            total_ms: duration_ms(started.elapsed()),
            redirects_followed: self.followed,
            redirects: self.hops,
            truncated,
        }
    }
}

/// Sends GET requests, following at most `max_redirects` redirects by hand.
///
/// When the budget is exhausted on a redirect response, that response is returned as the final
/// response: `followed == max_redirects` together with a 3xx status identifies it. Intermediate
/// hops are only recorded when `collect_hops` is set; `followed` always counts them. Only
/// redirects to `http` or `https` URLs are followed, so the final response may be a redirect.
pub(crate) async fn fetch_with_redirects(
    client: &wreq::Client,
    url: Url,
    max_redirects: u32,
    collect_hops: bool,
) -> Result<(wreq::Response, FetchOutcome), ApiError> {
    let started = Instant::now();
    let mut current = url;
    let mut hops: Vec<RedirectHop> = Vec::new();
    let mut followed: u32 = 0;

    loop {
        let span = telemetry::http_client_span("GET", &current, followed);
        let response = client
            .get(current.as_str())
            .send()
            .instrument(span.clone())
            .await
            .map_err(|error| {
                telemetry::record_error(&span, &error);

                // The error's `Display`/`Debug` renderings include the request URI, which can
                // carry a user's query or userinfo. Strip it before the error reaches the 5xx
                // log line; the span already recorded the sanitized URL.
                ApiError::upstream(error.without_uri())
            })?;
        telemetry::record_response(&span, &response);

        if let Some(target) = redirect_target(&response, &current)
            && followed < max_redirects
        {
            followed += 1;

            if collect_hops {
                hops.push(RedirectHop {
                    status: response.status().as_u16(),
                    url: current.to_string(),
                });
            }

            current = target;

            continue;
        }

        let info = ResponseInfo::capture(&response);

        return Ok((
            response,
            FetchOutcome {
                url: current,
                followed,
                hops,
                info,
                headers_elapsed: started.elapsed(),
            },
        ));
    }
}

/// Whether `status` is a redirect the fetch endpoints follow.
///
/// Only the statuses that resend the request at another URL are followed: `304 Not Modified`
/// and the other 3xx statuses are final responses.
fn is_redirect_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

/// Returns the redirect target of a redirect response, if it is one.
fn redirect_target(response: &wreq::Response, base: &Url) -> Option<Url> {
    if !is_redirect_status(response.status()) {
        return None;
    }

    let location = response.headers().get(LOCATION)?.to_str().ok()?;

    redirect_location(base, location)
}

/// Resolves a `Location` header value against `base`, keeping only absolute http(s) targets.
fn redirect_location(base: &Url, location: &str) -> Option<Url> {
    base.join(location.trim())
        .ok()
        .filter(|target| matches!(target.scheme(), "http" | "https"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn redacts_urls_from_upstream_errors() {
        let client = wreq::Client::new();
        let url =
            Url::parse("http://user:password@127.0.0.1:0/path?token=query-secret#fragment-secret")
                .expect("the test URL parses");

        let Err(error) = fetch_with_redirects(&client, url, 0, false).await else {
            panic!("port zero has no listener");
        };
        let rendered = format!("{error:?}");

        for secret in ["user", "password", "query-secret", "fragment-secret"] {
            assert!(!rendered.contains(secret), "the error leaked {secret:?}");
        }
    }

    #[test]
    fn extracts_media_types() {
        assert_eq!(media_type("text/html; charset=utf-8"), "text/html");
        assert_eq!(media_type(" Application/JSON "), "application/json");
        assert_eq!(media_type(""), "");
    }

    #[test]
    fn follows_only_redirect_statuses() {
        for status in [
            StatusCode::MOVED_PERMANENTLY,
            StatusCode::FOUND,
            StatusCode::SEE_OTHER,
            StatusCode::TEMPORARY_REDIRECT,
            StatusCode::PERMANENT_REDIRECT,
        ] {
            assert!(is_redirect_status(status), "{status}");
        }

        let unassigned = StatusCode::from_u16(306).expect("306 is a valid status code");

        for status in [
            StatusCode::MULTIPLE_CHOICES,
            StatusCode::NOT_MODIFIED,
            StatusCode::USE_PROXY,
            unassigned,
        ] {
            assert!(!is_redirect_status(status), "{status}");
        }
    }

    #[test]
    fn follows_only_http_and_https_targets() {
        let base = Url::parse("https://example.com/start").expect("valid url");

        assert_eq!(
            redirect_location(&base, "/next").map(|url| url.to_string()),
            Some("https://example.com/next".to_owned())
        );
        assert_eq!(
            redirect_location(&base, " http://other.example/ ").map(|url| url.to_string()),
            Some("http://other.example/".to_owned())
        );

        for target in [
            "ftp://example.com/file",
            "file:///etc/passwd",
            "mailto:user@example.com",
        ] {
            assert!(
                redirect_location(&base, target).is_none(),
                "followed the non-http target {target}"
            );
        }
    }
}
