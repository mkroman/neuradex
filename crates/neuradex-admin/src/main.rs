//! Administrative commands for the neuradex content plane.
//!
//! This binary is intentionally separate from the service: it needs write credentials for
//! bucket configuration and must never be linked into the request path.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod config;
mod content_lifecycle;

/// Administrative commands for neuradex.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// The command to run.
    #[command(subcommand)]
    command: Command,
}

/// Administrative subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Generate or apply the content retention lifecycle policy.
    #[command(subcommand)]
    ContentLifecycle(content_lifecycle::Command),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::ContentLifecycle(command) => content_lifecycle::run(command).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
