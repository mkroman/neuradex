//! Request parsing shared by the API handlers.
//!
//! The wire types are the single source of truth for the request contracts: they deserialize
//! the request (queries through `ApiQuery`, a thin extractor over
//! [`axum_extra::extract::Query`], which uses `serde_html_form`, so the `include` parameter may
//! be repeated — `include=redirects&include=headers` — or comma-separated; JSON bodies through
//! `ApiJson`, a thin extractor over [`axum::extract::Json`]), document the OpenAPI query
//! parameters or request body, and expose the validation each endpoint applies to its own
//! request.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum_extra::extract::Query;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::api::v1::error::ApiError;

/// The maximum number of redirects the fetch endpoints will follow.
pub const MAX_REDIRECTS: u32 = 5;

/// The maximum value accepted by the search endpoint's `timeout` parameter, in seconds.
pub const MAX_SEARCH_TIMEOUT_SECS: u64 = 30;

/// A response section that an endpoint may include.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Include {
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
pub const PEEK_INCLUDES: &[Include] = &[Include::Redirects];

/// The `include` values accepted by the fetch endpoint.
pub const FETCH_INCLUDES: &[Include] = &[Include::Redirects, Include::Headers];

/// The query parameters shared by the page-fetching endpoints (`/v1/fetch` and `/v1/peek`), as
/// they arrive on the wire.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct PageParams {
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
pub struct SearchRequest {
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

/// The number of results returned when the request carries no `count`.
pub const DEFAULT_COUNT: usize = 5;

/// The request body of the Open WebUI external search endpoint, as it arrives on the wire.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(examples(json!({"query": "rust programming", "count": 5})))]
#[serde(deny_unknown_fields)]
pub struct OpenWebUiSearchRequest {
    /// The user's search query string.
    #[schema(example = "rust programming")]
    pub query: String,
    /// The suggested maximum number of results; defaults to `DEFAULT_COUNT` when omitted.
    #[schema(default = 5, example = 5)]
    pub count: Option<usize>,
}

impl OpenWebUiSearchRequest {
    /// Resolves the requested result count, defaulting to [`DEFAULT_COUNT`].
    pub(crate) fn count(&self) -> usize {
        self.count.unwrap_or(DEFAULT_COUNT)
    }

    /// Validates the request body.
    ///
    /// # Errors
    ///
    /// Returns an error when the query is empty.
    pub(crate) fn validate(&self) -> Result<(), ApiError> {
        validate_query(&self.query)
    }
}

/// Extracts the query parameters of an endpoint into its wire type.
///
/// A thin wrapper over [`axum_extra::extract::Query`] whose rejection is [`ApiError`], so a
/// malformed query — an unknown field, a missing or non-parseable parameter — renders the JSON
/// error envelope instead of axum's plain-text 400 body. That rejection shape is all it
/// guarantees: endpoint-specific validation is applied by the handlers on the extracted value,
/// not here.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApiQuery<T>(pub(crate) T);

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
/// The body-side counterpart of [`ApiQuery`]: a thin wrapper over [`axum::extract::Json`] whose
/// rejection is [`ApiError`], so a malformed body — a missing or non-parseable field, an
/// unknown field, or a missing `Content-Type: application/json` header — renders the JSON
/// error envelope instead of axum's plain-text rejection body. That rejection shape is all it
/// guarantees: endpoint-specific validation is applied by the handlers on the extracted value,
/// not here.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApiJson<T>(pub(crate) T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) =
            Json::<T>::from_request(req, state)
                .await
                .map_err(|error| match error {
                    JsonRejection::MissingJsonContentType(_) => ApiError::unsupported_media_type(
                        "expected the request body to be JSON (Content-Type: application/json)",
                    ),
                    rejection => {
                        ApiError::invalid_param(format!("invalid request body: {rejection}"))
                    }
                })?;

        Ok(Self(value))
    }
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

/// A validated page request, ready for the redirect-following layer.
#[derive(Debug)]
pub struct PageRequest {
    /// The absolute URL of the page to fetch.
    pub url: Url,
    /// The maximum number of redirects to follow.
    pub redirects: u32,
    /// The optional response sections selected by the `include` parameter.
    pub includes: IncludeSet,
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

    /// Resolves the number of redirects to follow, defaulting to [`MAX_REDIRECTS`].
    ///
    /// # Errors
    ///
    /// Returns an error when the value exceeds [`MAX_REDIRECTS`].
    pub(crate) fn redirects(&self) -> Result<u32, ApiError> {
        let redirects = self.redirects.unwrap_or(MAX_REDIRECTS);

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

    /// Validates the page parameters against the endpoint's `allowed` includes.
    ///
    /// The parameters are validated in the order the handlers report them — URL, redirects,
    /// includes — so the first invalid parameter is the one reported.
    ///
    /// # Errors
    ///
    /// Returns the first error from [`PageParams::url`], [`PageParams::redirects`], or
    /// [`PageParams::includes`].
    pub(crate) fn validate(&self, allowed: &[Include]) -> Result<PageRequest, ApiError> {
        let url = self.url()?;
        let redirects = self.redirects()?;
        let includes = self.includes(allowed)?;

        Ok(PageRequest {
            url,
            redirects,
            includes,
        })
    }
}

/// Validates a search query.
///
/// # Errors
///
/// Returns an error when the query is empty.
fn validate_query(query: &str) -> Result<(), ApiError> {
    if query.is_empty() {
        return Err(ApiError::invalid_param("the query is empty"));
    }

    Ok(())
}

impl SearchRequest {
    /// Validates the request body.
    ///
    /// # Errors
    ///
    /// Returns an error when the query is empty or the timeout is out of range.
    pub(crate) fn validate(&self) -> Result<(), ApiError> {
        validate_query(&self.query)?;

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

    use axum::http::Uri;
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
        let request = query.validate(FETCH_INCLUDES).expect("valid");

        assert_eq!(request.url.as_str(), "https://example.com/");
        assert_eq!(request.redirects, 2);
        assert!(request.includes.redirects);
        assert!(!request.includes.headers);
    }

    #[test]
    fn validates_page_params_in_url_redirects_includes_order() {
        // An invalid URL takes precedence over the out-of-range redirects and the unknown
        // include, so the router reports the same parameter it always did.
        let query = page_params("url=not%20a%20url&redirects=6&include=nope").expect("valid");
        assert_matches!(
            query.validate(FETCH_INCLUDES),
            Err(ApiError::InvalidParam(message)) if message.starts_with("invalid url:")
        );

        // Out-of-range redirects take precedence over the unknown include.
        let query = page_params("url=https://example.com&redirects=6&include=nope").expect("valid");
        assert_matches!(
            query.validate(FETCH_INCLUDES),
            Err(ApiError::InvalidParam(message)) if message.starts_with("redirects must be")
        );
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

        assert_eq!(query.redirects().expect("valid"), MAX_REDIRECTS);
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

    #[test]
    fn extracts_openwebui_search_requests_through_the_real_json_extractor() {
        let Json(request) =
            Json::<OpenWebUiSearchRequest>::from_bytes(br#"{"query":"rust programming"}"#)
                .expect("valid body");

        assert!(request.validate().is_ok());
        assert_eq!(request.query, "rust programming");
        assert_eq!(request.count(), DEFAULT_COUNT);
    }
}
