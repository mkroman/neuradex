use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use htmlize::unescape;
use regex::Regex;
use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, HeaderValue, PRAGMA, REFERER, SET_COOKIE,
    UPGRADE_INSECURE_REQUESTS,
};
use reqwest::redirect::Policy;
use scraper::{ElementRef, Html, Node, Selector};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::time::Instant;
use tracing::{debug, error};

use super::{BASE_URL, ClientOptions, Error, ImageResult, SearchResult};

/// The base duration of the exponential backoff between session fetch retries.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
/// The upper bound of the exponential backoff between session fetch retries.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

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

/// A single persistent session slot.
///
/// A slot is either checked out — used by exactly one request, which may be fetching a fresh
/// session for it — or parked with an idle session. Each slot carries its own HTTP client with
/// a private cookie jar, so concurrent sessions do not overwrite each other's cookies, and its
/// own retry bookkeeping: after a failed fetch the slot backs off exponentially before the
/// next fetch attempt.
struct Slot {
    /// The mutable state of the slot. Guarded by a synchronous lock that is never held across
    /// an await point.
    inner: Mutex<SlotInner>,
    /// The HTTP client used for this session's requests, with a private cookie jar.
    http: reqwest::Client,
}

/// The guarded state of a [`Slot`].
struct SlotInner {
    /// The parked session, or `None` while the slot is checked out or has not been
    /// established yet.
    session: Option<Session>,
    /// Whether the slot is currently checked out by a request.
    busy: bool,
    /// The number of consecutive failed session fetches since the last success.
    failures: u32,
    /// The earliest instant at which the next session fetch may be attempted.
    next_retry: Option<Instant>,
}

impl SlotInner {
    /// Marks the slot checked out and returns the parked session.
    fn check_out(&mut self) -> Option<Session> {
        debug_assert!(!self.busy, "checked out an already busy slot");
        self.busy = true;
        self.session.take()
    }
}

/// What kind of slot a check-out is looking for, in preference order: a parked session that is
/// still valid, one that has expired, or an empty slot to establish a session on.
#[derive(Clone, Copy)]
enum SlotKind {
    /// A parked session that is still valid.
    Valid,
    /// A parked session that has expired and needs a refresh.
    Expired,
    /// A slot without a session, waiting to be established.
    Empty,
}

impl SlotKind {
    fn matches(self, inner: &SlotInner, session_duration: Duration) -> bool {
        match self {
            SlotKind::Valid => inner
                .session
                .as_ref()
                .is_some_and(|session| session.is_valid(session_duration)),
            SlotKind::Expired => inner
                .session
                .as_ref()
                .is_some_and(|session| !session.is_valid(session_duration)),
            SlotKind::Empty => inner.session.is_none(),
        }
    }
}

/// A checked-out session slot, restored to the pool on drop.
///
/// Dropping the lease — including when the caller's future is cancelled at an await point —
/// parks the session back in its slot and signals a request waiting for a session to free up.
struct SessionLease {
    /// The slot the session was checked out from.
    slot: Arc<Slot>,
    /// The checked-out session, parked again on drop.
    session: Option<Session>,
    /// Signals requests waiting for a session to be checked back in; one permit per check-in.
    check_in: Arc<Semaphore>,
}

impl SessionLease {
    /// Returns the session's HTTP client, which carries this session's cookie jar.
    fn http(&self) -> &reqwest::Client {
        &self.slot.http
    }

    /// Returns the session's nonce, if it had one.
    fn take_nonce(&mut self) -> Option<String> {
        self.session.as_mut().and_then(Session::take_nonce)
    }

    /// Drops the checked-out session and records a failed fetch on the slot: the
    /// next request re-establishes the session after the slot's backoff.
    fn invalidate(&mut self) {
        self.session = None;
        Client::record_failure(&self.slot);
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        {
            let mut inner = self.slot.inner.lock().unwrap();
            inner.busy = false;
            inner.session = self.session.take();
        }

        // One permit per check-in: permits accumulate when no request is waiting yet, so a
        // burst of check-ins is never collapsed into a single wakeup.
        self.check_in.add_permits(1);
    }
}

/// Produces the nonce for a fresh session with the given HTTP client.
///
/// The production implementation performs the session-cookie and nonce requests; tests supply
/// a mock. The future borrows the session's HTTP client so that its requests land in the
/// session's cookie jar.
type SessionFetchFuture<'a> = Pin<Box<dyn Future<Output = Result<String, Error>> + Send + 'a>>;
type SessionFetcher = dyn for<'a> Fn(&'a reqwest::Client) -> SessionFetchFuture<'a> + Send + Sync;

/// Client for searching with Kagi.
///
/// The client keeps a pool of up to [`ClientOptions::max_sessions`] persistent sessions. Each
/// in-flight search uses its own session; an inrush of requests grows the pool by establishing
/// sessions one at a time, and requests beyond the pool capacity wait for a session to free
/// up. Sessions expire independently and are refreshed with a new nonce after their own
/// duration; a stream response rejecting the session's credentials (401/403) drops the session
/// so the next request re-establishes it.
pub struct Client {
    /// Kagi login token.
    token: Arc<SecretString>,
    /// The `Accept-Language` header sent with requests.
    language: HeaderValue,
    /// The maximum number of simultaneous sessions.
    max_sessions: usize,
    /// The duration of a single session.
    session_duration: Duration,
    /// The duration before an HTTP request times out; used to build per-session clients.
    timeout: Duration,
    /// The `User-Agent` used to build per-session clients.
    user_agent: HeaderValue,
    /// The session slots, bounded by [`Client::max_sessions`].
    slots: Mutex<Vec<Arc<Slot>>>,
    /// Serializes session-creation fetches so that sessions are established one at a time.
    creation: AsyncMutex<()>,
    /// Signals requests waiting for a session to be checked back in; one permit per check-in.
    check_in: Arc<Semaphore>,
    /// Produces the nonce for a fresh session.
    fetch: Box<SessionFetcher>,
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
        Self::with_token_and_options(token, &ClientOptions::default())
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
        options: &ClientOptions,
    ) -> Result<Client, Error> {
        let token = Arc::new(token.into());
        let language = HeaderValue::from_str(&options.language)?;

        let fetch: Box<SessionFetcher> = {
            let token = Arc::clone(&token);

            Box::new(move |http: &reqwest::Client| -> SessionFetchFuture<'_> {
                let token = Arc::clone(&token);
                let language = language.clone();

                Box::pin(async move { http_fetch_session(http, &token, &language).await })
            })
        };

        Self::assemble(token, options, fetch)
    }

    /// Assembles a client from its parts. Split out so tests can supply their own fetcher.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidHeader`] if a configured header value is invalid.
    fn assemble(
        token: Arc<SecretString>,
        options: &ClientOptions,
        fetch: Box<SessionFetcher>,
    ) -> Result<Client, Error> {
        Ok(Client {
            token,
            language: HeaderValue::from_str(&options.language)?,
            max_sessions: options.max_sessions.max(1),
            session_duration: options.session_duration,
            timeout: options.timeout,
            user_agent: HeaderValue::from_str(&options.user_agent)?,
            slots: Mutex::new(Vec::new()),
            creation: AsyncMutex::new(()),
            check_in: Arc::new(Semaphore::new(0)),
            fetch,
        })
    }

    /// Produces a fresh session: checks a slot out of the pool and returns a lease holding the
    /// session with its nonce, establishing or refreshing the session if necessary.
    ///
    /// A checked-out session is used by exactly one request at a time, so there is never more
    /// than one request fetching for a session. Idle sessions are preferred over new ones: a
    /// valid session is used as-is, an expired session is refreshed in place, and only when no
    /// session is parked does the pool grow — one session at a time, bounded by
    /// [`Client::max_sessions`] — after which further requests wait for a session to be
    /// checked back in.
    ///
    /// A failed fetch backs off exponentially for the slot before the next attempt; the
    /// waiting request holds the slot checked out while backing off. None of the waiting has
    /// a deadline: a request stays pending for as long as its caller does.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the session could not be established or refreshed.
    async fn acquire(&self) -> Result<SessionLease, Error> {
        loop {
            // An idle, valid session is used without any fetch.
            if let Some((slot, Some(session))) = self.try_check_out(SlotKind::Valid) {
                return Ok(self.lease(slot, Some(session)));
            }

            // An idle, expired session is refreshed in place. Refreshes of distinct sessions
            // are independent and are not serialized by the creation gate.
            if let Some((slot, expired)) = self.try_check_out(SlotKind::Expired) {
                let mut lease = self.lease(slot, expired);
                let session = self.refresh(&lease.slot).await?;
                lease.session = Some(session);

                return Ok(lease);
            }

            // No parked session: establish one, either on an idle empty slot or, if the pool
            // has room, on a new slot. Both hold the creation gate across the fetch, so that
            // sessions are created one at a time. While queued on the gate, a session may
            // have been checked back in — re-scan before growing the pool.
            let slot = match self.try_check_out(SlotKind::Empty) {
                Some((slot, _ /* none parked */)) => Some(slot),
                None if self.slot_count() < self.max_sessions => {
                    // Growing the pool needs the gate; while queued, a session may have
                    // been checked back in — re-scan before adding a slot.
                    let gate = self.creation.lock().await;

                    if self.has_idle() || self.slot_count() >= self.max_sessions {
                        drop(gate);
                        continue;
                    }

                    let slot = self.add_slot()?;
                    drop(gate);

                    Some(slot)
                }
                None => None,
            };

            let Some(slot) = slot else {
                // The pool is at capacity with every slot checked out: wait for a check-in.
                // Each check-in adds one permit; the permit is consumed rather than returned so
                // the re-scan below decides progress and the next check-in produces the next
                // wakeup.
                self.check_in
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("the semaphore is never closed")
                    .forget();
                continue;
            };

            let mut lease = self.lease(slot, None);
            let session = self.establish(&lease.slot).await?;
            lease.session = Some(session);

            return Ok(lease);
        }
    }

    /// Wraps a checked-out slot and session into a lease.
    fn lease(&self, slot: Arc<Slot>, session: Option<Session>) -> SessionLease {
        SessionLease {
            slot,
            session,
            check_in: Arc::clone(&self.check_in),
        }
    }

    /// Checks out the first parked slot matching `kind`, returning it with its session.
    fn try_check_out(&self, kind: SlotKind) -> Option<(Arc<Slot>, Option<Session>)> {
        let slots = self.slots.lock().unwrap();

        slots.iter().find_map(|slot| {
            let mut inner = slot.inner.lock().unwrap();

            if inner.busy || !kind.matches(&inner, self.session_duration) {
                return None;
            }

            let session = inner.check_out();
            drop(inner);

            Some((Arc::clone(slot), session))
        })
    }

    /// Returns whether any slot is parked, of any kind.
    fn has_idle(&self) -> bool {
        let slots = self.slots.lock().unwrap();

        slots.iter().any(|slot| {
            let inner = slot.inner.lock().unwrap();
            !inner.busy
        })
    }

    /// Returns the number of slots in the pool.
    fn slot_count(&self) -> usize {
        self.slots.lock().unwrap().len()
    }

    /// Adds a new, empty slot to the pool.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the session's HTTP client fails to build.
    fn add_slot(&self) -> Result<Arc<Slot>, Error> {
        let http = reqwest::ClientBuilder::new()
            .cookie_store(true)
            .redirect(Policy::none())
            .timeout(self.timeout)
            .user_agent(self.user_agent.clone())
            .build()
            .map_err(Error::BuildClient)?;

        let slot = Arc::new(Slot {
            inner: Mutex::new(SlotInner {
                session: None,
                // The slot is born checked out: the caller establishes its session.
                busy: true,
                failures: 0,
                next_retry: None,
            }),
            http,
        });

        self.slots.lock().unwrap().push(Arc::clone(&slot));

        Ok(slot)
    }

    /// Waits out the slot's remaining backoff, if one is scheduled.
    async fn wait_backoff(&self, slot: &Slot) {
        let next_retry = slot.inner.lock().unwrap().next_retry;

        if let Some(next_retry) = next_retry
            && next_retry > Instant::now()
        {
            tokio::time::sleep_until(next_retry).await;
        }
    }

    /// Records a failed fetch on the slot: its backoff doubles, bounded.
    fn record_failure(slot: &Slot) {
        let mut inner = slot.inner.lock().unwrap();
        inner.failures = inner.failures.saturating_add(1);

        let backoff = backoff_after_failures(inner.failures);
        debug!(
            failures = inner.failures,
            ?backoff,
            "backing off session fetch"
        );
        inner.next_retry = Some(Instant::now() + backoff);
    }

    /// Fetches a fresh session for the given checked-out slot.
    async fn fetch_session(&self, slot: &Slot) -> Result<Session, Error> {
        match (self.fetch)(&slot.http).await {
            Ok(nonce) => {
                {
                    let mut inner = slot.inner.lock().unwrap();
                    inner.failures = 0;
                    inner.next_retry = None;
                }

                Ok(Session {
                    nonce: Some(nonce),
                    created_at: Instant::now(),
                })
            }
            Err(error) => {
                Self::record_failure(slot);
                Err(error)
            }
        }
    }

    /// Refreshes the given expired, checked-out slot in place.
    ///
    /// Refreshes of distinct sessions are independent: this does not hold the creation gate.
    /// The slot's backoff is waited out first.
    async fn refresh(&self, slot: &Slot) -> Result<Session, Error> {
        self.wait_backoff(slot).await;
        self.fetch_session(slot).await
    }

    /// Establishes a session for the given empty, checked-out slot.
    ///
    /// The creation gate is held across the fetch, so at most one creation fetch runs at any
    /// moment. The slot's backoff is waited out before the gate is taken, so retry waits
    /// never block other creations.
    async fn establish(&self, slot: &Slot) -> Result<Session, Error> {
        self.wait_backoff(slot).await;

        let gate = self.creation.lock().await;
        let session = self.fetch_session(slot).await;
        drop(gate);

        session
    }

    /// Connects to one of the socket stream endpoints (`search` or `images`) and returns the
    /// messages parsed from the response.
    ///
    /// The session is checked out for the duration of the stream request and returned
    /// afterwards — or immediately, if the caller's future is dropped mid-request.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if a session could not be established, the stream request could not
    /// be sent, or the response could not be read.
    async fn stream(&self, endpoint: &str, query: &str) -> Result<Vec<KagiMessage>, Error> {
        let mut lease = self.acquire().await?;
        let nonce = lease.take_nonce();

        let url = url_with_query(&format!("/socket/{endpoint}"), &[("q", query)]);
        let referer = url_with_query(&format!("/{endpoint}"), &[("q", query)]);
        let req = stream_request(lease.http(), &self.token, &self.language, url)
            .header(REFERER, referer.as_str());
        let req = if let Some(nonce) = nonce {
            req.query(&[("nonce", nonce)])
        } else {
            req
        };

        debug!(%endpoint, "connecting to stream");
        let res = req.send().await.map_err(Error::StreamRequest)?;
        let res = match res.error_for_status() {
            Ok(res) => res,
            Err(error) => {
                // A rejected session is dropped: the next request re-establishes
                // it, after the slot's backoff. Other statuses keep the session —
                // they say nothing about the session itself.
                if error.status().is_some_and(invalidates_session) {
                    lease.invalidate();
                }

                return Err(Error::StreamStatus(error));
            }
        };
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

/// Fetches a fresh session with the given session HTTP client: issues a request with the login
/// token to receive the session cookies — the token is part of the query string, so the
/// request is intentionally not logged — and then requests the main page to extract the nonce
/// for the session's first stream request.
async fn http_fetch_session(
    http: &reqwest::Client,
    token: &SecretString,
    language: &HeaderValue,
) -> Result<String, Error> {
    let token_url = url_with_query("/search", &[("token", token.expose_secret())]);
    debug!("requesting session cookies");

    let res = document_request(http, token, language, token_url.as_str())
        .send()
        .await
        .map_err(Error::RequestSession)?;
    if !res.headers().contains_key(SET_COOKIE) {
        error!("the response does not include set-cookie headers!");
        return Err(Error::SessionCookies);
    }

    debug!("requesting nonce");
    let res = document_request(http, token, language, BASE_URL)
        .send()
        .await
        .map_err(Error::RequestNonce)?;
    let body = res.text().await.map_err(Error::ReadNonce)?;

    extract_nonce(&body).ok_or(Error::Nonce)
}

/// Attaches the session authorization header to the request, mirroring the browser which
/// sends it with every request.
fn authorize(request: reqwest::RequestBuilder, token: &SecretString) -> reqwest::RequestBuilder {
    request.header("X-Kagi-Authorization", token.expose_secret())
}

/// Builds a request shaped like a browser document navigation.
fn document_request(
    http: &reqwest::Client,
    token: &SecretString,
    language: &HeaderValue,
    url: &str,
) -> reqwest::RequestBuilder {
    authorize(http.get(url), token)
        .header(ACCEPT, ACCEPT_DOCUMENT)
        .header(ACCEPT_LANGUAGE, language.clone())
        .header("Sec-Fetch-Dest", HeaderValue::from_static("document"))
        .header("Sec-Fetch-Mode", HeaderValue::from_static("navigate"))
        .header("Sec-Fetch-Site", HeaderValue::from_static("none"))
        .header("Sec-Fetch-User", HeaderValue::from_static("?1"))
        .header(UPGRADE_INSECURE_REQUESTS, HeaderValue::from_static("1"))
}

/// Builds a request shaped like a browser `EventSource` connection.
fn stream_request(
    http: &reqwest::Client,
    token: &SecretString,
    language: &HeaderValue,
    url: reqwest::Url,
) -> reqwest::RequestBuilder {
    authorize(http.get(url), token)
        .header(ACCEPT, ACCEPT_EVENT_STREAM)
        .header(ACCEPT_LANGUAGE, language.clone())
        .header("Sec-Fetch-Dest", HeaderValue::from_static("empty"))
        .header("Sec-Fetch-Mode", HeaderValue::from_static("cors"))
        .header("Sec-Fetch-Site", HeaderValue::from_static("same-origin"))
        .header(PRAGMA, HeaderValue::from_static("no-cache"))
        .header(CACHE_CONTROL, HeaderValue::from_static("no-cache"))
        .header("Priority", HeaderValue::from_static("u=4"))
}

/// Returns the backoff duration after the given number of consecutive failed fetches:
/// exponential from [`BACKOFF_BASE`], doubling per failure and bounded by [`BACKOFF_MAX`].
fn backoff_after_failures(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);

    BACKOFF_BASE
        .checked_mul(1 << exponent)
        .map_or(BACKOFF_MAX, |backoff| backoff.min(BACKOFF_MAX))
}

/// Returns whether the stream response status invalidates the session: the
/// session's credentials were rejected, so it must be re-established with fresh
/// cookies and a new nonce. Any other status says nothing about the session
/// itself — rate limits and server errors keep it.
fn invalidates_session(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
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
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    /// A session fetcher that fails its first `failures` calls, optionally sleeps, and tracks
    /// the number of concurrent in-flight fetches.
    struct MockFetch {
        calls: AtomicUsize,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        failures: AtomicUsize,
        delay: Option<Duration>,
    }

    impl MockFetch {
        fn new(failures: u32, delay: Option<Duration>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                failures: AtomicUsize::new(failures as usize),
                delay,
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }

        fn max_in_flight(&self) -> usize {
            self.max_in_flight.load(Ordering::Relaxed)
        }

        async fn hit(&self) -> Result<String, Error> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;

            if call <= self.failures.load(Ordering::Relaxed) {
                return Err(Error::Nonce);
            }

            let in_flight = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_in_flight.fetch_max(in_flight, Ordering::AcqRel);

            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }

            self.in_flight.fetch_sub(1, Ordering::AcqRel);

            Ok(format!("nonce-{call}"))
        }
    }

    /// Builds a client whose session fetch is the given mock.
    fn client_with(
        token: &str,
        max_sessions: usize,
        session_duration: Duration,
        fetch: Arc<MockFetch>,
    ) -> Client {
        let fetcher: Box<SessionFetcher> = Box::new(move |_http: &reqwest::Client| {
            let mock = Arc::clone(&fetch);

            Box::pin(async move { mock.hit().await })
        });

        let options = ClientOptions {
            session_duration,
            max_sessions,
            ..ClientOptions::default()
        };

        Client::assemble(
            Arc::new(SecretString::from(token.to_owned())),
            &options,
            fetcher,
        )
        .expect("client options are valid")
    }

    /// Acquires a session, holds it for the given duration as if streaming a search, and
    /// releases it.
    async fn exercise(client: &Client, hold: Duration) -> Result<(), Error> {
        let lease = client.acquire().await?;
        tokio::time::sleep(hold).await;
        drop(lease);

        Ok(())
    }

    fn read_fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);

        String::from_utf8(std::fs::read(path).expect("could not read fixture"))
            .expect("fixture is not valid UTF-8")
    }

    #[tokio::test(start_paused = true)]
    async fn inrush_establishes_sessions_one_at_a_time() {
        let mock = MockFetch::new(0, Some(Duration::from_millis(5)));
        let client = client_with("token", 2, Duration::from_secs(60), Arc::clone(&mock));

        let hold = Duration::from_millis(5);
        let (a, b, c, d) = tokio::join!(
            exercise(&client, hold),
            exercise(&client, hold),
            exercise(&client, hold),
            exercise(&client, hold),
        );

        a.expect("first search succeeds");
        b.expect("second search succeeds");
        c.expect("third search succeeds");
        d.expect("fourth search succeeds");

        // The inrush of four requests grows the pool to its capacity of two, one session at
        // a time: exactly two fetches, never more than one in flight.
        assert_eq!(mock.calls(), 2);
        assert_eq!(mock.max_in_flight(), 1);
        assert_eq!(client.slot_count(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn sequential_requests_reuse_the_session() {
        let mock = MockFetch::new(0, Some(Duration::from_millis(5)));
        let client = client_with("token", 2, Duration::from_secs(60), Arc::clone(&mock));

        let hold = Duration::from_millis(1);
        exercise(&client, hold)
            .await
            .expect("first search succeeds");
        exercise(&client, hold)
            .await
            .expect("second search succeeds");
        exercise(&client, hold)
            .await
            .expect("third search succeeds");

        // The session from the first request is still valid, so no further fetches happen.
        assert_eq!(mock.calls(), 1);
        assert_eq!(client.slot_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn request_beyond_capacity_waits_for_a_session() {
        let mock = MockFetch::new(0, Some(Duration::from_millis(5)));
        let client = client_with("token", 1, Duration::from_secs(60), Arc::clone(&mock));

        let hold = Duration::from_millis(20);
        let (a, b) = tokio::join!(exercise(&client, hold), exercise(&client, hold));

        a.expect("first search succeeds");
        b.expect("second search succeeds");

        // The second request waits for the first to release the session and reuses it.
        assert_eq!(mock.calls(), 1);
        assert_eq!(client.slot_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_session_is_refreshed_with_a_new_nonce() {
        let mock = MockFetch::new(0, Some(Duration::from_millis(1)));
        let client = client_with("token", 1, Duration::from_millis(30), Arc::clone(&mock));

        exercise(&client, Duration::from_millis(1))
            .await
            .expect("first search succeeds");
        assert_eq!(mock.calls(), 1);

        // Outlive the session duration: the next request refreshes the session.
        tokio::time::sleep(Duration::from_millis(40)).await;
        exercise(&client, Duration::from_millis(1))
            .await
            .expect("second search succeeds");

        assert_eq!(mock.calls(), 2);
        assert_eq!(client.slot_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_refreshes_are_single_flight() {
        let mock = MockFetch::new(0, Some(Duration::from_millis(5)));
        let client = client_with("token", 1, Duration::from_millis(30), Arc::clone(&mock));

        exercise(&client, Duration::from_millis(1))
            .await
            .expect("first search succeeds");
        tokio::time::sleep(Duration::from_millis(40)).await;

        let hold = Duration::from_millis(10);
        let (a, b) = tokio::join!(exercise(&client, hold), exercise(&client, hold));

        a.expect("first search succeeds");
        b.expect("second search succeeds");

        // Both requests race on the one expired session: it is refreshed exactly once, with
        // no more than one fetch in flight; the other request waits and reuses the result.
        assert_eq!(mock.calls(), 2);
        assert_eq!(mock.max_in_flight(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_fetch_backs_off_exponentially() {
        let mock = MockFetch::new(2, Some(Duration::from_millis(1)));
        let client = client_with("token", 1, Duration::from_secs(60), Arc::clone(&mock));

        let first = client.acquire().await;
        assert!(matches!(first, Err(Error::Nonce)), "first fetch fails");

        // The second attempt waits out the first backoff (500 ms) and fails again.
        let second = client.acquire().await;
        assert!(matches!(second, Err(Error::Nonce)), "second fetch fails");

        // The third attempt waits out the doubled backoff (1s) and succeeds.
        let third = client.acquire().await;
        assert!(third.is_ok(), "third fetch succeeds");

        assert_eq!(mock.calls(), 3);
        assert_eq!(client.slot_count(), 1);

        // The success resets the backoff.
        let inner = client.slots.lock().unwrap();
        assert_eq!(inner.first().unwrap().inner.lock().unwrap().failures, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_establishment_does_not_grow_the_pool() {
        let mock = MockFetch::new(1, Some(Duration::from_millis(1)));
        let client = client_with("token", 2, Duration::from_secs(60), Arc::clone(&mock));

        let first = client.acquire().await;
        assert!(matches!(first, Err(Error::Nonce)), "first fetch fails");
        assert_eq!(client.slot_count(), 1, "the empty slot absorbs the retry");

        exercise(&client, Duration::from_millis(1))
            .await
            .expect("retry succeeds on the same slot");

        assert_eq!(mock.calls(), 2);
        assert_eq!(client.slot_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_caller_releases_the_session() {
        let mock = MockFetch::new(0, Some(Duration::from_millis(1)));
        let client = Arc::new(client_with(
            "token",
            1,
            Duration::from_secs(60),
            Arc::clone(&mock),
        ));

        let task = tokio::spawn({
            let client = Arc::clone(&client);

            async move {
                let lease = client.acquire().await.expect("session is established");
                // The lease is dropped when the task is cancelled at this await point.
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(lease);
            }
        });

        // Let the task establish the session and park on its hold.
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(mock.calls(), 1);

        task.abort();

        // The cancelled task released the session: the next request reuses it immediately.
        exercise(&client, Duration::from_millis(1))
            .await
            .expect("second search succeeds");
        assert_eq!(mock.calls(), 1, "the released session is reused");
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_stream_status_invalidates_the_session() {
        let mock = MockFetch::new(0, Some(Duration::from_millis(1)));
        let client = client_with("token", 1, Duration::from_secs(60), Arc::clone(&mock));

        let mut lease = client.acquire().await.expect("session is established");
        assert_eq!(mock.calls(), 1);

        // Kagi rejects the session's credentials (401/403): the lease is
        // invalidated the way `stream` does — the session is dropped and the
        // slot backs off like a failed fetch.
        lease.invalidate();
        drop(lease);

        // The rejection backed the slot off.
        let backed_off = {
            let inner = client.slots.lock().unwrap();

            inner
                .first()
                .unwrap()
                .inner
                .lock()
                .unwrap()
                .next_retry
                .is_some()
        };
        assert!(backed_off, "the rejection backs the slot off");

        // The next request cannot reuse the dead session: it re-establishes it
        // after the backoff.
        client.acquire().await.expect("second search succeeds");
        assert_eq!(mock.calls(), 2);

        // The success resets the backoff.
        let inner = client.slots.lock().unwrap();
        assert_eq!(inner.first().unwrap().inner.lock().unwrap().failures, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn other_stream_statuses_keep_the_session() {
        let mock = MockFetch::new(0, Some(Duration::from_millis(1)));
        let client = client_with("token", 1, Duration::from_secs(60), Arc::clone(&mock));

        let lease = client.acquire().await.expect("session is established");
        assert_eq!(mock.calls(), 1);

        // A rate limit or server error says nothing about the session: it is
        // parked back and reused.
        assert!(!invalidates_session(StatusCode::TOO_MANY_REQUESTS));
        assert!(!invalidates_session(StatusCode::INTERNAL_SERVER_ERROR));
        drop(lease);

        client.acquire().await.expect("second search succeeds");
        assert_eq!(mock.calls(), 1, "the parked session is reused");
    }

    #[test]
    fn backoff_doubles_and_is_bounded() {
        assert_eq!(backoff_after_failures(1), Duration::from_millis(500));
        assert_eq!(backoff_after_failures(2), Duration::from_secs(1));
        assert_eq!(backoff_after_failures(3), Duration::from_secs(2));
        assert_eq!(backoff_after_failures(5), Duration::from_secs(8));
        assert_eq!(backoff_after_failures(7), BACKOFF_MAX);
        assert_eq!(backoff_after_failures(100), BACKOFF_MAX);
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
            Client::with_token_and_options("token", &options),
            Err(Error::InvalidHeader(_))
        ));
    }
}
