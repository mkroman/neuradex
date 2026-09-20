# kagi

This is a Rust crate that implements a client and parser for
[Kagi Search](https://kagi.com).

It queries the same streaming endpoints used by the browser experience and
parses the responses into structured web and image results. Authentication
uses a session token, i.e. the value of the `kagi_session` cookie from an
authenticated browser session.

## Features

* Web search via [`Client::search`] and image search via [`Client::images`]
* Selectable TLS backend: `rustls` (default), `native-tls` or
  `native-tls-vendored`
* Opt-out decompression of compressed responses (`gzip`, `brotli`, `deflate`,
  `zstd`)
* The session token is kept in memory as a `SecretString`
* A pool of up to `max_sessions` persistent sessions, each with its own nonce
  and refresh cycle, grown one session at a time under load

## Quick Start

To get started, add this crate to your `Cargo.toml`. The main entry points for
searching are the [`Client::search`] and [`Client::images`] methods.

```rust
use kagi::{Client, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Create a new client with a Kagi session token.
    let client = Client::with_token("kagi session token");

    // Search for the given query.
    let results = client.search("rust programming language").await?;

    if let Some(result) = results.first() {
        println!("{} - {}", result.title, result.url);
    }

    // Or search for images.
    let images = client.images("ferrous wheel").await?;

    if let Some(image) = images.first() {
        println!("{} ({}x{}) {}", image.title, image.width, image.height, image.image_url);
    }

    Ok(())
}
```

## Sessions

The client maintains a pool of persistent sessions, each mirroring one browser
tab: every in-flight search uses its own session, and only the first stream
request of a session carries the page's `sse_nonce`.

The pool holds at most [`ClientOptions::max_sessions`] sessions and starts
empty. Under an inrush of requests it grows by establishing sessions **one at
a time** — at most one creation request runs at any moment. When the pool is
at capacity, further requests wait for a session to be checked back in; they
stay pending for as long as the caller does, with no deadline of their own.

Sessions expire independently after `session_duration` and are refreshed in
place with a new nonce on their next use. A failed fetch (session or nonce
request) backs off exponentially for that session — 500 ms doubling up to
30 s — before it is retried, and the error is returned to the request that
triggered it.
