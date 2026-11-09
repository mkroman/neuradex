//! The binary entry point.
//!
//! Without a subcommand the binary serves the HTTP service; `neuradex export-docs` writes the
//! documentation page to disk. Arguments are parsed with clap, so unknown arguments and typo'd
//! subcommands print the usage to stderr and exit with status 2 — they never start the server.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

#[cfg(feature = "docs")]
mod export;

use neuradex::config::Config;
use neuradex::http;
use neuradex::state::AppState;
use neuradex::telemetry;

/// The duration before an HTTP request times out, shared with the Kagi client.
const REQUEST_TIMEOUT: Duration = kagi::HTTP_TIMEOUT;

/// The environment variable selecting JSON log output.
const LOG_FORMAT_ENV: &str = "LOG_FORMAT";

/// A small API service implementing tools for LLM agents.
#[derive(Parser)]
#[command(
    name = "neuradex",
    version,
    after_help = "Environment:\n  KAGI_SESSION_TOKEN   the Kagi session token; required to serve, never for export-docs\n  LISTEN_ADDR          the address to bind to (default: 127.0.0.1:8080)\n  USER_AGENT           the user agent sent with fetches (default: a current Firefox one)\n  KAGI_MAX_SESSIONS    simultaneous searches (default: 2, maximum: 64)\n  KAGI_PRIVACY_PASS    use anonymous Privacy Pass search tokens (default: true)\n  RUST_LOG             tracing filter (default: info)\n  LOG_FORMAT           text or json for the log line format (default: text)\n  OTEL_SERVICE_NAME    service.name resource attribute (default: neuradex)\n  OTEL_RESOURCE_ATTRIBUTES  extra resource attributes, e.g. deployment.environment.name=production\n  OTEL_EXPORTER_OTLP_ENDPOINT  OTLP/HTTP endpoint for traces and logs; unset disables export\n  OTEL_TRACES_EXPORTER / OTEL_LOGS_EXPORTER  set none to disable a signal\n  OTEL_SDK_DISABLED    set true to disable OpenTelemetry entirely\n"
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
    export::run(&arguments.output, arguments.spec.as_deref())
        .map_err(|error| error.to_string().into())
}

/// Rejects `export-docs` in builds without the `docs` feature: there is nothing to render.
#[cfg(not(feature = "docs"))]
fn export_docs(_arguments: &ExportDocsArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err("this binary was built without the docs feature; rebuild with --features docs".into())
}

#[cfg(all(test, feature = "docs"))]
mod tests {
    use super::*;

    #[test]
    fn renders_export_failures_as_plain_messages() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let arguments = ExportDocsArgs {
            output: directory.path().join("missing").join("docs.html"),
            spec: None,
        };

        let error = export_docs(&arguments).expect_err("the export fails");
        let rendered = format!("{error:?}");

        assert!(
            rendered.contains("could not write"),
            "unexpected error: {rendered}"
        );
        assert!(
            !rendered.contains("Write {"),
            "the derived debug representation leaked: {rendered}"
        );
    }
}

/// Whether `LOG_FORMAT` selects JSON output.
fn json_logs() -> bool {
    is_json_format(std::env::var(LOG_FORMAT_ENV).ok().as_deref())
}

/// Whether `value` selects JSON output, ignoring ASCII case.
fn is_json_format(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("json"))
}

/// Builds the application state and serves the HTTP service.
///
/// The telemetry providers are shut down on every path after initialization, including
/// configuration and state-build failures, so buffered records are flushed.
fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let telemetry = telemetry::init(json_logs())?;
    let result = run_service();

    telemetry.shutdown();

    result
}

/// Reads the configuration, builds the application state, and runs the server.
fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    // Keep the startup errors the CLI printed before the config extraction: the plain message,
    // not the error type's `Debug` representation.
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => return Err(error.to_string().into()),
    };
    tracing::info!(
        "neuradex.search.authentication" = if config.privacy_pass {
            "privacy_pass"
        } else {
            "session"
        },
        "neuradex.search.max_sessions" = config.max_sessions,
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

    let served = runtime.block_on(http::serve(state, config.listen_addr));

    Ok(served?)
}

#[cfg(test)]
mod log_format_tests {
    use super::is_json_format;

    #[test]
    fn selects_json_logs() {
        assert!(is_json_format(Some("json")));
        assert!(is_json_format(Some(" JSON ")));
        assert!(!is_json_format(Some("text")));
        assert!(!is_json_format(None));
    }
}
