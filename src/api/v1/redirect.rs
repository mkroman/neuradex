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
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    /// Builds the client the redirect loop runs with: redirects are never followed by the
    /// client itself.
    fn client() -> wreq::Client {
        wreq::Client::builder()
            .redirect(wreq::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .expect("valid client")
    }

    #[tokio::test]
    async fn follows_redirects_and_records_the_hops() {
        let server = MockServer::start().await;
        let base = server.uri();

        let relative = ResponseTemplate::new(302).insert_header("Location", "/target");
        Mock::given(method("GET"))
            .and(path("/relative"))
            .respond_with(relative)
            .expect(1)
            .mount(&server)
            .await;

        let absolute =
            ResponseTemplate::new(302).insert_header("Location", format!("{base}/target"));
        Mock::given(method("GET"))
            .and(path("/absolute"))
            .respond_with(absolute)
            .expect(1)
            .mount(&server)
            .await;

        let target = ResponseTemplate::new(200).set_body_string("the target");
        Mock::given(method("GET"))
            .and(path("/target"))
            .respond_with(target)
            .expect(2)
            .mount(&server)
            .await;

        // A relative Location is resolved against the current URL, an absolute one is taken
        // as-is; both are counted as a single hop each.
        for start in ["/relative", "/absolute"] {
            let (response, fetched) = fetch(
                &client(),
                Url::parse(&format!("{base}{start}")).expect("valid"),
                5,
            )
            .await
            .expect("the fetch succeeds");

            assert_eq!(fetched.info.status, 200);
            assert_eq!(fetched.url.as_str(), format!("{base}/target"));
            assert_eq!(fetched.followed, 1);
            assert_eq!(fetched.hops.len(), 1);
            assert_eq!(fetched.hops[0].status, 302);
            assert_eq!(fetched.hops[0].url, format!("{base}{start}"));
            assert_eq!(response.status().as_u16(), 200);
        }
    }

    #[tokio::test]
    async fn returns_the_redirect_response_when_the_budget_is_exhausted() {
        let server = MockServer::start().await;
        let base = server.uri();

        Mock::given(method("GET"))
            .and(path("/redirect"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/target"))
            .expect(1)
            .mount(&server)
            .await;

        let (response, fetched) = fetch(
            &client(),
            Url::parse(&format!("{base}/redirect")).expect("valid"),
            0,
        )
        .await
        .expect("the fetch succeeds");

        // With no redirect budget, the redirect response itself is the final response: the
        // caller sees the truncation through `followed`.
        assert_eq!(fetched.info.status, 302);
        assert_eq!(fetched.followed, 0);
        assert!(fetched.hops.is_empty());
        assert_eq!(fetched.url.as_str(), format!("{base}/redirect"));
        assert_eq!(response.status().as_u16(), 302);
    }

    #[tokio::test]
    async fn caps_redirect_loops_at_the_budget() {
        let server = MockServer::start().await;
        let base = server.uri();

        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/loop"))
            .expect(6)
            .mount(&server)
            .await;

        let (response, fetched) = fetch(
            &client(),
            Url::parse(&format!("{base}/loop")).expect("valid"),
            5,
        )
        .await
        .expect("the fetch succeeds");

        // Five redirects are followed, one per budget unit, and the sixth — the budget
        // exhausted — is returned as the final response.
        assert_eq!(fetched.followed, 5);
        assert_eq!(fetched.hops.len(), 5);
        assert_eq!(fetched.info.status, 302);
        assert_eq!(response.status().as_u16(), 302);
    }
}
