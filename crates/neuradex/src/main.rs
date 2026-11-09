//! The binary entry point.
//!
//! Without a subcommand the binary serves the HTTP service; `neuradex export-docs` writes the
//! documentation page to disk. Arguments are parsed with clap, so unknown arguments and typo'd
//! subcommands print the usage to stderr and exit with status 2 — they never start the server.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[cfg(feature = "docs")]
mod export;

use neuradex::config::Config;
use neuradex::http;
use neuradex::state::AppState;

/// The duration before an HTTP request times out, shared with the Kagi client.
const REQUEST_TIMEOUT: Duration = kagi::HTTP_TIMEOUT;

/// A small API service implementing tools for LLM agents.
#[derive(Parser)]
#[command(
    name = "neuradex",
    version,
    after_help = "Environment:\n  KAGI_SESSION_TOKEN   the Kagi session token; required to serve, never for export-docs\n  LISTEN_ADDR          the address to bind to (default: 127.0.0.1:8080)\n  USER_AGENT           the user agent sent with fetches (default: a current Firefox one)\n  KAGI_MAX_SESSIONS    simultaneous searches (default: 2)\n  KAGI_PRIVACY_PASS    use anonymous Privacy Pass search tokens (default: true)\n"
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

    // Keep the startup errors the CLI printed before the config extraction: the plain message,
    // not the error type's `Debug` representation.
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => return Err(error.to_string().into()),
    };
    tracing::info!(
        privacy_pass = config.privacy_pass,
        "Kagi search authentication configured"
    );

    let runtime = tokio::runtime::Runtime::new()?;

    let state = AppState::try_new(
        &config.user_agent,
        config.kagi_token().clone(),
        REQUEST_TIMEOUT,
        config.max_sessions,
        config.privacy_pass,
    )?;

    Ok(runtime.block_on(http::serve(state, config.listen_addr))?)
}
