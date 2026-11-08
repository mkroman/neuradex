use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

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

use super::{ClientOptions, Error, ImageResult, SearchResult};

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
    tag: String,
    /// The flexible payload. Using `Value` allows this struct to handle
    /// diverse message types (HTML strings, objects, or nulls) without breaking.
    payload: Option<Value>,
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

/// The outcome of checking a slot out of the pool: a parked session (valid or expired) or an
/// empty slot that must establish one.
enum CheckOut {
    /// A parked session that is still valid.
    Ready(Arc<Slot>, Session),
    /// A parked session that has expired and needs a refresh.
    Expired(Arc<Slot>, Session),
    /// A slot without a session, waiting to be established.
    Empty(Arc<Slot>),
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
    /// The pool capacity; bounds the accumulated check-in permits.
    max_sessions: usize,
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
            let mut inner = self
                .slot
                .inner
                .lock()
                .expect("the session lock is not poisoned");
            inner.busy = false;
            inner.session = self.session.take();
        }

        // One permit per check-in: permits accumulate when no request is waiting yet, so a
        // burst of check-ins is never collapsed into a single wakeup. At most `max_sessions`
        // slots can be checked in concurrently, so more permits would only be stale.
        if self.check_in.available_permits() < self.max_sessions {
            self.check_in.add_permits(1);
        }
    }
}

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
    /// The base URL of the Kagi service.
    base_url: reqwest::Url,
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
}

impl Client {
    /// Constructs a new [`Client`] for searching with Kagi using the given session token and
    /// default options.
    ///
    /// The token is the value of the `kagi_session` cookie from an authenticated browser session.
    ///
    /// # Panics
    ///
    /// Panics if the default options are invalid; they are compile-time constants.
    pub fn with_token(token: impl Into<SecretString>) -> Client {
        Self::with_token_and_options(token, &ClientOptions::default())
            .expect("the default client options are valid")
    }

    /// Constructs a new [`Client`] for searching with Kagi using the given session token and
    /// options.
    ///
    /// The token is the value of the `kagi_session` cookie from an authenticated browser session.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidHeader`] if a configured header value is invalid, or
    /// [`Error::InvalidBaseUrl`] if [`ClientOptions::base_url`] is not a usable HTTP(S) URL.
    pub fn with_token_and_options(
        token: impl Into<SecretString>,
        options: &ClientOptions,
    ) -> Result<Client, Error> {
        let base_url = parse_base_url(&options.base_url)?;

        Ok(Client {
            token: Arc::new(token.into()),
            base_url,
            language: HeaderValue::from_str(&options.language)?,
            max_sessions: options.max_sessions.max(1),
            session_duration: options.session_duration,
            timeout: options.timeout,
            user_agent: HeaderValue::from_str(&options.user_agent)?,
            slots: Mutex::new(Vec::new()),
            creation: AsyncMutex::new(()),
            check_in: Arc::new(Semaphore::new(0)),
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
            if let Some(checkout) = self.try_check_out() {
                return self.prepare(checkout).await;
            }

            // No parked session: grow the pool if there is room, one session at a time. While
            // queued on the creation gate, a session may have been checked back in or another
            // request may have grown the pool — re-scan before adding a slot.
            let slot = if self.slot_count() < self.max_sessions {
                let gate = self.creation.lock().await;

                let (idle, count) = self.pool_state();
                if idle || count >= self.max_sessions {
                    drop(gate);
                    continue;
                }

                let slot = self.add_slot()?;
                drop(gate);

                Some(slot)
            } else {
                None
            };

            let Some(slot) = slot else {
                // The pool is at capacity with every slot checked out: wait for a check-in.
                // Each check-in adds one permit; the permit is consumed rather than returned so
                // the re-scan below decides progress and the next check-in produces the next
                // wakeup.
                self.check_in
                    .acquire()
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

    /// Completes a check-out: uses a valid session as-is, refreshes an expired one, or
    /// establishes a session on an empty slot.
    async fn prepare(&self, checkout: CheckOut) -> Result<SessionLease, Error> {
        match checkout {
            CheckOut::Ready(slot, session) => Ok(self.lease(slot, Some(session))),
            CheckOut::Expired(slot, session) => {
                let mut lease = self.lease(slot, Some(session));
                let session = self.refresh(&lease.slot).await?;
                lease.session = Some(session);

                Ok(lease)
            }
            CheckOut::Empty(slot) => {
                let mut lease = self.lease(slot, None);
                let session = self.establish(&lease.slot).await?;
                lease.session = Some(session);

                Ok(lease)
            }
        }
    }

    /// Wraps a checked-out slot and session into a lease.
    fn lease(&self, slot: Arc<Slot>, session: Option<Session>) -> SessionLease {
        SessionLease {
            slot,
            session,
            check_in: Arc::clone(&self.check_in),
            max_sessions: self.max_sessions,
        }
    }

    /// Checks out the best parked slot in a single pass: a valid session, then an expired one,
    /// then an empty slot.
    // The pool guard must stay held through the candidate check-out below: dropping it after
    // the scan would let another request take the slot we picked.
    #[allow(clippy::significant_drop_tightening)]
    fn try_check_out(&self) -> Option<CheckOut> {
        let slots = self.slots.lock().expect("the session lock is not poisoned");
        let mut expired = None;
        let mut empty = None;

        for slot in slots.iter() {
            let mut inner = slot.inner.lock().expect("the session lock is not poisoned");

            if inner.busy {
                continue;
            }

            if inner
                .session
                .as_ref()
                .is_some_and(|session| session.is_valid(self.session_duration))
            {
                let session = inner.check_out().expect("a valid session was parked");
                drop(inner);

                return Some(CheckOut::Ready(Arc::clone(slot), session));
            }

            if inner.session.is_some() {
                if expired.is_none() {
                    expired = Some(Arc::clone(slot));
                }
            } else if empty.is_none() {
                empty = Some(Arc::clone(slot));
            }
        }

        if let Some(slot) = expired {
            let mut inner = slot.inner.lock().expect("the session lock is not poisoned");
            let session = inner.check_out().expect("an expired session was parked");
            drop(inner);

            return Some(CheckOut::Expired(slot, session));
        }

        if let Some(slot) = empty {
            slot.inner
                .lock()
                .expect("the session lock is not poisoned")
                .check_out();

            return Some(CheckOut::Empty(slot));
        }

        None
    }

    /// Returns whether any slot is parked and the number of slots in the pool.
    fn pool_state(&self) -> (bool, usize) {
        let slots = self.slots.lock().expect("the session lock is not poisoned");
        let idle = slots.iter().any(|slot| {
            !slot
                .inner
                .lock()
                .expect("the session lock is not poisoned")
                .busy
        });

        (idle, slots.len())
    }

    /// Returns the number of slots in the pool.
    fn slot_count(&self) -> usize {
        self.slots
            .lock()
            .expect("the session lock is not poisoned")
            .len()
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

        self.slots
            .lock()
            .expect("the session lock is not poisoned")
            .push(Arc::clone(&slot));

        Ok(slot)
    }

    /// Waits out the slot's remaining backoff, if one is scheduled.
    async fn wait_backoff(&self, slot: &Slot) {
        let next_retry = slot
            .inner
            .lock()
            .expect("the session lock is not poisoned")
            .next_retry;

        if let Some(next_retry) = next_retry
            && next_retry > Instant::now()
        {
            tokio::time::sleep_until(next_retry).await;
        }
    }

    /// Records a failed fetch on the slot: its backoff doubles, bounded.
    fn record_failure(slot: &Slot) {
        let mut inner = slot.inner.lock().expect("the session lock is not poisoned");
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
        match http_fetch_session(&slot.http, &self.base_url, &self.token, &self.language).await {
            Ok(nonce) => {
                {
                    let mut inner = slot.inner.lock().expect("the session lock is not poisoned");
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
    /// raw response body.
    ///
    /// The session is checked out for the duration of the stream request and returned
    /// afterwards — or immediately, if the caller's future is dropped mid-request.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if a session could not be established, the stream request could not
    /// be sent, or the response could not be read.
    async fn stream_body(&self, endpoint: &str, query: &str) -> Result<String, Error> {
        let mut lease = self.acquire().await?;
        let nonce = lease.take_nonce();

        let url = url_with_query(
            &self.base_url,
            &format!("socket/{endpoint}"),
            &[("q", query)],
        );
        let referer = url_with_query(&self.base_url, endpoint, &[("q", query)]);
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

        Ok(body)
    }

    /// Searches Kagi with the given query and returns the parsed search results.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if a session could not be established, the stream request could not
    /// be sent, the response could not be read, or the response could not be parsed.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>, Error> {
        let body = self.stream_body("search", query).await?;

        tokio::task::spawn_blocking(move || {
            let messages = parse_stream(&body);

            parse_search_result_messages(&messages)
        })
        .await
        .map_err(Error::ParseTask)
    }

    /// Searches Kagi images with the given query and returns the parsed image results.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if a session could not be established, the stream request could not
    /// be sent, the response could not be read, or the response could not be parsed.
    pub async fn images(&self, query: &str) -> Result<Vec<ImageResult>, Error> {
        let body = self.stream_body("images", query).await?;

        tokio::task::spawn_blocking(move || {
            let messages = parse_stream(&body);

            parse_image_result_messages(&messages)
        })
        .await
        .map_err(Error::ParseTask)
    }
}

/// Fetches a fresh session with the given session HTTP client: issues a request with the login
/// token to receive the session cookies — the token is part of the query string, so the
/// request is intentionally not logged — and then requests the main page to extract the nonce
/// for the session's first stream request.
async fn http_fetch_session(
    http: &reqwest::Client,
    base_url: &reqwest::Url,
    token: &SecretString,
    language: &HeaderValue,
) -> Result<String, Error> {
    let token_url = url_with_query(base_url, "search", &[("token", token.expose_secret())]);
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
    let res = document_request(http, token, language, base_url.as_str())
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

    BACKOFF_BASE.saturating_mul(1 << exponent).min(BACKOFF_MAX)
}

/// Returns whether the stream response status invalidates the session: the
/// session's credentials were rejected, so it must be re-established with fresh
/// cookies and a new nonce. Any other status says nothing about the session
/// itself — rate limits and server errors keep it.
fn invalidates_session(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

/// Parses and normalizes the configured base URL.
///
/// The base must be an HTTP(S) URL without a query or fragment. A path prefix is preserved
/// and treated as a directory: a trailing slash is appended so relative request paths join
/// beneath it.
///
/// # Errors
///
/// Returns [`Error::InvalidBaseUrl`] if `raw` is not a usable HTTP(S) base URL.
fn parse_base_url(raw: &str) -> Result<reqwest::Url, Error> {
    let mut base =
        reqwest::Url::parse(raw).map_err(|error| Error::InvalidBaseUrl(error.to_string()))?;

    if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() {
        return Err(Error::InvalidBaseUrl(format!(
            "expected an http or https URL, got {raw:?}"
        )));
    }

    if base.query().is_some() || base.fragment().is_some() {
        return Err(Error::InvalidBaseUrl(format!(
            "expected a base URL without a query or fragment, got {raw:?}"
        )));
    }

    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }

    Ok(base)
}

/// Builds a URL beneath `base` with the given query parameters, percent-encoding as needed.
///
/// `path` is relative to the base's path; callers pass request paths without a leading slash.
///
/// # Panics
///
/// Panics if a static request path cannot be joined to the normalized base URL; the paths are
/// compile-time constants, so this cannot happen.
fn url_with_query(base: &reqwest::Url, path: &str, params: &[(&str, &str)]) -> reqwest::Url {
    let mut url = base
        .join(path)
        .expect("the normalized base url joins request paths");

    url.query_pairs_mut().extend_pairs(params);

    url
}

// Extracts the `window.sse_nonce` value from the raw HTML content.
fn extract_nonce(html: &str) -> Option<String> {
    const MARKER: &str = "window.sse_nonce";

    let mut rest = html;

    while let Some(index) = rest.find(MARKER) {
        rest = &rest[index + MARKER.len()..];

        let Some(after) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some((nonce, _)) = quoted_string_literal(after.trim_start()) else {
            continue;
        };

        if !nonce.is_empty() {
            return Some(nonce.to_owned());
        }
    }

    None
}

/// Splits a leading double-quoted string literal from `input`.
///
/// Returns the literal's contents without the surrounding quotes and the input following the
/// closing quote. The literal ends at the first unescaped `"`, so backslash escapes such as
/// `\"` are skipped. Returns `None` if `input` does not start with a quote or the literal is
/// unterminated.
fn quoted_string_literal(input: &str) -> Option<(&str, &str)> {
    let body = input.strip_prefix('"')?;
    let mut escaped = false;

    for (index, character) in body.char_indices() {
        match (escaped, character) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '"') => return Some((&body[..index], &body[index + 1..])),
            _ => {}
        }
    }

    None
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
        .filter_map(|json| {
            serde_json::from_str::<Vec<KagiMessage>>(json)
                .inspect_err(|error| {
                    debug!(
                        event = json,
                        exception.message = %error,
                        "dropping an unparseable Kagi SSE event"
                    );
                })
                .ok()
        })
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
        .split_terminator("\0\n")
        .filter_map(|chunk| {
            // Split wire format: "tag:json_data"
            let Some((wire_tag, json_str)) = chunk.split_once(':') else {
                debug!(chunk, "dropping a Kagi chunk without a wire tag");
                return None;
            };
            // Parse JSON body
            let mut message: KagiMessage = match serde_json::from_str(json_str) {
                Ok(message) => message,
                Err(error) => {
                    debug!(
                        wire_tag,
                        exception.message = %error,
                        "dropping an unparseable Kagi chunk"
                    );
                    return None;
                }
            };
            // Normalize Tag: If the JSON body didn't have a tag, use the wire tag.
            if message.tag.is_empty() {
                message.tag = wire_tag.to_string();
            }

            Some(message)
        })
        .collect()
}

/// Returns the `content` string of a message's payload, if it has one.
fn payload_content(message: &KagiMessage) -> Option<&str> {
    message.payload.as_ref()?.get("content")?.as_str()
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
        let results = match serde_json::from_str::<SearchResultsJson>(payload) {
            Ok(results) => results,
            Err(error) => {
                debug!(
                    exception.message = %error,
                    "falling back to HTML results, structured payload is malformed"
                );
                continue;
            }
        };

        if !results.items.is_empty() {
            return results
                .items
                .into_iter()
                .map(|item| SearchResult {
                    title: htmlize::unescape(item.title).into_owned(),
                    url: item.url,
                    description: htmlize::unescape(item.snippet).into_owned(),
                })
                .collect();
        }
    }

    // Fall back to parsing the HTML fragments of `search` messages.
    let mut result: Vec<SearchResult> = Vec::new();

    for content in messages
        .iter()
        .filter(|message| message.tag == "search")
        .filter_map(payload_content)
    {
        result.extend(parse_search_results_html(content));
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

/// The pre-compiled result selectors, initialized on first use.
static SELECTORS: LazyLock<Selectors> = LazyLock::new(Selectors::new);

fn parse_search_results_html(html: &str) -> Vec<SearchResult> {
    let fragment = Html::parse_fragment(html);
    let selectors = &SELECTORS;

    let search_results = fragment.select(&selectors.search_result);

    let mut results = Vec::new();

    for result_div in search_results {
        let title = result_div.select(&selectors.title_link).next();
        let description = result_div.select(&selectors.description).next();

        if let (Some(title), Some(description)) = (title, description) {
            let url = title.attr("href").unwrap_or("").to_string();

            results.push(SearchResult {
                title: collect_trimmed_text(&title),
                url,
                description: extract_topmost_text(&description),
            });
        }
    }

    results
}

fn parse_image_result_messages(messages: &[KagiMessage]) -> Vec<ImageResult> {
    let mut result: Vec<ImageResult> = Vec::new();

    for content in messages
        .iter()
        .filter(|message| message.tag == "images")
        .filter_map(payload_content)
    {
        result.extend(parse_image_results_html(content));
    }

    result
}

fn parse_image_results_html(html: &str) -> Vec<ImageResult> {
    let fragment = Html::parse_fragment(html);
    let selectors = &SELECTORS;

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
            title: htmlize::unescape(title).into_owned(),
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

/// Collects an element's text nodes into one allocation, trimmed.
fn collect_trimmed_text(elem: &ElementRef<'_>) -> String {
    let mut text = String::with_capacity(elem.text().map(str::len).sum());

    for node in elem.text() {
        text.push_str(node);
    }

    let start = text.len() - text.trim_start().len();
    let end = start + text[start..].trim_end().len();
    text.truncate(end);
    text.drain(..start);

    text
}

fn extract_topmost_text(elem: &ElementRef<'_>) -> String {
    let mut extracted = String::new();

    for node in elem.children() {
        let Node::Text(text_node) = node.value() else {
            continue;
        };
        let text = text_node.trim();

        if text.is_empty() {
            continue;
        }

        if !extracted.is_empty() {
            extracted.push(' ');
        }

        extracted.push_str(text);
    }

    extracted
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use test_support::kagi::read_fixture;

    use super::*;

    #[test]
    fn parses_the_legacy_stream_format() {
        let messages = parse_stream(&read_fixture("search_stream.bin"));

        assert_eq!(messages.len(), 8);
    }

    #[test]
    fn parses_structured_search_results() {
        let messages = parse_stream(&read_fixture("search_stream_sse.bin"));

        assert!(
            messages
                .iter()
                .any(|message| message.tag == "search_results_json")
        );
        assert!(messages.iter().any(|message| message.tag == "search"));
        assert!(messages.iter().any(|message| message.tag == "search.info"));

        let results = parse_search_result_messages(&messages);

        assert_eq!(results.len(), 5);
        assert_eq!(results[0].title, "Hello, world - Wikipedia");
        assert_eq!(results[0].url, "https://en.wikipedia.org/wiki/Hello,_world");
        // Snippets arrive HTML-entity encoded and must be decoded.
        assert!(results[0].description.contains('"'));
        assert!(!results[0].description.contains("&quot;"));
    }

    #[test]
    fn unescapes_structured_search_result_titles_and_snippets() {
        let payload = r#"{"items":[{"title":"Fish &amp; Chips","url":"https://example.com/fish","snippet":"A &quot;classic&quot; dish &amp; more"}]}"#;
        let messages = vec![KagiMessage {
            tag: "search_results_json".to_string(),
            payload: Some(Value::String(payload.to_string())),
        }];

        let results = parse_search_result_messages(&messages);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Fish & Chips");
        assert_eq!(results[0].description, "A \"classic\" dish & more");
    }

    #[test]
    fn falls_back_to_html_search_results() {
        let messages = parse_stream(&read_fixture("search_stream.bin"));
        let results = parse_search_result_messages(&messages);

        assert_eq!(results.len(), 19);
        assert_eq!(
            results[0].title,
            "Vitamin D - Health Professional Fact Sheet"
        );
        assert_eq!(
            results[0].description,
            "Vitamin D (also referred to as calciferol) is a fat-soluble vitamin that is naturally present in a few foods, added to others, and available as a dietary ..."
        );
        assert_eq!(
            results[0].url,
            "https://ods.od.nih.gov/factsheets/VitaminD-HealthProfessional/"
        );
    }

    #[test]
    fn parses_image_results() {
        let messages = parse_stream(&read_fixture("images_stream_sse.bin"));
        let results = parse_image_result_messages(&messages);

        assert_eq!(results.len(), 5);

        let result = &results[0];
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
    fn extracts_the_nonce_from_the_landing_page() {
        let nonce = extract_nonce(&read_fixture("landing.html")).expect("could not extract nonce");

        assert_eq!(nonce, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn extracts_nonces_with_surrounding_whitespace() {
        assert_eq!(
            extract_nonce("window.sse_nonce =\n\t\"abc\""),
            Some("abc".to_owned())
        );
        assert_eq!(extract_nonce("window.sse_nonce = \"\""), None);
        // A malformed marker does not hide a later valid one.
        assert_eq!(
            extract_nonce("window.sse_nonce_nope = \"x\"; window.sse_nonce = \"y\""),
            Some("y".to_owned())
        );
    }

    #[test]
    fn extracts_nonces_with_escaped_quotes() {
        // The scan continues past a backslash-escaped quote instead of truncating there.
        assert_eq!(
            extract_nonce(r#"window.sse_nonce = "abc\"def""#),
            Some(r#"abc\"def"#.to_owned())
        );
        assert_eq!(
            extract_nonce(r#"window.sse_nonce = "abc\";def";"#),
            Some(r#"abc\";def"#.to_owned())
        );
    }

    #[test]
    fn skips_malformed_and_empty_nonces() {
        // A marker without an assignment does not hide a later valid one.
        assert_eq!(
            extract_nonce("window.sse_nonce = 42; window.sse_nonce = \"later\""),
            Some("later".to_owned())
        );
        // An empty nonce is ignored in favor of a later assignment.
        assert_eq!(
            extract_nonce("window.sse_nonce = \"\"; window.sse_nonce = \"later\""),
            Some("later".to_owned())
        );
        // An unterminated literal and a page without the marker yield no nonce.
        assert_eq!(extract_nonce("window.sse_nonce = \"unterminated"), None);
        assert_eq!(extract_nonce("no nonce here"), None);
    }

    #[test]
    fn extracts_nonces_with_whitespace_around_the_assignment() {
        assert_eq!(
            extract_nonce("window.sse_nonce\t=\n  \"abc\""),
            Some("abc".to_owned())
        );
    }

    #[test]
    fn takes_the_session_nonce_only_once() {
        let mut session = Session {
            nonce: Some("nonce".to_string()),
            created_at: Instant::now(),
        };

        assert_eq!(session.take_nonce().as_deref(), Some("nonce"));
        assert_eq!(session.take_nonce(), None);
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
    fn rejects_an_invalid_language() {
        let options = ClientOptions {
            language: "invalid\nlanguage".to_string(),
            ..ClientOptions::default()
        };

        assert_matches!(
            Client::with_token_and_options("token", &options).err(),
            Some(Error::InvalidHeader(_))
        );
    }

    #[test]
    fn rejects_an_invalid_base_url() {
        let options = ClientOptions {
            base_url: "not a url".to_string(),
            ..ClientOptions::default()
        };

        assert_matches!(
            Client::with_token_and_options("token", &options).err(),
            Some(Error::InvalidBaseUrl(_))
        );
    }

    #[test]
    fn rejects_unusable_base_urls() {
        for raw in [
            "localhost:8080",
            "mailto:someone@example.com",
            "file:///tmp/neuradex",
            "https://example.com/kagi?token=secret",
            "https://example.com/kagi#fragment",
        ] {
            let options = ClientOptions {
                base_url: raw.to_string(),
                ..ClientOptions::default()
            };

            assert_matches!(
                Client::with_token_and_options("token", &options).err(),
                Some(Error::InvalidBaseUrl(_)),
                "expected {raw} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_https_and_preserves_a_base_path_prefix() {
        let options = ClientOptions {
            base_url: "https://example.com/kagi".to_string(),
            ..ClientOptions::default()
        };
        let client = Client::with_token_and_options("token", &options).expect("valid options");

        assert_eq!(client.base_url.as_str(), "https://example.com/kagi/");
        assert_eq!(
            url_with_query(&client.base_url, "search", &[("token", "secret")]).as_str(),
            "https://example.com/kagi/search?token=secret"
        );
        assert_eq!(
            url_with_query(&client.base_url, "socket/search", &[("q", "hello world")]).as_str(),
            "https://example.com/kagi/socket/search?q=hello+world"
        );
    }

    #[test]
    fn check_ins_do_not_accumulate_permits_beyond_capacity() {
        let slot = Arc::new(Slot {
            inner: Mutex::new(SlotInner {
                session: None,
                busy: false,
                failures: 0,
                next_retry: None,
            }),
            http: reqwest::Client::new(),
        });
        let check_in = Arc::new(Semaphore::new(0));

        for _ in 0..10 {
            drop(SessionLease {
                slot: Arc::clone(&slot),
                session: None,
                check_in: Arc::clone(&check_in),
                max_sessions: 2,
            });
        }

        assert_eq!(
            check_in.available_permits(),
            2,
            "stale check-in permits are capped at the pool capacity"
        );
    }
}
