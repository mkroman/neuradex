//! A small pool of threads driving `!Send` head-parser futures.
//!
//! html5ever's tokenizer is `!Send`, so a parser cannot live in a task on the worker runtime.
//! Parsing each response on `spawn_blocking` would park one OS thread per response for as long
//! as the upstream keeps the body open. With many slow responses, the blocking pool's thread cap
//! turns into head-of-line blocking for every other request: the pool is shared process-wide.
//!
//! Instead, each parser waits for the next body chunk as a `!Send` future on one of a few
//! [`LocalSet`] threads. Waiting is async, so a thread drives many parsers while their upstreams
//! are slow; only the actual tokenization occupies the thread. The pool is created on first use
//! and lives for the rest of the process.
//!
//! The pool size follows `available_parallelism`: one thread per eight CPUs, clamped to 2
//! through 4.

#![expect(
    clippy::future_not_send,
    reason = "the parser futures are deliberately !Send; they run on a LocalSet"
)]

use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use axum::body::Bytes;
use futures::FutureExt;
use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};
use tokio::task::LocalSet;

use crate::api::v1::error::ApiError;
use crate::metadata::{HeadParser, PageMetadata};

/// The maximum size of a response consumed while extracting document head metadata.
pub(super) const HEAD_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The outcome of streaming a response through the head parser.
pub(super) struct StreamedBody {
    /// The metadata captured from the document head.
    pub metadata: PageMetadata,
    /// The body bytes read, when collecting the full body.
    pub body: Option<Vec<u8>>,
    /// The number of body bytes read (decompressed).
    pub bytes_read: u64,
    /// Whether the read was cut short by a size limit.
    pub truncated: bool,
}

/// How many chunks a parser processes before it yields to the other parsers on its thread.
///
/// A yield costs a scheduler round trip, so it is amortized over a batch: a typical head fits in
/// one batch, while a full body still yields regularly.
const CHUNKS_PER_YIELD: u32 = 32;

/// A parser job submitted to the pool.
struct Job {
    /// The response chunks, in order.
    chunks: mpsc::Receiver<Result<Bytes, wreq::Error>>,
    /// Whether the full body should be collected.
    collect_body: bool,
    /// The size limit for the read.
    max_bytes: u64,
    /// The capacity hint for the collected body.
    body_capacity: usize,
    /// The channel that receives the parsed result.
    result: oneshot::Sender<Result<StreamedBody, ApiError>>,
    /// Resolves when the caller no longer needs the result.
    cancelled: oneshot::Receiver<()>,
}

/// The process-wide parser pool.
struct Pool {
    /// One job channel per parser thread.
    senders: Vec<mpsc::UnboundedSender<Job>>,
    /// The round-robin start index.
    next: AtomicUsize,
}

impl Pool {
    /// Returns the global pool, starting it on first use.
    fn global() -> &'static Self {
        static POOL: OnceLock<Pool> = OnceLock::new();

        POOL.get_or_init(Self::start)
    }

    /// Starts one `LocalSet` thread per parser thread.
    fn start() -> Self {
        let threads = parser_threads();
        let mut senders = Vec::with_capacity(threads);

        for index in 0..threads {
            let (sender, mut receiver) = mpsc::unbounded_channel::<Job>();
            let runtime = Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the parser runtime builds");
            let spawned = thread::Builder::new()
                .name(format!("neuradex-parser-{index}"))
                .spawn(move || {
                    let local = LocalSet::new();
                    local.spawn_local(async move {
                        while let Some(job) = receiver.recv().await {
                            tokio::task::spawn_local(run_job(job));
                        }
                    });
                    runtime.block_on(local);
                });

            if spawned.is_err() {
                // Keep the workers that did start. An empty pool makes `submit` report an
                // internal error instead of hanging the caller.
                break;
            }

            senders.push(sender);
        }

        Self {
            senders,
            next: AtomicUsize::new(0),
        }
    }

    /// Submits `job` to the next thread, falling back to the others if one is gone.
    fn submit(&self, job: Job) {
        let mut job = job;

        for _ in 0..self.senders.len() {
            let index = self.next.fetch_add(1, Ordering::Relaxed) % self.senders.len();

            match self.senders[index].send(job) {
                Ok(()) => return,
                Err(error) => job = error.0,
            }
        }

        let _ = job.result.send(Err(ApiError::internal_message(
            "the parser pool is unavailable",
        )));
    }
}

/// Returns the number of parser threads.
fn parser_threads() -> usize {
    thread::available_parallelism()
        .map_or(2, |parallelism| parallelism.get().div_ceil(8).clamp(2, 4))
}

/// Parses `chunks` on the shared pool.
///
/// The future completes after the last chunk, the first error, or the head boundary in head-only
/// mode. Dropping it cancels the queued parser job.
///
/// # Errors
///
/// Returns an error when a chunk failed to read, the parser task could not run, or the parser
/// panicked.
pub(super) async fn parse(
    chunks: mpsc::Receiver<Result<Bytes, wreq::Error>>,
    collect_body: bool,
    max_bytes: u64,
    body_capacity: usize,
) -> Result<StreamedBody, ApiError> {
    let (result, result_rx) = oneshot::channel();
    let (cancel_tx, cancelled) = oneshot::channel();
    let job = Job {
        chunks,
        collect_body,
        max_bytes,
        body_capacity,
        result,
        cancelled,
    };

    Pool::global().submit(job);
    let parsed = result_rx.await;
    drop(cancel_tx);

    parsed.map_err(|_| ApiError::internal_message("the parser task was dropped"))?
}

/// Runs one job to completion and reports its result.
async fn run_job(job: Job) {
    let Job {
        chunks,
        collect_body,
        max_bytes,
        body_capacity,
        result,
        cancelled,
    } = job;
    let parsed = AssertUnwindSafe(parse_chunks(
        chunks,
        collect_body,
        max_bytes,
        body_capacity,
        cancelled,
    ))
    .catch_unwind()
    .await;
    let parsed =
        parsed.unwrap_or_else(|_| Err(ApiError::internal_message("the head parser panicked")));

    let _ = result.send(parsed);
}

/// Consumes `chunks` through the head parser, collecting the body when `collect_body` is set.
///
/// `body_capacity` pre-sizes the collected body; it is a hint and does not limit the read.
async fn parse_chunks(
    mut chunks: mpsc::Receiver<Result<Bytes, wreq::Error>>,
    collect_body: bool,
    max_bytes: u64,
    body_capacity: usize,
    mut cancelled: oneshot::Receiver<()>,
) -> Result<StreamedBody, ApiError> {
    let mut consumer = Consumer::new(collect_body, max_bytes, body_capacity);
    let mut since_yield = 0_u32;

    loop {
        let chunk = tokio::select! {
            biased;
            _ = &mut cancelled => {
                return Err(ApiError::internal_message("the response was cancelled"));
            }
            chunk = chunks.recv() => chunk,
        };
        let Some(chunk) = chunk else {
            break;
        };

        if !consumer.push(chunk)? {
            break;
        }

        since_yield = since_yield.saturating_add(1);

        // Let the other parsers on this thread run after a batch of chunks.
        if since_yield >= CHUNKS_PER_YIELD {
            since_yield = 0;
            tokio::task::yield_now().await;
        }
    }

    Ok(consumer.finish())
}

/// The parsing state for one response.
struct Consumer {
    /// The tokenizer and metadata sink.
    parser: HeadParser,
    /// The collected body, when requested.
    body: Option<Vec<u8>>,
    /// The number of body bytes read (decompressed).
    bytes_read: u64,
    /// Whether the read was cut short by a size limit.
    truncated: bool,
    /// Whether the size limit was just reached.
    at_limit: bool,
    /// The read cap in bytes.
    cap: usize,
    /// The read cap in bytes, as configured.
    max_bytes: u64,
    /// Whether the body is being collected.
    collect_body: bool,
}

impl Consumer {
    /// Builds a parser for one response.
    fn new(collect_body: bool, max_bytes: u64, body_capacity: usize) -> Self {
        Self {
            parser: HeadParser::new(),
            body: collect_body.then(|| Vec::with_capacity(body_capacity)),
            bytes_read: 0,
            truncated: false,
            at_limit: false,
            cap: usize::try_from(max_bytes).unwrap_or(usize::MAX),
            max_bytes,
            collect_body,
        }
    }

    /// Feeds one chunk, returning whether the read should continue.
    ///
    /// # Errors
    ///
    /// Returns an error when the chunk failed to read from the upstream response.
    fn push(&mut self, chunk: Result<Bytes, wreq::Error>) -> Result<bool, ApiError> {
        // The size limit was reached on the previous chunk. A further chunk proves the response
        // was cut short, while a clean end of stream proves it fit exactly and should not be
        // reported as truncated.
        if self.at_limit {
            self.truncated = true;

            return Ok(false);
        }

        let chunk = chunk.map_err(|error| {
            // `wreq::Error`'s renderings include the request URI; redact it before it can reach
            // the 5xx log line.
            ApiError::upstream(error.without_uri())
        })?;
        self.bytes_read = self
            .bytes_read
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));

        self.parser.feed(&chunk);

        if let Some(body) = self.body.as_mut()
            && body.len() < self.cap
        {
            let remaining = self.cap - body.len();
            let take = remaining.min(chunk.len());
            body.extend_from_slice(&chunk[..take]);

            // The final chunk crossed the cap: bytes were cut even though the loop ends before
            // the next `at_limit` check.
            if take < chunk.len() {
                self.truncated = true;
            }
        }

        if self
            .body
            .as_ref()
            .is_some_and(|body| body.len() >= self.cap)
        {
            self.at_limit = true;

            return Ok(true);
        }

        // Only the head-only mode can stop at the end of the head; the full mode keeps reading
        // the body until its cap or the end of the response.
        if !self.collect_body && self.parser.is_done() {
            return Ok(false);
        }

        if !self.collect_body && self.bytes_read >= self.max_bytes {
            self.at_limit = true;
        }

        Ok(true)
    }

    /// Finishes the parser and returns the streamed body.
    fn finish(mut self) -> StreamedBody {
        self.parser.finish();

        StreamedBody {
            metadata: self.parser.into_metadata(),
            body: self.body,
            bytes_read: self.bytes_read,
            truncated: self.truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Parses `chunks` through the real pool.
    async fn read_chunks(chunks: &[&[u8]], collect_body: bool, max_bytes: u64) -> StreamedBody {
        let (sender, receiver) = mpsc::channel(chunks.len().max(1));

        for chunk in chunks {
            sender
                .send(Ok(Bytes::copy_from_slice(chunk)))
                .await
                .expect("the parser accepts the chunk");
        }
        drop(sender);

        parse(receiver, collect_body, max_bytes, 0)
            .await
            .expect("reads")
    }

    #[tokio::test]
    async fn reserves_the_body_capacity_hint() {
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(Ok(Bytes::from_static(b"hello")))
            .await
            .expect("the parser accepts the chunk");
        drop(sender);

        let read = parse(receiver, true, HEAD_MAX_BYTES, 1024)
            .await
            .expect("reads");
        let body = read.body.expect("full mode collects the body");

        assert_eq!(body, b"hello");
        assert!(body.capacity() >= 1024);
    }

    #[tokio::test]
    async fn reads_full_bodies_up_to_the_cap() {
        struct Case {
            name: &'static str,
            chunks: &'static [&'static [u8]],
            cap: u64,
            bytes_read: u64,
            body: &'static [u8],
            truncated: bool,
        }

        let cases = [
            Case {
                name: "a body that fits the cap exactly is complete",
                chunks: &[b"xxxxxxxx"],
                cap: 8,
                bytes_read: 8,
                body: b"xxxxxxxx",
                truncated: false,
            },
            Case {
                name: "a further chunk proves the body was cut short",
                chunks: &[b"xxxxxxxx", b"yyyy"],
                cap: 8,
                bytes_read: 8,
                body: b"xxxxxxxx",
                truncated: true,
            },
            Case {
                name: "a single chunk crossing the cap is truncated",
                chunks: &[b"xxxxxxxxxxxx"],
                cap: 8,
                bytes_read: 12,
                body: b"xxxxxxxx",
                truncated: true,
            },
            Case {
                name: "a final chunk crossing the cap is truncated",
                chunks: &[b"xxxx", b"yyyyyyyy"],
                cap: 8,
                bytes_read: 12,
                body: b"xxxxyyyy",
                truncated: true,
            },
        ];

        for case in cases {
            let read = read_chunks(case.chunks, true, case.cap).await;

            assert_eq!(
                read.bytes_read, case.bytes_read,
                "bytes_read for {}",
                case.name
            );
            assert_eq!(
                read.body.as_deref(),
                Some(case.body),
                "body for {}",
                case.name
            );
            assert_eq!(
                read.truncated, case.truncated,
                "truncated for {}",
                case.name
            );
        }
    }

    #[tokio::test]
    async fn reads_heads_up_to_the_head_or_the_cap() {
        struct Case {
            name: &'static str,
            chunks: &'static [&'static [u8]],
            collect_body: bool,
            max_bytes: u64,
            bytes_read: u64,
            body: Option<&'static [u8]>,
            truncated: bool,
            title: Option<&'static str>,
        }

        const HTML: &[u8] = b"<html><head><title>Hello</title></head></html>";
        const HEAD: &[u8] = b"<html><head><title>Hello</title></head>";
        const TAIL: &[u8] = b"<body><p>Hello</p></body></html>";
        const FULL: &[u8] =
            b"<html><head><title>Hello</title></head><body><p>Hello</p></body></html>";

        let cases = [
            Case {
                name: "a head that fits the cap exactly parses completely",
                chunks: &[HTML],
                collect_body: false,
                max_bytes: u64::try_from(HTML.len()).expect("fits"),
                bytes_read: u64::try_from(HTML.len()).expect("fits"),
                body: None,
                truncated: false,
                title: Some("Hello"),
            },
            Case {
                name: "a further chunk proves the head was cut short",
                chunks: &[b"<<<<<<<<<<<<<<<<", b"xxxxxxxx"],
                collect_body: false,
                max_bytes: 16,
                bytes_read: 16,
                body: None,
                truncated: true,
                title: None,
            },
            Case {
                name: "full mode keeps reading past the head",
                chunks: &[HEAD, TAIL],
                collect_body: true,
                max_bytes: HEAD_MAX_BYTES,
                bytes_read: u64::try_from(FULL.len()).expect("fits"),
                body: Some(FULL),
                truncated: false,
                title: Some("Hello"),
            },
            Case {
                name: "head mode stops at the end of the head",
                chunks: &[HEAD, b"xxxxxxxx"],
                collect_body: false,
                max_bytes: HEAD_MAX_BYTES,
                bytes_read: u64::try_from(HEAD.len()).expect("fits"),
                body: None,
                truncated: false,
                title: Some("Hello"),
            },
        ];

        for case in cases {
            let read = read_chunks(case.chunks, case.collect_body, case.max_bytes).await;

            assert_eq!(
                read.metadata.title.as_deref(),
                case.title,
                "title for {}",
                case.name
            );
            assert_eq!(
                read.bytes_read, case.bytes_read,
                "bytes_read for {}",
                case.name
            );
            assert_eq!(read.body.as_deref(), case.body, "body for {}", case.name);
            assert_eq!(
                read.truncated, case.truncated,
                "truncated for {}",
                case.name
            );
        }
    }

    #[tokio::test]
    async fn dropping_the_future_cancels_the_parser() {
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(Ok(Bytes::from_static(b"<html><head>")))
            .await
            .expect("the parser accepts the chunk");

        let task = tokio::spawn(parse(receiver, false, HEAD_MAX_BYTES, 0));
        tokio::time::sleep(Duration::from_millis(10)).await;
        task.abort();
        let _ = task.await;

        tokio::time::timeout(Duration::from_secs(1), sender.closed())
            .await
            .expect("the parser releases the channel after the future is dropped");
    }
}
