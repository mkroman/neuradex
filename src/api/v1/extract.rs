//! Query parameter parsing shared by the API handlers.
//!
//! The wire types are the single source of truth for the query contracts: they deserialize the
//! request (through [`ApiQuery`], a thin extractor over [`axum_extra::extract::Query`],
//! which uses `serde_html_form`, so the `include` parameter may be repeated —
//! `include=redirects&include=headers` — or comma-separated), document the OpenAPI parameters,
//! and expose the validation each endpoint applies to its own query.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::Query;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;
use utoipa::IntoParams;

use crate::api::v1::error::ApiError;
use crate::api::v1::{MAX_REDIRECTS, MAX_SEARCH_TIMEOUT_SECS};

/// The default number of redirects to follow when none is requested.
pub const DEFAULT_REDIRECTS: u32 = 5;

/// The `include` values accepted by the peek endpoint.
pub const PEEK_INCLUDES: &[&str] = &["redirects"];

/// The `include` values accepted by the fetch endpoint.
pub const FETCH_INCLUDES: &[&str] = &["redirects", "headers"];

/// The query parameters of the page-fetching endpoints (`/v1/fetch` and `/v1/peek`), as they
/// arrive on the wire.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct FetchParams {
    /// The absolute URL of the page to fetch.
    #[param(example = "https://maero.dk", format = "uri")]
    pub(crate) url: String,
    /// The maximum number of redirects to follow; defaults to 5.
    #[param(minimum = 0, maximum = 5, default = 5, example = 2)]
    pub(crate) redirects: Option<u32>,
    /// The optional data to include in the response; may be repeated and comma-separated.
    #[serde(default)]
    #[param(style = Form, explode = true, example = json!(["redirects"]))]
    pub(crate) include: Vec<String>,
}

/// The query parameters of the search endpoint (`/v1/search`), as they arrive on the wire.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct SearchParams {
    /// The search query.
    #[param(example = "rust programming")]
    pub(crate) query: String,
    /// The maximum number of results to return.
    #[param(example = 10)]
    pub(crate) limit: Option<usize>,
    /// The maximum time to wait for the search — the queue wait, the session wait, and the
    /// search itself — in whole seconds.
    #[param(minimum = 1, maximum = 30, example = 10)]
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

impl FetchParams {
    /// Parses and validates the `url` parameter.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not a valid URL.
    pub(crate) fn url(&self) -> Result<Url, ApiError> {
        Url::parse(&self.url)
            .map_err(|error| ApiError::invalid_param(format!("invalid url: {error}")))
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
    /// Returns an error when an unknown include value is requested.
    pub(crate) fn includes(&self, allowed: &[&str]) -> Result<IncludeSet, ApiError> {
        IncludeSet::parse(&self.include, allowed)
    }
}

impl SearchParams {
    /// Validates the query parameters.
    ///
    /// # Errors
    ///
    /// Returns an error when the query is empty or the timeout is out of range.
    pub(crate) fn validate(&self) -> Result<(), ApiError> {
        if self.query.is_empty() {
            return Err(ApiError::invalid_param("the query parameter is empty"));
        }

        if self.timeout == Some(0) {
            return Err(ApiError::invalid_param(
                "the timeout parameter must be at least 1 second",
            ));
        }

        if self
            .timeout
            .is_some_and(|timeout| timeout > MAX_SEARCH_TIMEOUT_SECS)
        {
            return Err(ApiError::invalid_param(format!(
                "the timeout parameter must be between 1 and {MAX_SEARCH_TIMEOUT_SECS} seconds"
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Uri;
    use axum_extra::extract::Query;

    use super::*;

    /// Parses `pairs` into `FetchParams`, mirroring the `Query` extraction path.
    fn fetch_params(pairs: &str) -> Result<FetchParams, serde_html_form::de::Error> {
        serde_html_form::from_str(pairs)
    }

    /// Parses `pairs` into `SearchParams`, mirroring the `Query` extraction path.
    fn search_params(pairs: &str) -> Result<SearchParams, serde_html_form::de::Error> {
        serde_html_form::from_str(pairs)
    }

    #[test]
    fn extracts_repeated_includes_through_the_real_extractor() {
        let uri: Uri =
            "https://maero.dk/v1/fetch?url=https://maero.dk&include=redirects&include=headers"
                .parse()
                .expect("valid uri");

        let Query(query) = Query::<FetchParams>::try_from_uri(&uri).expect("valid query");
        let includes = query.includes(FETCH_INCLUDES).expect("valid includes");

        assert!(includes.redirects);
        assert!(includes.headers);
    }

    #[test]
    fn rejects_unknown_parameters_through_the_real_extractor() {
        let uri: Uri = "https://maero.dk/v1/fetch?url=https://maero.dk&nope=1"
            .parse()
            .expect("valid uri");

        assert!(Query::<FetchParams>::try_from_uri(&uri).is_err());
    }

    #[test]
    fn parses_fetch_params() {
        let query =
            fetch_params("url=https://maero.dk&redirects=2&include=redirects").expect("valid");

        assert_eq!(query.url().expect("valid").as_str(), "https://maero.dk/");
        assert_eq!(query.redirects().expect("valid"), 2);
        assert!(query.includes(FETCH_INCLUDES).expect("valid").redirects);
        assert!(!query.includes(FETCH_INCLUDES).expect("valid").headers);
    }

    #[test]
    fn parses_repeated_includes() {
        let query =
            fetch_params("url=https://maero.dk&include=redirects&include=headers").expect("valid");
        let includes = query.includes(FETCH_INCLUDES).expect("valid");

        assert!(includes.redirects);
        assert!(includes.headers);
    }

    #[test]
    fn parses_comma_separated_includes() {
        let query =
            fetch_params("url=https://maero.dk&include=redirects,%20headers").expect("valid");
        let includes = query.includes(FETCH_INCLUDES).expect("valid");

        assert!(includes.redirects);
        assert!(includes.headers);
    }

    #[test]
    fn defaults_redirects() {
        let query = fetch_params("url=https://maero.dk").expect("valid");

        assert_eq!(query.redirects().expect("valid"), DEFAULT_REDIRECTS);
        assert_eq!(
            query.includes(FETCH_INCLUDES).expect("valid"),
            IncludeSet::default()
        );
    }

    #[test]
    fn rejects_out_of_range_redirects() {
        let query = fetch_params("url=https://maero.dk&redirects=6").expect("valid");

        assert!(matches!(query.redirects(), Err(ApiError::InvalidParam(_))));

        assert!(fetch_params("url=https://maero.dk&redirects=-1").is_err());
    }

    #[test]
    fn rejects_non_integer_redirects() {
        assert!(fetch_params("url=https://maero.dk&redirects=abc").is_err());
    }

    #[test]
    fn requires_a_url() {
        assert!(fetch_params("").is_err());

        let query = fetch_params("url=not a url").expect("valid");
        assert!(matches!(query.url(), Err(ApiError::InvalidParam(_))));
    }

    #[test]
    fn rejects_unknown_parameters_and_includes() {
        assert!(fetch_params("url=https://maero.dk&nope=1").is_err());

        let query = fetch_params("url=https://maero.dk&include=nope").expect("valid");
        assert!(matches!(
            query.includes(FETCH_INCLUDES),
            Err(ApiError::InvalidParam(_))
        ));

        // `include=headers` is valid for the fetch endpoint, but not for peek.
        let peek_query = fetch_params("url=https://maero.dk&include=headers").expect("valid");
        assert!(peek_query.includes(PEEK_INCLUDES).is_err());
        assert!(peek_query.includes(FETCH_INCLUDES).is_ok());
    }

    #[test]
    fn extracts_search_params_through_the_real_extractor() {
        let uri: Uri = "https://maero.dk/v1/search?query=rust+programming&limit=10"
            .parse()
            .expect("valid uri");

        let Query(params) = Query::<SearchParams>::try_from_uri(&uri).expect("valid query");

        assert!(params.validate().is_ok());
        assert_eq!(params.query, "rust programming");
        assert_eq!(params.limit, Some(10));
        assert_eq!(params.timeout, None);
    }

    #[test]
    fn extracts_the_search_timeout_through_the_real_extractor() {
        let uri: Uri = "https://maero.dk/v1/search?query=rust&timeout=10"
            .parse()
            .expect("valid uri");

        let Query(query) = Query::<SearchParams>::try_from_uri(&uri).expect("valid query");

        assert!(query.validate().is_ok());
        assert_eq!(query.timeout, Some(10));
    }

    #[test]
    fn rejects_out_of_range_search_timeouts() {
        for (raw, expected) in [
            (
                "query=rust&timeout=0",
                "the timeout parameter must be at least",
            ),
            (
                "query=rust&timeout=31",
                "the timeout parameter must be between",
            ),
        ] {
            let query = search_params(raw).expect("valid");

            assert!(
                matches!(query.validate(), Err(ApiError::InvalidParam(message)) if message.contains(expected)),
                "expected {raw} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_non_integer_search_timeouts() {
        let uri: Uri = "https://maero.dk/v1/search?query=rust&timeout=1.5"
            .parse()
            .expect("valid uri");

        assert!(Query::<SearchParams>::try_from_uri(&uri).is_err());
    }

    #[test]
    fn rejects_an_empty_search_params() {
        let uri: Uri = "https://maero.dk/v1/search?query="
            .parse()
            .expect("valid uri");
        let Query(query) = Query::<SearchParams>::try_from_uri(&uri).expect("valid query");

        assert!(matches!(query.validate(), Err(ApiError::InvalidParam(_))));
    }

    #[test]
    fn rejects_unknown_search_parameters() {
        let uri: Uri = "https://maero.dk/v1/search?query=rust&nope=1"
            .parse()
            .expect("valid uri");

        assert!(Query::<SearchParams>::try_from_uri(&uri).is_err());
    }
}
