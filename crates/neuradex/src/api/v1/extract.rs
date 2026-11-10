//! Request parsing shared by the API handlers.
//!
//! The wire types are the single source of truth for the request contracts: they deserialize
//! the request (queries through `ApiQuery`, a thin extractor over
//! [`axum_extra::extract::Query`], which uses `serde_html_form`, so the `include` parameter may
//! be repeated — `include=redirects&include=headers` — or comma-separated; JSON bodies through
//! `ApiJson`, which captures the raw bytes and parses them with the same JSON content type and
//! rejection contract as [`axum::extract::Json`]), document the OpenAPI query parameters or
//! request body, and expose the validation each endpoint applies to its own request.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use axum::http::request::Parts;
use axum_extra::extract::Query;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::api::v1::body_capture::{
    BodyCaptureLimit, CapturedBody, JSON_MAX_BODY_BYTES, capture_body,
};
use crate::api::v1::error::ApiError;
use crate::api::v1::{MAX_REDIRECTS, MAX_SEARCH_TIMEOUT_SECS};
use crate::audit::RequestAuditHandle;

/// The default number of redirects to follow when none is requested.
pub const DEFAULT_REDIRECTS: u32 = MAX_REDIRECTS;

/// A response section that an endpoint may include.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Include {
    /// The chain of intermediate redirect hops in the metrics.
    Redirects,
    /// The request and response headers of the fetched page.
    Headers,
}

impl Include {
    /// Every include the API understands.
    const ALL: &[Self] = &[Self::Redirects, Self::Headers];

    /// Returns the wire name of the include.
    const fn name(self) -> &'static str {
        match self {
            Self::Redirects => "redirects",
            Self::Headers => "headers",
        }
    }

    /// Resolves a wire name to its include, ignoring ASCII case.
    fn from_name(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|include| value.eq_ignore_ascii_case(include.name()))
    }

    /// Marks the include in `set`.
    const fn apply(self, set: &mut IncludeSet) {
        match self {
            Self::Redirects => set.redirects = true,
            Self::Headers => set.headers = true,
        }
    }
}

/// The `include` values accepted by the peek endpoint.
pub(crate) const PEEK_INCLUDES: &[Include] = &[Include::Redirects];

/// The `include` values accepted by the fetch endpoint.
pub(crate) const FETCH_INCLUDES: &[Include] = &[Include::Redirects, Include::Headers];

/// The query parameters shared by the page-fetching endpoints (`/v1/fetch` and `/v1/peek`), as
/// they arrive on the wire.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct PageParams {
    /// The absolute URL of the page to fetch.
    #[param(example = "https://example.com", format = "uri")]
    pub(crate) url: String,
    /// The maximum number of redirects to follow; defaults to 5.
    #[param(minimum = 0, maximum = 5, default = 5, example = 2)]
    pub(crate) redirects: Option<u32>,
    /// The optional data to include in the response; may be repeated and comma-separated.
    #[serde(default)]
    #[param(style = Form, explode = true, example = json!(["redirects"]))]
    pub(crate) include: Vec<String>,
}

/// The JSON request body of the search endpoint (`/v1/search`), as it arrives on the wire.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(examples(json!({
    "query": "rust programming",
    "limit": 10,
    "timeout": 10
})))]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchRequest {
    /// The search query.
    #[schema(example = "rust programming")]
    pub(crate) query: String,
    /// The maximum number of results to return.
    #[schema(example = 10)]
    pub(crate) limit: Option<usize>,
    /// The maximum time to wait for the search — the queue wait, the session wait, and the
    /// search itself — in whole seconds.
    #[schema(minimum = 1, maximum = 30, example = 10)]
    pub(crate) timeout: Option<u64>,
}

/// Extracts the query parameters of an endpoint into its wire type.
///
/// A thin wrapper over [`axum_extra::extract::Query`] whose rejection is [`ApiError`], so a
/// malformed query — an unknown field, a missing or non-parseable parameter — renders the JSON
/// error envelope instead of axum's plain-text 400 body. That rejection shape is all it
/// guarantees: endpoint-specific validation is applied by the handlers on the extracted value,
/// not here.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ApiQuery<T>(pub(crate) T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(query) = Query::<T>::from_request_parts(parts, state)
            .await
            .map_err(|error| {
                ApiError::invalid_param(format!("invalid query parameters: {error}"))
            })?;

        Ok(Self(query))
    }
}

/// Extracts the JSON body of an endpoint into its wire type.
///
/// The body-side counterpart of [`ApiQuery`]: a thin wrapper over the JSON body contract whose
/// rejection is [`ApiError`], so a malformed body — a missing or non-parseable field, an
/// unknown field, an oversized body, or a missing `Content-Type: application/json` header —
/// renders the JSON error envelope instead of axum's plain-text rejection body. That rejection
/// shape is all it guarantees: endpoint-specific validation is applied by the handlers on the
/// extracted value, not here.
///
/// The raw body is captured alongside the parsed value, up to the configured content limit
/// clamped to [`JSON_MAX_BODY_BYTES`], so the audit and content paths can refer to the exact
/// bytes the client sent without a second extraction.
#[derive(Debug, Clone, Default)]
pub(crate) struct ApiJson<T> {
    /// The deserialized request value.
    pub(crate) value: T,
    /// The raw request body bytes.
    ///
    /// Content storage reads this in a follow-up; until then only the extractor itself and the
    /// raw-body tests read it.
    #[allow(dead_code)]
    pub(crate) raw: CapturedBody,
}

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();

        if !json_content_type(&parts.headers) {
            return Err(ApiError::unsupported_media_type(
                "expected the request body to be JSON (Content-Type: application/json)",
            ));
        }

        let limit = parts
            .extensions
            .get::<BodyCaptureLimit>()
            .map_or(JSON_MAX_BODY_BYTES, |limit| {
                limit.0.min(JSON_MAX_BODY_BYTES)
            });
        let raw = capture_body(body, limit)
            .await
            .map_err(|error| ApiError::invalid_param(format!("invalid request body: {error}")))?;

        if let Some(handle) = parts.extensions.get::<RequestAuditHandle>() {
            handle.update(|record| {
                record.set_request_body_bytes(u64::try_from(raw.len()).unwrap_or(u64::MAX));
            });
        }

        if raw.truncated() {
            return Err(ApiError::invalid_param(format!(
                "invalid request body: the body exceeds the {limit} byte limit"
            )));
        }

        let value = serde_json::from_slice(&raw.bytes)
            .map_err(|error| ApiError::invalid_param(format!("invalid request body: {error}")))?;

        Ok(Self { value, raw })
    }
}

/// Whether `headers` declare a JSON request body, matching axum's `Json` extractor.
fn json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(CONTENT_TYPE) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let media_type = value.split(';').next().unwrap_or_default().trim();
    let Some((kind, subtype)) = media_type.split_once('/') else {
        return false;
    };

    kind.eq_ignore_ascii_case("application")
        && (subtype.eq_ignore_ascii_case("json") || subtype.to_ascii_lowercase().ends_with("+json"))
}

/// The optional response sections selected by the `include` parameter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IncludeSet {
    /// Include the chain of intermediate redirect hops in the metrics.
    pub redirects: bool,
    /// Include the request and response headers of the fetched page.
    pub headers: bool,
}

impl IncludeSet {
    /// Parses `include` parameter values into a set, validating each entry against `allowed`.
    ///
    /// Values are comma-separated, and the parameter may be repeated; duplicates collapse.
    ///
    /// # Errors
    ///
    /// Returns an error when an include value is unknown or is not allowed by the endpoint.
    pub(crate) fn parse(values: &[String], allowed: &[Include]) -> Result<Self, ApiError> {
        let mut includes = Self::default();

        for value in values.iter().flat_map(|value| value.split(',')) {
            let value = value.trim();

            if value.is_empty() {
                continue;
            }

            let include = Include::from_name(value).ok_or_else(|| {
                ApiError::invalid_param(format!("unknown include: {}", value.to_ascii_lowercase()))
            })?;

            if !allowed.contains(&include) {
                return Err(ApiError::invalid_param(format!(
                    "include {} is not supported by this endpoint",
                    include.name()
                )));
            }

            include.apply(&mut includes);
        }

        Ok(includes)
    }
}

impl PageParams {
    /// Parses and validates the `url` parameter.
    ///
    /// Only absolute `http` and `https` URLs with a host are accepted.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not a valid absolute HTTP(S) URL with a host.
    pub(crate) fn url(&self) -> Result<Url, ApiError> {
        let url = Url::parse(&self.url)
            .map_err(|error| ApiError::invalid_param(format!("invalid url: {error}")))?;

        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(ApiError::invalid_param(
                "the url must be an absolute http or https URL with a host",
            ));
        }

        Ok(url)
    }

    /// Resolves the number of redirects to follow, defaulting to [`DEFAULT_REDIRECTS`].
    ///
    /// # Errors
    ///
    /// Returns an error when the value exceeds [`MAX_REDIRECTS`].
    pub(crate) fn redirects(&self) -> Result<u32, ApiError> {
        let redirects = self.redirects.unwrap_or(DEFAULT_REDIRECTS);

        if redirects > MAX_REDIRECTS {
            return Err(ApiError::invalid_param(format!(
                "redirects must be between 0 and {MAX_REDIRECTS}"
            )));
        }

        Ok(redirects)
    }

    /// Parses the `include` parameter values, validating each entry against `allowed`.
    ///
    /// # Errors
    ///
    /// Returns an error when an include value is unknown or is not allowed by the endpoint.
    pub(crate) fn includes(&self, allowed: &[Include]) -> Result<IncludeSet, ApiError> {
        IncludeSet::parse(&self.include, allowed)
    }
}

impl SearchRequest {
    /// Validates the request body.
    ///
    /// # Errors
    ///
    /// Returns an error when the query is empty or the timeout is out of range.
    pub(crate) fn validate(&self) -> Result<(), ApiError> {
        if self.query.is_empty() {
            return Err(ApiError::invalid_param("the query is empty"));
        }

        if self.timeout == Some(0) {
            return Err(ApiError::invalid_param(
                "the timeout must be at least 1 second",
            ));
        }

        if self
            .timeout
            .is_some_and(|timeout| timeout > MAX_SEARCH_TIMEOUT_SECS)
        {
            return Err(ApiError::invalid_param(format!(
                "the timeout must be between 1 and {MAX_SEARCH_TIMEOUT_SECS} seconds"
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use axum::Json;
    use axum::body::Body;
    use axum::http::{Request, Uri};
    use axum_extra::extract::{Query, QueryRejection};

    use super::*;

    /// Parses `pairs` into `PageParams` through the real query extractor.
    fn page_params(pairs: &str) -> Result<PageParams, QueryRejection> {
        let uri: Uri = format!("https://example.com/v1/fetch?{pairs}")
            .parse()
            .expect("the test query is a valid URI");

        Query::<PageParams>::try_from_uri(&uri).map(|Query(params)| params)
    }

    #[test]
    fn extracts_repeated_includes_through_the_real_extractor() {
        let uri: Uri =
            "https://example.com/v1/fetch?url=https://example.com&include=redirects&include=headers"
                .parse()
                .expect("valid uri");

        let Query(query) = Query::<PageParams>::try_from_uri(&uri).expect("valid query");
        let includes = query.includes(FETCH_INCLUDES).expect("valid includes");

        assert!(includes.redirects);
        assert!(includes.headers);
    }

    #[test]
    fn rejects_unknown_parameters_through_the_real_extractor() {
        let uri: Uri = "https://example.com/v1/fetch?url=https://example.com&nope=1"
            .parse()
            .expect("valid uri");

        assert!(Query::<PageParams>::try_from_uri(&uri).is_err());
    }

    #[test]
    fn parses_page_params() {
        let query =
            page_params("url=https://example.com&redirects=2&include=redirects").expect("valid");

        assert_eq!(query.url().expect("valid").as_str(), "https://example.com/");
        assert_eq!(query.redirects().expect("valid"), 2);
        assert!(query.includes(FETCH_INCLUDES).expect("valid").redirects);
        assert!(!query.includes(FETCH_INCLUDES).expect("valid").headers);
    }

    #[test]
    fn parses_comma_separated_includes() {
        let query =
            page_params("url=https://example.com&include=redirects,%20headers").expect("valid");
        let includes = query.includes(FETCH_INCLUDES).expect("valid");

        assert!(includes.redirects);
        assert!(includes.headers);
    }

    #[test]
    fn defaults_redirects() {
        let query = page_params("url=https://example.com").expect("valid");

        assert_eq!(query.redirects().expect("valid"), DEFAULT_REDIRECTS);
        assert_eq!(
            query.includes(FETCH_INCLUDES).expect("valid"),
            IncludeSet::default()
        );
    }

    #[test]
    fn rejects_out_of_range_redirects() {
        let query = page_params("url=https://example.com&redirects=6").expect("valid");

        assert_matches!(query.redirects(), Err(ApiError::InvalidParam(_)));

        assert!(page_params("url=https://example.com&redirects=-1").is_err());
    }

    #[test]
    fn rejects_non_integer_redirects() {
        assert!(page_params("url=https://example.com&redirects=abc").is_err());
    }

    #[test]
    fn requires_a_url() {
        assert!(page_params("").is_err());

        let query = page_params("url=not%20a%20url").expect("valid");
        assert_matches!(query.url(), Err(ApiError::InvalidParam(_)));
    }

    #[test]
    fn accepts_absolute_http_urls() {
        for url in ["https://example.com/page", "http://example.com/page"] {
            let query = page_params(&format!("url={url}")).expect("valid query");

            assert_eq!(query.url().expect("valid url").as_str(), url);
        }
    }

    #[test]
    fn rejects_non_http_urls() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/file",
            "mailto:user@example.com",
        ] {
            let query = page_params(&format!("url={url}")).expect("valid query");

            assert_matches!(
                query.url(),
                Err(ApiError::InvalidParam(_)),
                "expected {url} to be rejected"
            );
        }
    }

    #[test]
    fn validates_includes_against_each_endpoints_allow_list() {
        let query = page_params("url=https://example.com&include=nope").expect("valid");
        assert_matches!(
            query.includes(FETCH_INCLUDES),
            Err(ApiError::InvalidParam(_))
        );

        // `include=headers` is valid for the fetch endpoint, but not for peek.
        let peek_query = page_params("url=https://example.com&include=headers").expect("valid");
        assert!(peek_query.includes(PEEK_INCLUDES).is_err());
        assert!(peek_query.includes(FETCH_INCLUDES).is_ok());
    }

    #[test]
    fn distinguishes_unsupported_includes_from_unknown_ones() {
        let query = page_params("url=https://example.com&include=headers").expect("valid");
        assert_matches!(
            query.includes(PEEK_INCLUDES),
            Err(ApiError::InvalidParam(message))
                if message == "include headers is not supported by this endpoint"
        );

        let query = page_params("url=https://example.com&include=nope").expect("valid");
        assert_matches!(
            query.includes(FETCH_INCLUDES),
            Err(ApiError::InvalidParam(message)) if message == "unknown include: nope"
        );
    }

    #[test]
    fn extracts_search_request_through_the_real_json_extractor() {
        let Json(request) =
            Json::<SearchRequest>::from_bytes(br#"{"query":"rust programming","limit":10}"#)
                .expect("valid body");

        assert!(request.validate().is_ok());
        assert_eq!(request.query, "rust programming");
        assert_eq!(request.limit, Some(10));
        assert_eq!(request.timeout, None);
    }

    #[test]
    fn extracts_the_search_timeout_through_the_real_json_extractor() {
        let Json(request) = Json::<SearchRequest>::from_bytes(br#"{"query":"rust","timeout":10}"#)
            .expect("valid body");

        assert!(request.validate().is_ok());
        assert_eq!(request.timeout, Some(10));
    }

    #[test]
    fn rejects_out_of_range_search_timeouts() {
        for (body, expected) in [
            (
                r#"{"query":"rust","timeout":0}"#,
                "the timeout must be at least",
            ),
            (
                r#"{"query":"rust","timeout":31}"#,
                "the timeout must be between",
            ),
        ] {
            let Json(request) =
                Json::<SearchRequest>::from_bytes(body.as_bytes()).expect("valid body");

            assert_matches!(
                request.validate(),
                Err(ApiError::InvalidParam(message)) if message.contains(expected),
                "expected {body} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_an_empty_search_request() {
        let Json(request) =
            Json::<SearchRequest>::from_bytes(br#"{"query":""}"#).expect("valid body");

        assert_matches!(request.validate(), Err(ApiError::InvalidParam(_)));
    }

    /// Builds a request for the real `ApiJson` extractor.
    fn json_request(content_type: Option<&str>, body: &'static str) -> Request<Body> {
        let mut builder = Request::builder().method("POST").uri("/v1/search");

        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }

        builder
            .body(Body::from(body))
            .expect("the test request is valid")
    }

    #[tokio::test]
    async fn captures_the_raw_json_body_through_the_real_extractor() {
        let body = r#" { "query" : "rust programming", "limit": 10 } "#;
        let request = json_request(Some("application/json; charset=utf-8"), body);

        let ApiJson { value, raw } = ApiJson::<SearchRequest>::from_request(request, &())
            .await
            .expect("valid body");

        assert_eq!(value.query, "rust programming");
        assert_eq!(value.limit, Some(10));
        assert_eq!(&raw.bytes[..], body.as_bytes());
        assert_eq!(raw.len(), body.len());
        assert!(!raw.truncated());
    }

    #[tokio::test]
    async fn accepts_json_suffixed_media_types_through_the_real_extractor() {
        let request = json_request(Some("application/cloudevents+json"), r#"{"query":"rust"}"#);

        let ApiJson { value, .. } = ApiJson::<SearchRequest>::from_request(request, &())
            .await
            .expect("valid body");

        assert_eq!(value.query, "rust");
    }

    #[tokio::test]
    async fn rejects_missing_and_non_json_content_types_through_the_real_extractor() {
        for content_type in [None, Some("text/plain"), Some("text/json")] {
            let request = json_request(content_type, r#"{"query":"rust"}"#);
            let error = ApiJson::<SearchRequest>::from_request(request, &())
                .await
                .expect_err("the content type is rejected");

            assert_matches!(error, ApiError::UnsupportedMediaType(message)
                if message == "expected the request body to be JSON (Content-Type: application/json)");
        }
    }

    #[tokio::test]
    async fn rejects_malformed_bodies_through_the_real_extractor() {
        for body in [
            "not json",
            r#"{"query":"rust","nope":1}"#,
            r#"{"limit":10}"#,
        ] {
            let request = json_request(Some("application/json"), body);
            let error = ApiJson::<SearchRequest>::from_request(request, &())
                .await
                .expect_err("the body is rejected");

            assert_matches!(error, ApiError::InvalidParam(message)
                if message.starts_with("invalid request body: "));
        }
    }

    #[tokio::test]
    async fn rejects_bodies_above_the_capture_limit() {
        let mut request = json_request(Some("application/json"), r#"{"query":"rust"}"#);
        request.extensions_mut().insert(BodyCaptureLimit(8));

        let error = ApiJson::<SearchRequest>::from_request(request, &())
            .await
            .expect_err("the body exceeds the limit");

        assert_matches!(error, ApiError::InvalidParam(message)
            if message.contains("exceeds the 8 byte limit"));
    }
}
