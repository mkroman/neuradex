//! Query parameter extractors shared by the API handlers.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::Query;
use serde::Deserialize;
use std::time::Duration;
use url::Url;

use crate::api::v1::error::ApiError;
use crate::api::v1::{MAX_REDIRECTS, MAX_SEARCH_TIMEOUT_SECS};

/// The default number of redirects to follow when none is requested.
pub const DEFAULT_REDIRECTS: u32 = 5;

/// The `include` values accepted by the peek endpoint.
pub const PEEK_INCLUDES: &[&str] = &["redirects"];

/// The `include` values accepted by the fetch endpoint.
pub const FETCH_INCLUDES: &[&str] = &["redirects", "headers"];

/// The raw query parameters of the fetch endpoints, as they arrive on the wire.
///
/// Extracted with [`axum_extra::extract::Query`], which uses `serde_html_form`: the `include`
/// parameter may be repeated (`include=redirects&include=headers`) or comma-separated.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchQuery {
    /// The URL to fetch.
    url: String,
    /// The maximum number of redirects to follow.
    redirects: Option<u32>,
    /// The optional data to include in the response; may be repeated and comma-separated.
    #[serde(default)]
    include: Vec<String>,
}

/// The raw query parameters of the search endpoint, as they arrive on the wire.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    /// The search query.
    q: String,
    /// The maximum number of results to return.
    limit: Option<usize>,
    /// The maximum time to wait for the search, in whole seconds.
    timeout: Option<u64>,
}

/// The parameters accepted by the fetch endpoint (`/v1/fetch`).
#[derive(Clone, Debug)]
pub struct FetchParams {
    /// The URL to fetch.
    pub url: Url,
    /// The maximum number of redirects to follow.
    pub redirects: u32,
    /// The optional data to include in the response.
    pub includes: IncludeSet,
}

/// The parameters accepted by the peek endpoint (`/v1/peek`).
#[derive(Clone, Debug)]
pub struct PeekParams {
    /// The URL to fetch.
    pub url: Url,
    /// The maximum number of redirects to follow.
    pub redirects: u32,
    /// The optional data to include in the response.
    pub includes: IncludeSet,
}

/// The parameters accepted by the search endpoint (`/v1/search`).
#[derive(Clone, Debug)]
pub struct SearchParams {
    /// The search query.
    pub query: String,
    /// The maximum number of results to return, if limited.
    pub limit: Option<usize>,
    /// The maximum time to wait for the search, including queueing, if limited.
    pub timeout: Option<Duration>,
}

impl SearchParams {
    /// Builds the search parameters from the raw query.
    fn from_query(query: SearchQuery) -> Result<Self, ApiError> {
        if query.q.is_empty() {
            return Err(ApiError::invalid_param("the q parameter is empty"));
        }

        if query.timeout == Some(0) {
            return Err(ApiError::invalid_param(
                "the timeout parameter must be at least 1 second",
            ));
        }

        if query
            .timeout
            .is_some_and(|timeout| timeout > MAX_SEARCH_TIMEOUT_SECS)
        {
            return Err(ApiError::invalid_param(format!(
                "the timeout parameter must be between 1 and {MAX_SEARCH_TIMEOUT_SECS} seconds"
            )));
        }

        Ok(Self {
            query: query.q,
            limit: query.limit,
            timeout: query.timeout.map(Duration::from_secs),
        })
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
    /// Returns an error when an unknown include value is requested.
    pub fn parse(values: &[String], allowed: &[&str]) -> Result<Self, ApiError> {
        let mut includes = Self::default();

        for value in values.iter().flat_map(|value| value.split(',')) {
            let value = value.trim().to_ascii_lowercase();

            if value.is_empty() {
                continue;
            }

            if !allowed.contains(&value.as_str()) {
                return Err(ApiError::invalid_param(format!("unknown include: {value}")));
            }

            match value.as_str() {
                "redirects" => includes.redirects = true,
                "headers" => includes.headers = true,
                _ => unreachable!("validated against `allowed`"),
            }
        }

        Ok(includes)
    }
}

impl FetchQuery {
    /// Validates the query against the `include` values `allowed` by the calling endpoint.
    fn into_params(self, allowed_includes: &[&str]) -> Result<FetchParams, ApiError> {
        let url = Url::parse(&self.url)
            .map_err(|error| ApiError::invalid_param(format!("invalid url: {error}")))?;
        let redirects = self.redirects.unwrap_or(DEFAULT_REDIRECTS);

        if redirects > MAX_REDIRECTS {
            return Err(ApiError::invalid_param(format!(
                "redirects must be between 0 and {MAX_REDIRECTS}"
            )));
        }

        let includes = IncludeSet::parse(&self.include, allowed_includes)?;

        Ok(FetchParams {
            url,
            redirects,
            includes,
        })
    }
}

impl<S: Send + Sync> FromRequestParts<S> for FetchParams {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(query) = Query::<FetchQuery>::from_request_parts(parts, state)
            .await
            .map_err(|error| {
                ApiError::invalid_param(format!("invalid query parameters: {error}"))
            })?;

        query.into_params(FETCH_INCLUDES)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for PeekParams {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(query) = Query::<FetchQuery>::from_request_parts(parts, state)
            .await
            .map_err(|error| {
                ApiError::invalid_param(format!("invalid query parameters: {error}"))
            })?;

        let params = query.into_params(PEEK_INCLUDES)?;

        Ok(Self {
            url: params.url,
            redirects: params.redirects,
            includes: params.includes,
        })
    }
}

impl<S: Send + Sync> FromRequestParts<S> for SearchParams {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(query) = Query::<SearchQuery>::from_request_parts(parts, state)
            .await
            .map_err(|error| {
                ApiError::invalid_param(format!("invalid query parameters: {error}"))
            })?;

        Self::from_query(query)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Uri;

    use super::*;

    /// Parses `pairs` and validates them, mirroring the `Query` extraction path.
    fn fetch_query(pairs: &str) -> Result<FetchParams, ApiError> {
        let query: FetchQuery = serde_html_form::from_str(pairs).map_err(|error| {
            ApiError::invalid_param(format!("invalid query parameters: {error}"))
        })?;

        query.into_params(FETCH_INCLUDES)
    }

    #[test]
    fn extracts_repeated_includes_through_the_real_extractor() {
        let uri: Uri =
            "https://maero.dk/v1/fetch?url=https://maero.dk&include=redirects&include=headers"
                .parse()
                .expect("valid uri");

        let Query(query) = Query::<FetchQuery>::try_from_uri(&uri).expect("valid query");
        let params = query.into_params(FETCH_INCLUDES).expect("valid params");

        assert!(params.includes.redirects);
        assert!(params.includes.headers);
    }

    #[test]
    fn rejects_unknown_parameters_through_the_real_extractor() {
        let uri: Uri = "https://maero.dk/v1/fetch?url=https://maero.dk&nope=1"
            .parse()
            .expect("valid uri");

        assert!(Query::<FetchQuery>::try_from_uri(&uri).is_err());
    }

    #[test]
    fn parses_fetch_params() {
        let params =
            fetch_query("url=https://maero.dk&redirects=2&include=redirects").expect("valid");

        assert_eq!(params.url.as_str(), "https://maero.dk/");
        assert_eq!(params.redirects, 2);
        assert!(params.includes.redirects);
        assert!(!params.includes.headers);
    }

    #[test]
    fn parses_repeated_includes() {
        let params =
            fetch_query("url=https://maero.dk&include=redirects&include=headers").expect("valid");

        assert!(params.includes.redirects);
        assert!(params.includes.headers);
    }

    #[test]
    fn parses_comma_separated_includes() {
        let params =
            fetch_query("url=https://maero.dk&include=redirects,%20headers").expect("valid");

        assert!(params.includes.redirects);
        assert!(params.includes.headers);
    }

    #[test]
    fn defaults_redirects() {
        let params = fetch_query("url=https://maero.dk").expect("valid");

        assert_eq!(params.redirects, DEFAULT_REDIRECTS);
        assert_eq!(params.includes, IncludeSet::default());
    }

    #[test]
    fn rejects_out_of_range_redirects() {
        assert!(matches!(
            fetch_query("url=https://maero.dk&redirects=6"),
            Err(ApiError::InvalidParam(_))
        ));
        assert!(fetch_query("url=https://maero.dk&redirects=-1").is_err());
    }

    #[test]
    fn rejects_non_integer_redirects() {
        assert!(fetch_query("url=https://maero.dk&redirects=abc").is_err());
    }

    #[test]
    fn requires_a_url() {
        assert!(matches!(fetch_query(""), Err(ApiError::InvalidParam(_))));
        assert!(matches!(
            fetch_query("url=not a url"),
            Err(ApiError::InvalidParam(_))
        ));
    }

    #[test]
    fn rejects_unknown_parameters_and_includes() {
        assert!(fetch_query("url=https://maero.dk&nope=1").is_err());
        assert!(matches!(
            fetch_query("url=https://maero.dk&include=nope"),
            Err(ApiError::InvalidParam(_))
        ));

        // `include=headers` is valid for the fetch endpoint, but not for peek.
        let peek_query: FetchQuery =
            serde_html_form::from_str("url=https://maero.dk&include=headers").expect("valid");
        assert!(peek_query.into_params(PEEK_INCLUDES).is_err());

        let fetch_query: FetchQuery =
            serde_html_form::from_str("url=https://maero.dk&include=headers").expect("valid");
        assert!(fetch_query.into_params(FETCH_INCLUDES).is_ok());
    }

    #[test]
    fn extracts_search_params_through_the_real_extractor() {
        let uri: Uri = "https://maero.dk/v1/search?q=rust+programming&limit=10"
            .parse()
            .expect("valid uri");

        let Query(query) = Query::<SearchQuery>::try_from_uri(&uri).expect("valid query");
        let params = SearchParams::from_query(query).expect("valid params");

        assert_eq!(params.query, "rust programming");
        assert_eq!(params.limit, Some(10));
        assert_eq!(params.timeout, None);
    }

    #[test]
    fn extracts_the_search_timeout_through_the_real_extractor() {
        let uri: Uri = "https://maero.dk/v1/search?q=rust&timeout=10"
            .parse()
            .expect("valid uri");

        let Query(query) = Query::<SearchQuery>::try_from_uri(&uri).expect("valid query");
        let params = SearchParams::from_query(query).expect("valid params");

        assert_eq!(params.timeout, Some(Duration::from_secs(10)));
    }

    #[test]
    fn rejects_out_of_range_search_timeouts() {
        for (raw, expected) in [
            ("q=rust&timeout=0", "the timeout parameter must be at least"),
            ("q=rust&timeout=31", "the timeout parameter must be between"),
        ] {
            let uri: Uri = format!("https://maero.dk/v1/search?{raw}")
                .parse()
                .expect("valid uri");

            let Query(query) = Query::<SearchQuery>::try_from_uri(&uri).expect("valid query");

            assert!(
                matches!(SearchParams::from_query(query), Err(ApiError::InvalidParam(message)) if message.contains(expected)),
                "expected {raw} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_non_integer_search_timeouts() {
        let uri: Uri = "https://maero.dk/v1/search?q=rust&timeout=1.5"
            .parse()
            .expect("valid uri");

        assert!(Query::<SearchQuery>::try_from_uri(&uri).is_err());
    }

    #[test]
    fn rejects_an_empty_search_query() {
        let uri: Uri = "https://maero.dk/v1/search?q=".parse().expect("valid uri");

        let Query(query) = Query::<SearchQuery>::try_from_uri(&uri).expect("valid query");

        assert!(matches!(
            SearchParams::from_query(query),
            Err(ApiError::InvalidParam(_))
        ));
    }

    #[test]
    fn rejects_unknown_search_parameters() {
        let uri: Uri = "https://maero.dk/v1/search?q=rust&nope=1"
            .parse()
            .expect("valid uri");

        assert!(Query::<SearchQuery>::try_from_uri(&uri).is_err());
    }
}
