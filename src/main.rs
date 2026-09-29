//! The binary entry point.
//!
//! Without a subcommand the binary serves the HTTP service; `neuradex export-docs` writes the
//! documentation page to disk. Arguments are parsed with clap, so unknown arguments and typo'd
//! subcommands print the usage to stderr and exit with status 2 — they never start the server.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use secrecy::SecretString;
use tracing_subscriber::EnvFilter;

#[cfg(feature = "docs")]
mod export;

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

/// A small API service implementing tools for LLM agents.
#[derive(Parser)]
#[command(
    name = "neuradex",
    version,
    after_help = "Environment:\n  KAGI_SESSION_TOKEN   the Kagi session token; required to serve, never for export-docs\n  LISTEN_ADDR          the address to bind to (default: 127.0.0.1:8080)\n  USER_AGENT           the user agent sent with fetches (default: a current Firefox one)\n  KAGI_MAX_SESSIONS    simultaneous Kagi sessions (default: 2)\n"
)]
struct Cli {
    /// The subcommand to run; without one, the HTTP service is served.
    #[command(subcommand)]
    command: Option<Command>,
}

/// The available subcommands.
#[derive(Subcommand)]
enum Command {
    /// Write the API documentation page to disk.
    ///
    /// Requires no configuration and no KAGI_SESSION_TOKEN: the subcommand never builds the
    /// application state.
    ExportDocs(ExportDocsArgs),
}

/// The arguments of `export-docs`.
#[derive(Args)]
#[cfg_attr(not(feature = "docs"), allow(dead_code))]
struct ExportDocsArgs {
    /// The path to write the rendered documentation page to.
    output: PathBuf,
    /// Also write the OpenAPI document to this path, as pretty-printed JSON.
    #[arg(long, value_name = "FILE")]
    spec: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        None => serve(),
        Some(Command::ExportDocs(arguments)) => export_docs(&arguments),
    }
}

/// Runs the `export-docs` subcommand: renders the documentation page (and optionally the
/// OpenAPI document) to files on disk.
#[cfg(feature = "docs")]
fn export_docs(arguments: &ExportDocsArgs) -> Result<(), Box<dyn std::error::Error>> {
    export::run(&arguments.output, arguments.spec.as_deref()).map_err(Into::into)
}

/// Rejects `export-docs` in builds without the `docs` feature: there is nothing to render.
#[cfg(not(feature = "docs"))]
fn export_docs(_arguments: &ExportDocsArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err("this binary was built without the docs feature; rebuild with --features docs".into())
}

/// Builds the application state and serves the HTTP service.
fn serve() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let listen_addr = env::var(LISTEN_ADDR_ENV).unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_string());
    let addr: SocketAddr = listen_addr.parse().map_err(|_| {
        format!("invalid {LISTEN_ADDR_ENV}: expected a socket address, got {listen_addr:?}")
    })?;
    let user_agent = env::var(USER_AGENT_ENV).unwrap_or_else(|_| DEFAULT_USER_AGENT.to_string());
    let max_sessions = match env::var(KAGI_MAX_SESSIONS_ENV) {
        Ok(value) => value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|count| *count > 0)
            .ok_or_else(|| {
                format!(
                    "invalid {KAGI_MAX_SESSIONS_ENV}: expected a positive integer, got {value:?}"
                )
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
