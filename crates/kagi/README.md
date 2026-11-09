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
* Optional [Privacy Pass](#privacy-pass) authentication: searches redeem
  anonymous batched VOPRF tokens instead of sending the session token or
  cookies

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

## Privacy Pass

Privacy Pass mode issues anonymous batched VOPRF tokens (token type `0xF91A`)
from Kagi's `/pp/wwwa` and `/pp/gettokens` endpoints and redeems one token per
search request. The configured session token is used only to authorize
issuance; searches send no cookies, no token query parameter, and no
`X-Kagi-Authorization` header.

```rust
use kagi::{Client, Error};

# async fn example() -> Result<(), Error> {
// The token is still the kagi_session value from an authenticated session.
let client = Client::with_privacy_pass("kagi session token")?;
let results = client.search("rust programming language").await?;
# Ok(())
# }
```

`ClientOptions::privacy_pass` accepts a `PrivacyPassOptions` value to tune the
batch size, store capacity, and low-water mark. Tokens live in memory, are
consumed once, and are refilled on demand or in the background when the store
runs low.

## Wire protocol

Searches stream from `/socket/search?q=…` and image searches from `/socket/images?q=…`. The
first stream request of a session appends the target document's `window.sse_nonce` as `nonce`;
later requests reuse the session cookies without it. Kagi scopes the nonce to a single HTML
document, so when establishing a session the client first loads `/search?q=…` or `/images?q=…`
and uses that document's trace ID, mirroring the browser. Both transports Kagi uses are parsed:
standard server-sent events (`data: [...]` arrays, ending with `id: CLOSE`) and the
`application/vnd.kagi.stream` NUL-delimited framing (`tag:JSON\0\n`) used by the site's
fetch-stream mode.

Structured `search_results_json` payloads carry `rank` and `source` in addition to the title,
URL, and snippet; `SearchResult` exposes them, and titles/snippets are converted to plain text
so Kagi's `<strong>` highlight markup does not leak into the API. The HTML fallbacks (`search`
and `images` messages) remain supported and leave `rank` and `source` empty.

Image results expose Kagi's original image URL (the sentinel `#no_preview` when Kagi cannot
preview the image), the proxied thumbnail, dimensions, host, and Kagi's rank field.

Kagi's `/autosuggest` endpoints and its server-rendered `script_stream` transport are outside
this crate's scope; the client uses the `/socket/search` and `/socket/images` streams.

## Request headers

Requests mirror the headers a current Firefox session sends: the session token as
`X-Kagi-Authorization`, browser `Accept`, `Accept-Language`, and `Sec-Fetch-*` headers, and the
`Priority` hint. The session cookies (`kagi_session` and `_kagi_search_`) live in a per-session
cookie jar, so requests reuse and refresh them exactly like a browser tab.

## Sessions

The client maintains a pool of persistent sessions, each mirroring one browser
tab: every in-flight search uses its own session, and only the first stream
request of a session carries the session's `sse_nonce`.

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

A stream response that rejects the session's credentials (`401` or `403`)
invalidates it: the session is dropped and re-established on the next use,
after the same backoff. Any other stream status (rate limits, server errors)
keeps the session — it says nothing about the session itself.
