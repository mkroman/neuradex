//! The binary entry point.
//!
//! Without a subcommand the binary serves the HTTP service; `neuradex export-docs` writes the
//! documentation page to disk. Arguments are parsed with clap, so unknown arguments and typo'd
//! subcommands print the usage to stderr and exit with status 2 — they never start the server.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use neuradex_audit::{AuditSink, HeaderPolicy, JsonlSink};
use secrecy::ExposeSecret;

#[cfg(feature = "docs")]
mod export;
mod otel;

use neuradex::audit::AuditConfig;
use neuradex::config::{Config, ContentBackend};
use neuradex::content;
use neuradex::http;
use neuradex::state::AppState;

/// The duration before an HTTP request times out, shared with the Kagi client.
const REQUEST_TIMEOUT: Duration = kagi::HTTP_TIMEOUT;

/// The environment variable selecting JSON log output.
const LOG_FORMAT_ENV: &str = "LOG_FORMAT";

/// A small API service implementing tools for LLM agents.
#[derive(Parser)]
#[command(
    name = "neuradex",
    version,
    after_help = "Environment:\n  KAGI_SESSION_TOKEN   the Kagi session token; required to serve, never for export-docs\n  KAGI_BASE_URL        override the Kagi base URL (default: https://kagi.com)\n  LISTEN_ADDR          the address to bind to (default: 127.0.0.1:8080)\n  USER_AGENT           the user agent sent with fetches (default: a current Firefox one)\n  KAGI_MAX_SESSIONS    simultaneous searches (default: 2, maximum: 64)\n  KAGI_PRIVACY_PASS    use anonymous Privacy Pass search tokens (default: true)\n  KAGI_PRIVACY_PASS_STATE  file for unspent Privacy Pass tokens (default: XDG state; off disables)\n  RUST_LOG             tracing filter (default: info)\n  LOG_FORMAT           text or json for the log line format (default: text)\n  AUDIT_LOG_PATH       JSONL audit event log path (- or unset: stdout)\n  AUDIT_HMAC_KEY       key for hashing identifiers and IPs in audit headers\n  CONTENT_STORE        off, local, or s3 (default: off)\n  CONTENT_LOCAL_PATH   the local content store root; required for local\n  CONTENT_S3_ENDPOINT / CONTENT_S3_REGION / CONTENT_S3_BUCKET / CONTENT_S3_PREFIX  S3 settings\n  CONTENT_S3_ACCESS_KEY_ID / CONTENT_S3_SECRET_ACCESS_KEY  S3 credentials\n  CONTENT_S3_FORCE_PATH_STYLE  path-style S3 addressing (default: true)\n  CONTENT_PACK_TARGET_BYTES  target plaintext bytes per pack (default: 134217728)\n  CONTENT_MAX_PACK_AGE_SECS  maximum seconds before an open pack is sealed (default: 600)\n  CONTENT_MAX_BODY_BYTES  maximum captured request body bytes (default: 26214400)\n  CONTENT_RETENTION_DAYS  content retention period (default: 365)\n  CONTENT_COMPRESSION_LEVEL  zstd level for packs (default: 3)\n  OTEL_SERVICE_NAME    service.name resource attribute (default: neuradex)\n  OTEL_RESOURCE_ATTRIBUTES  extra resource attributes, e.g. deployment.environment.name=production\n  OTEL_EXPORTER_OTLP_ENDPOINT  OTLP/HTTP endpoint for traces and logs; unset disables export\n  OTEL_TRACES_EXPORTER / OTEL_LOGS_EXPORTER  set none to disable a signal\n  OTEL_SDK_DISABLED    set true to disable OpenTelemetry entirely\n"
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
    let telemetry = otel::init(json_logs())?;
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
    log_privacy_pass_state(&config);

    let runtime = tokio::runtime::Runtime::new()?;

    let content = content::build(&config.content)?;
    log_content(&config);

    let state = AppState::try_new_with_audit_and_base_url(
        &config.user_agent,
        config.kagi_token().clone(),
        config.kagi_base_url(),
        REQUEST_TIMEOUT,
        config.max_sessions,
        config.privacy_pass,
        config.privacy_pass_fallback,
        config.privacy_pass_state(),
        audit_config(&config)?,
    )?;
    let state = match content {
        Some(content) => state.with_content(content),
        None => state,
    };

    let served = runtime.block_on(http::serve(state, config.listen_addr));

    Ok(served?)
}

/// Logs the Privacy Pass token persistence path without any token values.
fn log_privacy_pass_state(config: &Config) {
    if !config.privacy_pass {
        return;
    }

    if let Some(path) = config.privacy_pass_state() {
        tracing::info!(
            "neuradex.privacy_pass.state" = %path.display(),
            "unspent Privacy Pass tokens persist across restarts"
        );
    } else {
        tracing::info!(
            "neuradex.privacy_pass.state" = "memory",
            "unspent Privacy Pass tokens are kept in memory only; no XDG state directory is available"
        );
    }
}

/// Logs the content store mode and policy without credentials.
fn log_content(config: &Config) {
    let policy = config.content.policy();

    match config.content.backend() {
        ContentBackend::Disabled => tracing::info!(
            "neuradex.content.store" = "off",
            "content storage is disabled"
        ),
        ContentBackend::Local { path } => tracing::info!(
            "neuradex.content.store" = "local",
            "neuradex.content.path" = %path.display(),
            "neuradex.content.pack_target_bytes" = policy.pack_target_bytes(),
            "neuradex.content.max_pack_age_secs" = policy.max_pack_age().as_secs(),
            "neuradex.content.max_body_bytes" = policy.max_captured_body_bytes(),
            "neuradex.content.retention_days" = policy.retention_days(),
            "neuradex.content.compression_level" = policy.compression_level(),
            "content storage configured"
        ),
        ContentBackend::S3(settings) => tracing::info!(
            "neuradex.content.store" = "s3",
            "neuradex.content.bucket" = settings.bucket(),
            "neuradex.content.prefix" = settings.prefix().unwrap_or_default(),
            "neuradex.content.force_path_style" = settings.force_path_style(),
            "neuradex.content.pack_target_bytes" = policy.pack_target_bytes(),
            "neuradex.content.max_pack_age_secs" = policy.max_pack_age().as_secs(),
            "neuradex.content.max_body_bytes" = policy.max_captured_body_bytes(),
            "neuradex.content.retention_days" = policy.retention_days(),
            "neuradex.content.compression_level" = policy.compression_level(),
            "content storage configured"
        ),
    }
}

/// Builds the audit sink and header policy from the service configuration.
///
/// Audit events are always written as JSON Lines by the sink itself, independently of
/// `RUST_LOG` and `LOG_FORMAT`; the local tracing formatter never filters them. Without
/// `AUDIT_LOG_PATH` (or with `-`), events go to stdout.
fn audit_config(config: &Config) -> Result<AuditConfig, Box<dyn std::error::Error>> {
    let mut header_policy = HeaderPolicy::new();

    if let Some(key) = config.audit_hmac_key() {
        header_policy = header_policy.with_hmac_key(key.expose_secret().as_bytes().to_vec());
    }

    let sink: Arc<dyn AuditSink> = match config.audit_log_path() {
        Some(path) => Arc::new(JsonlSink::new(open_audit_log(path)?)),
        None => Arc::new(JsonlSink::new(std::io::stdout())),
    };

    Ok(AuditConfig::new(
        sink,
        header_policy,
        config.content.max_body_bytes(),
    ))
}

/// Opens the audit log for appending, creating it when it does not exist.
fn open_audit_log(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().create(true).append(true).open(path)
}
