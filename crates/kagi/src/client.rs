use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, HeaderValue, PRAGMA, REFERER, SET_COOKIE,
    UPGRADE_INSECURE_REQUESTS,
};
use reqwest::redirect::Policy;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::time::Instant;
use tracing::{debug, error};

use super::parse::{
    extract_nonce, parse_image_result_messages, parse_search_result_messages, parse_stream,
};
use super::privacy_pass::{PrivacyPassClient, PrivacyPassOptions};
use super::{ClientOptions, Error, ImageResult, RequestError, SearchResult};

/// Wraps `error` in a [`RequestError`], whose URL keeps no credential from the query string:
/// the login token is part of Kagi's query strings.
///
/// The failed request is logged once with its redacted URL and error type: the caller reports
/// the short rendering, so this is the only line that names the request.
#[expect(
    clippy::redundant_pub_crate,
    reason = "the client module is private, so pub(crate) states the effective reachability"
)]
pub(crate) fn log_request_error(error: reqwest::Error) -> RequestError {
    let error = RequestError::from(error);

    if let Some(url) = error.url() {
        debug!(
            url.full = %url,
            error.type = error.error_type(),
            exception.message = %error.full(),
            "request failed"
        );
    }

    error
}

/// The base duration of the exponential backoff between session fetch retries.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
/// The upper bound of the exponential backoff between session fetch retries.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The `Accept` header sent for document (navigation) requests.
const ACCEPT_DOCUMENT: HeaderValue =
    HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8");
/// The `Accept` header sent for server-sent event stream requests.
const ACCEPT_EVENT_STREAM: HeaderValue = HeaderValue::from_static("text/event-stream");

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

    /// Returns the nonce on the first call and `None` afterwards: Kagi only sends a document's
    /// `sse_nonce` on that document's initial stream connection. The client fetches the target
    /// search or images document when it establishes a session.
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
enum SlotCheckout {
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
/// At most [`ClientOptions::max_sessions`] searches run at once in either authentication mode;
/// additional searches wait for a slot. In session mode the client also keeps a pool of up to
/// that many persistent sessions. Each in-flight search uses its own session; an inrush of
/// requests grows the pool by establishing sessions one at a time, and requests beyond the pool
/// capacity wait for a session to free up. Sessions expire independently and are refreshed with
/// a new nonce after their own duration; a stream response rejecting the session's credentials
/// (401/403) drops the session so the next request re-establishes it.
///
/// With [`ClientOptions::privacy_pass`] set, searches redeem anonymous Privacy Pass tokens
/// instead and the session pool is not used; [`ClientOptions::max_sessions`] still bounds
/// in-flight searches.
pub struct Client {
    /// Kagi login token.
    token: Arc<SecretString>,
    /// The base URL of the Kagi service.
    base_url: reqwest::Url,
    /// The `Accept-Language` header sent with requests.
    language: HeaderValue,
    /// The maximum number of simultaneous searches.
    max_sessions: usize,
    /// Bounds the number of in-flight searches in either authentication mode.
    search_gate: Semaphore,
    /// The duration of a single session.
    session_duration: Duration,
    /// Privacy Pass authentication, when enabled.
    privacy_pass: Option<Arc<PrivacyPassClient>>,
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

    /// Constructs a client that authenticates searches with anonymous Privacy Pass tokens.
    ///
    /// The token is still the value of the `kagi_session` cookie from an authenticated browser
    /// session, but it is used only to authorize token issuance. Searches redeem tokens
    /// instead of sending the token or session cookies.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the default options are invalid or the Privacy Pass HTTP client
    /// cannot be built.
    pub fn with_privacy_pass(token: impl Into<SecretString>) -> Result<Client, Error> {
        let options = ClientOptions {
            privacy_pass: Some(PrivacyPassOptions::default()),
            ..ClientOptions::default()
        };

        Self::with_token_and_options(token, &options)
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
        let language = HeaderValue::from_str(&options.language)?;
        let user_agent = HeaderValue::from_str(&options.user_agent)?;
        let token = Arc::new(token.into());
        let max_sessions = options.max_sessions.max(1);
        let privacy_pass = match &options.privacy_pass {
            Some(privacy_pass) => Some(Arc::new(PrivacyPassClient::new(
                base_url.clone(),
                Arc::clone(&token),
                language.clone(),
                options.timeout,
                user_agent.clone(),
                privacy_pass,
            )?)),
            None => None,
        };

        Ok(Client {
            token,
            base_url,
            language,
            max_sessions,
            search_gate: Semaphore::new(max_sessions),
            session_duration: options.session_duration,
            privacy_pass,
            timeout: options.timeout,
            user_agent,
            slots: Mutex::new(Vec::new()),
            creation: AsyncMutex::new(()),
            check_in: Arc::new(Semaphore::new(0)),
        })
    }

    /// Returns the Privacy Pass client, or `None` when Privacy Pass mode is disabled.
    #[must_use]
    pub fn privacy_pass(&self) -> Option<&PrivacyPassClient> {
        self.privacy_pass.as_deref()
    }

    /// Returns the maximum number of simultaneous searches, as normalized from the options.
    #[must_use]
    pub const fn max_sessions(&self) -> usize {
        self.max_sessions
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
    async fn acquire(&self, endpoint: &str, query: &str) -> Result<SessionLease, Error> {
        loop {
            if let Some(checkout) = self.try_checkout() {
                return self.prepare(checkout, endpoint, query).await;
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
            let session = self.establish(&lease.slot, endpoint, query).await?;
            lease.session = Some(session);

            return Ok(lease);
        }
    }

    /// Completes a check-out: uses a valid session as-is, refreshes an expired one, or
    /// establishes a session on an empty slot.
    async fn prepare(
        &self,
        checkout: SlotCheckout,
        endpoint: &str,
        query: &str,
    ) -> Result<SessionLease, Error> {
        match checkout {
            SlotCheckout::Ready(slot, session) => Ok(self.lease(slot, Some(session))),
            SlotCheckout::Expired(slot, session) => {
                let mut lease = self.lease(slot, Some(session));
                let session = self.refresh(&lease.slot, endpoint, query).await?;
                lease.session = Some(session);

                Ok(lease)
            }
            SlotCheckout::Empty(slot) => {
                let mut lease = self.lease(slot, None);
                let session = self.establish(&lease.slot, endpoint, query).await?;
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
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the pool guard must stay held through the candidate check-out"
    )]
    fn try_checkout(&self) -> Option<SlotCheckout> {
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

                return Some(SlotCheckout::Ready(Arc::clone(slot), session));
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

            return Some(SlotCheckout::Expired(slot, session));
        }

        if let Some(slot) = empty {
            slot.inner
                .lock()
                .expect("the session lock is not poisoned")
                .check_out();

            return Some(SlotCheckout::Empty(slot));
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
            .map_err(|error| Error::BuildClient(log_request_error(error)))?;

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
    async fn fetch_session(
        &self,
        slot: &Slot,
        endpoint: &str,
        query: &str,
    ) -> Result<Session, Error> {
        match http_fetch_session(
            &slot.http,
            &self.base_url,
            &self.token,
            &self.language,
            endpoint,
            query,
        )
        .await
        {
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
    async fn refresh(&self, slot: &Slot, endpoint: &str, query: &str) -> Result<Session, Error> {
        self.wait_backoff(slot).await;
        self.fetch_session(slot, endpoint, query).await
    }

    /// Establishes a session for the given empty, checked-out slot.
    ///
    /// The creation gate is held across the fetch, so at most one creation fetch runs at any
    /// moment. The slot's backoff is waited out before the gate is taken, so retry waits
    /// never block other creations.
    async fn establish(&self, slot: &Slot, endpoint: &str, query: &str) -> Result<Session, Error> {
        self.wait_backoff(slot).await;

        let gate = self.creation.lock().await;
        let session = self.fetch_session(slot, endpoint, query).await;
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
        // Bound in-flight searches in both authentication modes: the session pool already
        // caps session-mode searches, but Privacy Pass mode has no pool of its own.
        let _permit = self
            .search_gate
            .acquire()
            .await
            .expect("the search gate is never closed");

        if let Some(privacy_pass) = &self.privacy_pass {
            return self.privacy_pass_body(privacy_pass, endpoint, query).await;
        }

        let mut lease = self.acquire(endpoint, query).await?;
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
        let res = req
            .send()
            .await
            .map_err(|error| Error::StreamRequest(log_request_error(error)))?;
        let res = match res.error_for_status() {
            Ok(res) => res,
            Err(error) => {
                let error = log_request_error(error);

                // A rejected session is dropped: the next request re-establishes
                // it, after the slot's backoff. Other statuses keep the session —
                // they say nothing about the session itself.
                if error.status().is_some_and(invalidates_session) {
                    lease.invalidate();
                }

                return Err(Error::StreamStatus(error));
            }
        };
        let body = res
            .text()
            .await
            .map_err(|error| Error::StreamRequestBody(log_request_error(error)))?;

        Ok(body)
    }

    /// Fetches a stream body through anonymous Privacy Pass tokens.
    ///
    /// A rejected token (401/403) drops the remaining batch and retries once with fresh tokens.
    async fn privacy_pass_body(
        &self,
        privacy_pass: &Arc<PrivacyPassClient>,
        endpoint: &str,
        query: &str,
    ) -> Result<String, Error> {
        let mut retried = false;

        loop {
            let result = self.privacy_pass_once(privacy_pass, endpoint, query).await;
            let retry = match &result {
                Err(Error::StreamStatus(error) | Error::RequestNonce(error)) => {
                    error.status().is_some_and(invalidates_session)
                }
                _ => false,
            };

            if !retry || retried {
                return result;
            }

            retried = true;
            privacy_pass
                .reset_and_replenish()
                .await
                .map_err(Error::PrivacyPass)?;
        }
    }

    /// Performs one Privacy Pass document and stream request pair.
    async fn privacy_pass_once(
        &self,
        privacy_pass: &Arc<PrivacyPassClient>,
        endpoint: &str,
        query: &str,
    ) -> Result<String, Error> {
        let nonce = self
            .privacy_pass_nonce(privacy_pass, endpoint, query)
            .await?;
        let token = privacy_pass
            .acquire_token()
            .await
            .map_err(Error::PrivacyPass)?;
        let url = url_with_query(
            &self.base_url,
            &format!("socket/{endpoint}"),
            &[("q", query)],
        );
        // The extension removes the referer and overrides the fetch site; both can link
        // otherwise separate searches.
        let req = browser_stream_request(
            privacy_pass.http(),
            &self.language,
            url,
            HeaderValue::from_static("none"),
        )
        .query(&[("nonce", nonce)]);
        let req = PrivacyPassClient::authorize(req, &token).map_err(Error::PrivacyPass)?;

        debug!(%endpoint, "connecting to stream with Privacy Pass");
        let res = req
            .send()
            .await
            .map_err(|error| Error::StreamRequest(log_request_error(error)))?;
        let res = res
            .error_for_status()
            .map_err(|error| Error::StreamStatus(log_request_error(error)))?;
        let body = res
            .text()
            .await
            .map_err(|error| Error::StreamRequestBody(log_request_error(error)))?;

        Ok(body)
    }

    /// Loads the target document with a Privacy Pass token and extracts its stream nonce.
    async fn privacy_pass_nonce(
        &self,
        privacy_pass: &Arc<PrivacyPassClient>,
        endpoint: &str,
        query: &str,
    ) -> Result<String, Error> {
        let token = privacy_pass
            .acquire_token()
            .await
            .map_err(Error::PrivacyPass)?;
        let url = url_with_query(&self.base_url, endpoint, &[("q", query)]);
        let req = browser_document_request(
            privacy_pass.http(),
            &self.language,
            url.as_str(),
            HeaderValue::from_static("none"),
            None,
        );
        let req = PrivacyPassClient::authorize(req, &token).map_err(Error::PrivacyPass)?;

        let res = req
            .send()
            .await
            .map_err(|error| Error::RequestNonce(log_request_error(error)))?;
        if let Err(error) = res.error_for_status_ref() {
            return Err(Error::RequestNonce(log_request_error(error)));
        }
        let body = res
            .text()
            .await
            .map_err(|error| Error::ReadNonce(log_request_error(error)))?;

        extract_nonce(&body).ok_or(Error::Nonce)
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
/// request is intentionally not logged — and then requests the target search or images
/// document to extract the nonce for the document's first stream request.
async fn http_fetch_session(
    http: &reqwest::Client,
    base_url: &reqwest::Url,
    token: &SecretString,
    language: &HeaderValue,
    endpoint: &str,
    query: &str,
) -> Result<String, Error> {
    let token_url = url_with_query(base_url, "search", &[("token", token.expose_secret())]);
    debug!("requesting session cookies");

    let res = document_request(
        http,
        token,
        language,
        token_url.as_str(),
        HeaderValue::from_static("none"),
        None,
    )
    .send()
    .await
    .map_err(|error| Error::RequestSession(log_request_error(error)))?;
    if let Err(error) = res.error_for_status_ref() {
        return Err(Error::RequestSession(log_request_error(error)));
    }
    if !sets_session_cookie(res.headers()) {
        error!("the response does not include a kagi_session cookie!");
        return Err(Error::SessionCookies);
    }

    // Mirror the browser: the nonce is the target document's trace ID, not the landing page's.
    debug!(%endpoint, "requesting document nonce");
    let document_url = url_with_query(base_url, endpoint, &[("q", query)]);
    let res = document_request(
        http,
        token,
        language,
        document_url.as_str(),
        HeaderValue::from_static("same-origin"),
        Some(base_url.as_str()),
    )
    .send()
    .await
    .map_err(|error| Error::RequestNonce(log_request_error(error)))?;
    if let Err(error) = res.error_for_status_ref() {
        return Err(Error::RequestNonce(log_request_error(error)));
    }
    let body = res
        .text()
        .await
        .map_err(|error| Error::ReadNonce(log_request_error(error)))?;

    extract_nonce(&body).ok_or(Error::Nonce)
}

/// Returns whether the response sets the `kagi_session` cookie that authenticates the session.
///
/// Kagi may set other cookies (such as `_kagi_search_`) on an unauthenticated response, so the
/// presence of any `Set-Cookie` header is not enough to tell that the token was accepted.
fn sets_session_cookie(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|cookie| cookie.trim_start().starts_with("kagi_session="))
        })
}

/// Attaches the session authorization header to the request, mirroring the browser which
/// sends it with every request.
fn authorize_session(
    request: reqwest::RequestBuilder,
    token: &SecretString,
) -> reqwest::RequestBuilder {
    request.header("X-Kagi-Authorization", token.expose_secret())
}

/// Builds a request shaped like a browser document navigation.
///
/// `site` is the navigation's `Sec-Fetch-Site` value, and `referer` is attached when the
/// navigation is reached from another page. The login-token request is a direct navigation
/// (`none`, no referer) so the token cannot leak through a `Referer` header; the subsequent
/// search or images document is a same-origin navigation from the site root.
fn document_request(
    http: &reqwest::Client,
    token: &SecretString,
    language: &HeaderValue,
    url: &str,
    site: HeaderValue,
    referer: Option<&str>,
) -> reqwest::RequestBuilder {
    authorize_session(
        browser_document_request(http, language, url, site, referer),
        token,
    )
}

/// Builds a request shaped like a browser document navigation without authentication headers.
fn browser_document_request(
    http: &reqwest::Client,
    language: &HeaderValue,
    url: &str,
    site: HeaderValue,
    referer: Option<&str>,
) -> reqwest::RequestBuilder {
    let request = http
        .get(url)
        .header(ACCEPT, ACCEPT_DOCUMENT)
        .header(ACCEPT_LANGUAGE, language.clone())
        .header("Sec-Fetch-Dest", HeaderValue::from_static("document"))
        .header("Sec-Fetch-Mode", HeaderValue::from_static("navigate"))
        .header("Sec-Fetch-Site", site)
        .header("Sec-Fetch-User", HeaderValue::from_static("?1"))
        .header(UPGRADE_INSECURE_REQUESTS, HeaderValue::from_static("1"))
        .header("Priority", HeaderValue::from_static("u=0, i"));

    match referer {
        Some(referer) => request.header(REFERER, referer),
        None => request,
    }
}

/// Builds a request shaped like a browser `EventSource` connection.
fn stream_request(
    http: &reqwest::Client,
    token: &SecretString,
    language: &HeaderValue,
    url: reqwest::Url,
) -> reqwest::RequestBuilder {
    authorize_session(
        browser_stream_request(http, language, url, HeaderValue::from_static("same-origin")),
        token,
    )
}

/// Builds an `EventSource`-shaped request without authentication headers.
///
/// `site` is the request's `Sec-Fetch-Site` value: `same-origin` for the session path and
/// `none` for Privacy Pass redemption.
fn browser_stream_request(
    http: &reqwest::Client,
    language: &HeaderValue,
    url: reqwest::Url,
    site: HeaderValue,
) -> reqwest::RequestBuilder {
    http.get(url)
        .header(ACCEPT, ACCEPT_EVENT_STREAM)
        .header(ACCEPT_LANGUAGE, language.clone())
        .header("Sec-Fetch-Dest", HeaderValue::from_static("empty"))
        .header("Sec-Fetch-Mode", HeaderValue::from_static("cors"))
        .header("Sec-Fetch-Site", site)
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

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

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
