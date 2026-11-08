//! Manual redirect following for the fetch endpoints.
//!
//! wreq's redirect policy has no per-request state, so redirects are followed by hand: each hop
//! is counted, and the chain of intermediate responses can be reported in the metrics.

use std::time::{Duration, Instant};

use url::Url;
use wreq::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};

use crate::api::v1::error::ApiError;
use crate::metrics::{Metrics, RedirectHop};

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
            http_version: http_version(response.version()),
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

/// Returns the HTTP version of `response` as a display string, e.g. `HTTP/1.1`.
#[must_use]
fn http_version(version: wreq::Version) -> String {
    match version {
        wreq::Version::HTTP_09 => "HTTP/0.9".to_string(),
        wreq::Version::HTTP_10 => "HTTP/1.0".to_string(),
        wreq::Version::HTTP_11 => "HTTP/1.1".to_string(),
        wreq::Version::HTTP_2 => "HTTP/2.0".to_string(),
        wreq::Version::HTTP_3 => "HTTP/3.0".to_string(),
        other => format!("{other:?}"),
    }
}

/// Returns the media type — the first, lowercased parameter-free segment — of a content type.
#[must_use]
pub(crate) fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// The outcome of a fetch with redirects followed manually.
pub(crate) struct Fetched {
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

impl Fetched {
    /// Assembles the request metrics from this fetch outcome and the body read.
    ///
    /// The intermediate hops are included only when `include_hops` is requested through
    /// `include=redirects`.
    #[must_use]
    pub(crate) fn into_metrics(
        self,
        bytes_read: u64,
        truncated: bool,
        include_hops: bool,
        started: Instant,
    ) -> Metrics {
        Metrics {
            status: self.info.status,
            final_url: self.url.to_string(),
            http_version: self.info.http_version,
            content_type: self.info.content_type,
            content_length: self.info.content_length,
            bytes_read,
            ttfb_ms: duration_ms(self.headers_elapsed),
            total_ms: duration_ms(started.elapsed()),
            redirects_followed: self.followed,
            redirects: if include_hops { self.hops } else { Vec::new() },
            truncated,
        }
    }
}

/// Sends GET requests, following at most `max_redirects` redirects by hand.
///
/// When the budget is exhausted on a redirect response, that response is returned as the final
/// response; the caller can see the truncation through [`Fetched::followed`].
pub(crate) async fn fetch(
    client: &wreq::Client,
    url: Url,
    max_redirects: u32,
) -> Result<(wreq::Response, Fetched), ApiError> {
    let started = Instant::now();
    let mut current = url;
    let mut hops: Vec<RedirectHop> = Vec::new();
    let mut followed: u32 = 0;

    loop {
        let response = client
            .get(current.as_str())
            .send()
            .await
            .map_err(|error| ApiError::upstream(error.to_string()))?;

        if let Some(target) = redirect_target(&response, &current)
            && followed < max_redirects
        {
            followed += 1;
            hops.push(RedirectHop {
                status: response.status().as_u16(),
                url: current.to_string(),
            });
            current = target;

            continue;
        }

        let info = ResponseInfo::capture(&response);

        return Ok((
            response,
            Fetched {
                url: current,
                followed,
                hops,
                info,
                headers_elapsed: started.elapsed(),
            },
        ));
    }
}

/// Returns the redirect target of a redirect response, if it is one.
fn redirect_target(response: &wreq::Response, base: &Url) -> Option<Url> {
    if !response.status().is_redirection() {
        return None;
    }

    let location = response.headers().get(LOCATION)?.to_str().ok()?;

    base.join(location.trim()).ok()
}

/// Converts a duration to whole milliseconds.
#[must_use]
pub(crate) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_media_types() {
        assert_eq!(media_type("text/html; charset=utf-8"), "text/html");
        assert_eq!(media_type(" Application/JSON "), "application/json");
        assert_eq!(media_type(""), "");
    }

    #[test]
    fn converts_durations() {
        assert_eq!(duration_ms(Duration::from_millis(1_234)), 1_234);
        assert_eq!(duration_ms(Duration::from_nanos(999)), 0);
    }
}
