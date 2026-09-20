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
