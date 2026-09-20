//! The binary entry point.

use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use secrecy::SecretString;
use tracing_subscriber::EnvFilter;

use neuradex::api::v1::AppState;
use neuradex::http;

/// The environment variable holding the Kagi session token.
const KAGI_TOKEN_ENV: &str = "KAGI_SESSION_TOKEN";

/// The environment variable holding the listen address.
const LISTEN_ADDR_ENV: &str = "LISTEN_ADDR";

/// The environment variable holding the `User-Agent` header sent with requests.
const USER_AGENT_ENV: &str = "USER_AGENT";

/// The environment variable holding the maximum number of simultaneous Kagi sessions.
const KAGI_MAX_SESSIONS_ENV: &str = "KAGI_MAX_SESSIONS";

/// The default listen address.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8080";

/// The default user agent.
const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:155.0) Gecko/20100101 Firefox/155.0";

/// The duration before an HTTP request times out.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let listen_addr = env::var(LISTEN_ADDR_ENV).unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_string());
    let addr: SocketAddr = listen_addr.parse().map_err(|_| {
        format!("invalid {LISTEN_ADDR_ENV}: expected a socket address, got {listen_addr:?}")
    })?;
    let user_agent = env::var(USER_AGENT_ENV).unwrap_or_else(|_| DEFAULT_USER_AGENT.to_string());
    let max_sessions = match env::var(KAGI_MAX_SESSIONS_ENV) {
        Ok(value) => value.trim().parse().map_err(|_| {
            format!("invalid {KAGI_MAX_SESSIONS_ENV}: expected a positive integer, got {value:?}")
        })?,
        Err(_) => kagi::DEFAULT_MAX_SESSIONS,
    };
    let kagi_token = env::var(KAGI_TOKEN_ENV)
        .ok()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| format!("{KAGI_TOKEN_ENV} is required"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let state = AppState::new(
        &user_agent,
        SecretString::from(kagi_token),
        REQUEST_TIMEOUT,
        max_sessions,
    )?;

    Ok(runtime.block_on(http::serve(state, addr))?)
}
