use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use htmlize::unescape;
use regex::Regex;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, HeaderValue, PRAGMA, REFERER, SET_COOKIE,
    UPGRADE_INSECURE_REQUESTS,
};
use reqwest::redirect::Policy;
use scraper::{ElementRef, Html, Node, Selector};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{debug, error};

use super::{BASE_URL, ClientOptions, Error, ImageResult, SearchResult};

/// The `Accept` header sent for document (navigation) requests.
const ACCEPT_DOCUMENT: HeaderValue =
    HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8");
/// The `Accept` header sent for server-sent event stream requests.
const ACCEPT_EVENT_STREAM: HeaderValue = HeaderValue::from_static("text/event-stream");

/// Represents a message parsed from a Kagi socket stream.
///
/// Messages arrive either framed as standard server-sent events or as `Tag:JSON_BODY\0\n`
/// chunks; in both cases each message carries a tag and a JSON payload.
#[derive(Deserialize, Debug)]
struct KagiMessage {
    /// The message tag (e.g., "search", "search.info", "meta").
    /// This is extracted from the wire prefix or the JSON body.
    pub tag: String,
    /// The flexible payload. Using `Value` allows this struct to handle
    /// diverse message types (HTML strings, objects, or nulls) without breaking.
    pub payload: Option<Value>,
}

/// The structured search results payload of a `search_results_json` message.
#[derive(Deserialize)]
struct SearchResultsJson {
    #[serde(default)]
    items: Vec<JsonSearchResult>,
}

/// A single structured result within a `search_results_json` payload.
#[derive(Deserialize)]
struct JsonSearchResult {
    title: String,
    url: String,
    #[serde(default)]
    snippet: String,
}

#[derive(Clone, Debug)]
struct Session {
    /// The nonce used for the first stream request of the session.
    nonce: Option<String>,
    /// When the nonce was created.
    created_at: Instant,
}

/// Client for searching with Kagi.
pub struct Client {
    /// HTTP client with a cookie jar.
    http: reqwest::Client,
    /// Kagi login token.
    token: SecretString,
    /// Session details.
    session: RwLock<Option<Session>>,
    /// The duration of a single session.
    session_duration: Duration,
    /// The `Accept-Language` header sent with requests.
    language: HeaderValue,
}

impl Session {
    fn is_valid(&self, session_duration: Duration) -> bool {
        self.created_at.elapsed() < session_duration
    }

    /// Returns the nonce on the first call and `None` afterwards, mirroring a browser which
    /// only sends the page's `sse_nonce` on its initial connection.
    const fn take_nonce(&mut self) -> Option<String> {
        self.nonce.take()
    }
}

impl Client {
    /// Constructs a new [`Client`] for searching with Kagi using the given session token and
    /// default options.
    ///
    /// The token is the value of the `kagi_session` cookie from an authenticated browser session.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client fails to build.
    pub fn with_token(token: impl Into<SecretString>) -> Client {
        Self::with_token_and_options(token, ClientOptions::default())
            .expect("could not build http client")
    }

    /// Constructs a new [`Client`] for searching with Kagi using the given session token and
    /// options.
    ///
    /// The token is the value of the `kagi_session` cookie from an authenticated browser session.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client fails to build.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidHeader`] if a configured header value is invalid.
    pub fn with_token_and_options(
        token: impl Into<SecretString>,
        options: ClientOptions,
    ) -> Result<Client, Error> {
        let client = reqwest::ClientBuilder::new()
            .cookie_store(true)
            .redirect(Policy::none())
            .timeout(options.timeout)
            .user_agent(options.user_agent)
            .build()
            .map_err(Error::BuildClient)?;

        Ok(Client {
            http: client,
            token: token.into(),
            session: RwLock::new(None),
            session_duration: options.session_duration,
            language: HeaderValue::from_str(&options.language)?,
        })
    }

    /// Attaches the session authorization header to the request, mirroring the browser which
    /// sends it with every request.
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.header("X-Kagi-Authorization", self.token.expose_secret())
    }

    /// Builds a request shaped like a browser document navigation.
    fn document_request(&self, url: &str) -> reqwest::RequestBuilder {
        self.authorize(self.http.get(url))
            .header(ACCEPT, ACCEPT_DOCUMENT)
            .header(ACCEPT_LANGUAGE, self.language.clone())
            .header("Sec-Fetch-Dest", HeaderValue::from_static("document"))
            .header("Sec-Fetch-Mode", HeaderValue::from_static("navigate"))
            .header("Sec-Fetch-Site", HeaderValue::from_static("none"))
            .header("Sec-Fetch-User", HeaderValue::from_static("?1"))
            .header(UPGRADE_INSECURE_REQUESTS, HeaderValue::from_static("1"))
    }

    /// Builds a request shaped like a browser `EventSource` connection.
    fn stream_request(&self, url: reqwest::Url) -> reqwest::RequestBuilder {
        self.authorize(self.http.get(url))
            .header(ACCEPT, ACCEPT_EVENT_STREAM)
            .header(ACCEPT_LANGUAGE, self.language.clone())
            .header("Sec-Fetch-Dest", HeaderValue::from_static("empty"))
            .header("Sec-Fetch-Mode", HeaderValue::from_static("cors"))
            .header("Sec-Fetch-Site", HeaderValue::from_static("same-origin"))
            .header(PRAGMA, HeaderValue::from_static("no-cache"))
            .header(CACHE_CONTROL, HeaderValue::from_static("no-cache"))
            .header("Priority", HeaderValue::from_static("u=4"))
    }

    async fn fetch_new_session_data(&self) -> Result<String, Error> {
        // Issue a request with the login token to receive session cookies. The token is part of
        // the query string, so the request is intentionally not logged here.
        let token_url = url_with_query("/search", &[("token", self.token.expose_secret())]);
        let req = self.document_request(token_url.as_str());
        debug!("requesting session cookies");

        let res = req.send().await.map_err(Error::RequestSession)?;
        if !res.headers().contains_key(SET_COOKIE) {
            error!("the response does not include set-cookie headers!");
            return Err(Error::SessionCookies);
        }

        // Request the main page to receive a nonce for the first stream request.
        debug!("requesting nonce");
        let req = self.document_request(BASE_URL);
        let res = req.send().await.map_err(Error::RequestNonce)?;
        let body = res.text().await.map_err(Error::ReadNonce)?;

        extract_nonce(&body).ok_or(Error::Nonce)
    }

    /// Returns the nonce to include with the next stream request, refreshing the session if
    /// necessary.
    ///
    /// The nonce is only returned once per session, mirroring a browser which sends the page's
    /// `sse_nonce` on its initial connection only.
    // `Option::map_or_else` is not viable here: the `None` arm has to store the refreshed
    // session back into the guard.
    #[allow(clippy::option_if_let_else)]
    async fn take_nonce(&self) -> Result<Option<String>, Error> {
        {
            let mut guard = self.session.write().await;

            if let Some(session) = guard
                .as_mut()
                .filter(|session| session.is_valid(self.session_duration))
            {
                return Ok(session.take_nonce());
            }
        }

        debug!("session expired or missing, refreshing...");
        let nonce = self.fetch_new_session_data().await?;

        // Double check and use the current session if it was refreshed while we were fetching
        // the new one.
        let taken = {
            let mut guard = self.session.write().await;

            if let Some(session) = guard
                .as_mut()
                .filter(|session| session.is_valid(self.session_duration))
            {
                session.take_nonce()
            } else {
                let mut session = Session {
                    nonce: Some(nonce),
                    created_at: Instant::now(),
                };
                let taken = session.take_nonce();
                *guard = Some(session);

                taken
            }
        };

        Ok(taken)
    }

    /// Connects to one of the socket stream endpoints (`search` or `images`) and returns the
    /// messages parsed from the response.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if a session could not be established, the stream request could not
    /// be sent, or the response could not be read.
    async fn stream(&self, endpoint: &str, query: &str) -> Result<Vec<KagiMessage>, Error> {
        let nonce = self.take_nonce().await?;
        let url = url_with_query(&format!("/socket/{endpoint}"), &[("q", query)]);
        let referer = url_with_query(&format!("/{endpoint}"), &[("q", query)]);
        let req = self.stream_request(url).header(REFERER, referer.as_str());
        let req = if let Some(nonce) = nonce {
            req.query(&[("nonce", nonce)])
        } else {
            req
        };

        debug!(%endpoint, "connecting to stream");
        let res = req.send().await.map_err(Error::StreamRequest)?;
        let res = res.error_for_status().map_err(Error::StreamStatus)?;
        let body = res.text().await.map_err(Error::StreamRequestBody)?;

        Ok(parse_stream(&body))
    }

    /// Searches Kagi with the given query and returns the parsed search results.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if a session could not be established, the stream request could not
    /// be sent, or the response could not be read.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>, Error> {
        let messages = self.stream("search", query).await?;

        Ok(parse_search_result_messages(&messages))
    }

    /// Searches Kagi images with the given query and returns the parsed image results.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if a session could not be established, the stream request could not
    /// be sent, or the response could not be read.
    pub async fn images(&self, query: &str) -> Result<Vec<ImageResult>, Error> {
        let messages = self.stream("images", query).await?;

        Ok(parse_image_result_messages(&messages))
    }
}

/// Builds a Kagi URL with the given query parameters, percent-encoding as needed.
///
/// # Panics
///
/// Panics if the static [`BASE_URL`] is not a valid URL.
fn url_with_query(path: &str, params: &[(&str, &str)]) -> reqwest::Url {
    reqwest::Url::parse_with_params(&format!("{BASE_URL}{path}"), params)
        .expect("the static base url always produces a valid url")
}

// Extracts the `window.sse_nonce` value from the raw HTML content.
fn extract_nonce(html: &str) -> Option<String> {
    nonce_regex()
        .captures(html)
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
}

/// Returns the regex matching the `window.sse_nonce` value in the page's HTML, compiled once.
fn nonce_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();

    RE.get_or_init(|| {
        Regex::new(r#"window\.sse_nonce\s*=\s*"([^"]+)""#).expect("the nonce pattern is valid")
    })
}

/// Parses a raw stream response body into a vector of `KagiMessage`s, detecting whether the
/// server responded with the standard server-sent event framing or the legacy null-delimited
/// wire format.
fn parse_stream(raw_body: &str) -> Vec<KagiMessage> {
    if raw_body.lines().any(|line| line.starts_with("data:")) {
        parse_sse_stream(raw_body)
    } else {
        parse_kagi_stream(raw_body)
    }
}

/// Parses a standard server-sent event stream where each `data:` line carries a JSON array of
/// messages.
///
/// Any non-data lines (such as `id:` fields or the `hi` greeting) are ignored.
fn parse_sse_stream(raw_body: &str) -> Vec<KagiMessage> {
    raw_body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .filter_map(|json| serde_json::from_str::<Vec<KagiMessage>>(json).ok())
        .flatten()
        .collect()
}

/// Parses a raw stream response body in the legacy `Tag:JSON_BODY\0\n` wire format into a vector
/// of `KagiMessage`s.
///
/// This handles the specific Kagi wire format:
/// 1. Splits by `\0\n` delimiter.
/// 2. Splits each chunk at the first `:` into (`WireTag`, `JsonBody`).
/// 3. Deserializes the JSON body.
/// 4. Ensures the `tag` field is populated.
fn parse_kagi_stream(raw_body: &str) -> Vec<KagiMessage> {
    raw_body
        .split("\0\n")
        .filter(|chunk| !chunk.is_empty())
        .filter_map(|chunk| {
            // Split wire format: "tag:json_data"
            let (wire_tag, json_str) = chunk.split_once(':')?;
            // Parse JSON body
            let mut message: KagiMessage = serde_json::from_str(json_str).ok()?;
            // Normalize Tag: If the JSON body didn't have a tag, use the wire tag.
            if message.tag.is_empty() {
                message.tag = wire_tag.to_string();
            }

            Some(message)
        })
        .collect()
}

fn parse_search_result_messages(messages: &[KagiMessage]) -> Vec<SearchResult> {
    // Prefer the structured results payload when the server provides one.
    for message in messages
        .iter()
        .filter(|message| message.tag == "search_results_json")
    {
        let Some(payload) = message.payload.as_ref().and_then(Value::as_str) else {
            continue;
        };
        let Ok(results) = serde_json::from_str::<SearchResultsJson>(payload) else {
            continue;
        };

        if !results.items.is_empty() {
            return results
                .items
                .into_iter()
                .map(|item| SearchResult {
                    title: item.title,
                    url: item.url,
                    description: unescape(&item.snippet).into_owned(),
                })
                .collect();
        }
    }

    // Fall back to parsing the HTML fragments of `search` messages.
    let mut result: Vec<SearchResult> = vec![];
    let search_msgs = messages.iter().filter(|x| x.tag == "search");

    for msg in search_msgs {
        if let Some(content) = msg
            .payload
            .as_ref()
            .and_then(|p| p.get("content").and_then(|v| v.as_str()))
        {
            let mut results = parse_search_results_html(content);

            result.append(&mut results);
        }
    }

    result
}

/// The CSS selectors for parsing the search and image result fragments.
struct Selectors {
    /// A search result block.
    search_result: Selector,
    /// The title link of a search result block.
    title_link: Selector,
    /// The description of a search result block.
    description: Selector,
    /// An image result block.
    image_item: Selector,
    /// The thumbnail image of an image result block.
    image_thumbnail: Selector,
}

impl Selectors {
    /// Parses the result selectors.
    ///
    /// # Panics
    ///
    /// Panics if a selector is invalid; the selectors are compile-time constants.
    fn new() -> Self {
        Self {
            search_result: Selector::parse("div.search-result").expect("search result selector"),
            title_link: Selector::parse("h3.__sri-title-box > a.__sri_title_link")
                .expect("title link selector"),
            description: Selector::parse("div.__sri-desc > div").expect("description selector"),
            image_item: Selector::parse("div._0_img-results > div.item").expect("item selector"),
            image_thumbnail: Selector::parse("img._0_img_src").expect("thumbnail selector"),
        }
    }
}

/// Returns the pre-compiled result selectors.
fn selectors() -> &'static Selectors {
    static SELECTORS: OnceLock<Selectors> = OnceLock::new();

    SELECTORS.get_or_init(Selectors::new)
}

fn parse_search_results_html(html: &str) -> Vec<SearchResult> {
    let fragment = Html::parse_fragment(html);
    let selectors = selectors();

    let search_results = fragment.select(&selectors.search_result);

    let mut results = Vec::new();

    for result_div in search_results {
        let title = result_div.select(&selectors.title_link).next();
        let description = result_div.select(&selectors.description).next();

        if let (Some(title), Some(description)) = (title, description) {
            let url = title.attr("href").unwrap_or("").to_string();

            results.push(SearchResult {
                title: title.text().collect::<String>().trim().to_owned(),
                url,
                description: extract_topmost_text(&description),
            });
        }
    }

    results
}

fn parse_image_result_messages(messages: &[KagiMessage]) -> Vec<ImageResult> {
    let mut result: Vec<ImageResult> = Vec::new();

    for message in messages.iter().filter(|message| message.tag == "images") {
        if let Some(content) = message
            .payload
            .as_ref()
            .and_then(|payload| payload.get("content").and_then(Value::as_str))
        {
            result.extend(parse_image_results_html(content));
        }
    }

    result
}

fn parse_image_results_html(html: &str) -> Vec<ImageResult> {
    let fragment = Html::parse_fragment(html);
    let selectors = selectors();

    let mut results = Vec::new();

    for item in fragment.select(&selectors.image_item) {
        let value = item.value();
        let (Some(title), Some(page_url), Some(image_url)) = (
            value.attr("data-title"),
            value.attr("data-host_url"),
            value.attr("data-content_url"),
        ) else {
            continue;
        };
        let Some(thumbnail) = item
            .select(&selectors.image_thumbnail)
            .next()
            .and_then(|img| img.attr("src"))
        else {
            continue;
        };

        results.push(ImageResult {
            title: unescape(title).into_owned(),
            page_url: page_url.to_string(),
            image_url: image_url.to_string(),
            thumbnail_url: thumbnail.to_string(),
            width: value
                .attr("data-width")
                .and_then(|width| width.parse().ok())
                .unwrap_or(0),
            height: value
                .attr("data-height")
                .and_then(|height| height.parse().ok())
                .unwrap_or(0),
            host: value.attr("data-host").unwrap_or_default().to_string(),
            rank: value
                .attr("data-rank")
                .and_then(|rank| rank.parse().ok())
                .unwrap_or(0),
        });
    }

    results
}

fn extract_topmost_text(elem: &'_ ElementRef<'_>) -> String {
    let extracted_text: String = elem
        .children()
        .filter_map(|node| match node.value() {
            Node::Text(text_node) => {
                let text = text_node.trim();

                if text.is_empty() { None } else { Some(text) }
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");

    extracted_text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn read_fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);

        String::from_utf8(std::fs::read(path).expect("could not read fixture"))
            .expect("fixture is not valid UTF-8")
    }

    #[test]
    fn test_parse_legacy_stream() {
        let stream = read_fixture("search_stream.bin");
        let result = parse_kagi_stream(&stream);

        assert_eq!(result.len(), 8); // 8 messages
    }

    #[test]
    fn test_parse_sse_stream() {
        let stream = read_fixture("search_stream_sse.bin");
        let result = parse_stream(&stream);

        assert!(
            result
                .iter()
                .any(|message| message.tag == "search_results_json")
        );
        assert!(result.iter().any(|message| message.tag == "search"));
        assert!(result.iter().any(|message| message.tag == "search.info"));
    }

    #[test]
    fn test_extract_nonce() {
        let html = read_fixture("landing.html");
        let result = extract_nonce(&html).expect("could not extract nonce");

        assert_eq!(result, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn test_search_results_structured() {
        let stream = read_fixture("search_stream_sse.bin");
        let messages = parse_stream(&stream);
        let results = parse_search_result_messages(&messages);

        assert_ne!(results, Vec::new());

        let result = results.first().unwrap();

        assert_eq!(result.title, "Hello, world - Wikipedia");
        assert_eq!(result.url, "https://en.wikipedia.org/wiki/Hello,_world");
        // Snippets arrive HTML-entity encoded and must be decoded.
        assert!(result.description.contains('"'));
        assert!(!result.description.contains("&quot;"));
    }

    #[test]
    fn test_search_results_html_fallback() {
        let stream = read_fixture("search_stream.bin");
        let messages = parse_kagi_stream(&stream);
        let results = parse_search_result_messages(&messages);

        assert_eq!(results.len(), 19);

        let result = results.first().unwrap();

        assert_eq!(result.title, "Vitamin D - Health Professional Fact Sheet");
        assert_eq!(
            result.description,
            "Vitamin D (also referred to as calciferol) is a fat-soluble vitamin that is naturally present in a few foods, added to others, and available as a dietary ..."
        );
        assert_eq!(
            result.url,
            "https://ods.od.nih.gov/factsheets/VitaminD-HealthProfessional/"
        );
    }

    #[test]
    fn test_image_results() {
        let stream = read_fixture("images_stream_sse.bin");
        let messages = parse_stream(&stream);
        let results = parse_image_result_messages(&messages);

        assert_eq!(results.len(), 5);

        let result = results.first().unwrap();

        assert_eq!(
            result.title,
            "How to Do a Reverse Image Search From Your Phone"
        );
        assert_eq!(
            result.page_url,
            "https://www.entrepreneur.com/business-news/how-to-do-a-reverse-image-search-from-your-phone/297541"
        );
        assert_eq!(
            result.image_url,
            "https://assets.entrepreneur.com/images/misc/1500561136_1.jpg"
        );
        assert!(
            result
                .thumbnail_url
                .starts_with("https://p.kagi.com/proxy/")
        );
        assert_eq!(result.width, 740);
        assert_eq!(result.height, 475);
        assert_eq!(result.host, "www.entrepreneur.com");
        assert_eq!(result.rank, 0);
    }

    #[test]
    fn test_session_nonce_is_only_taken_once() {
        let mut session = Session {
            nonce: Some("nonce".to_string()),
            created_at: Instant::now(),
        };

        assert_eq!(session.take_nonce().as_deref(), Some("nonce"));
        assert_eq!(session.take_nonce(), None);
    }

    #[test]
    fn test_invalid_language_is_rejected() {
        let options = ClientOptions {
            language: "invalid\nlanguage".to_string(),
            ..ClientOptions::default()
        };

        assert!(matches!(
            Client::with_token_and_options("token", options),
            Err(Error::InvalidHeader(_))
        ));
    }
}
