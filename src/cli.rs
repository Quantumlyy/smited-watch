//! Command-line argument parsing.
//!
//! Mirrors the spec's CLI surface verbatim. Env vars (`SMITED_HOST`,
//! `SMITED_BACKEND_ID`, `SMITED_WATCH_CONFIG`) are wired through clap so
//! `--help` lists them next to their flag equivalents.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{ArgAction, Parser};

/// Default backend id used when neither config nor `--backend-id` provides one.
pub const DEFAULT_BACKEND_ID: &str = "mock-owo";

#[derive(Parser, Debug)]
#[command(
    name = "smited-watch",
    version,
    about = "Wrap a command and fire haptic sensations on the smited daemon when its output matches configured patterns.",
    trailing_var_arg = true
)]
pub struct Cli {
    /// Path to config file (defaults to platform-specific user dir).
    #[arg(
        short = 'c',
        long = "config",
        env = "SMITED_WATCH_CONFIG",
        value_name = "PATH"
    )]
    pub config: Option<PathBuf>,

    /// Override the daemon `host:port` from config.
    #[arg(
        short = 'H',
        long = "host",
        env = "SMITED_HOST",
        value_name = "HOST:PORT"
    )]
    pub host: Option<String>,

    /// Override the default backend id from config.
    #[arg(
        short = 'b',
        long = "backend-id",
        env = "SMITED_BACKEND_ID",
        value_name = "ID"
    )]
    pub backend_id: Option<String>,

    /// Increase log verbosity (repeatable; -v=DEBUG, -vv=TRACE).
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
    pub verbose: u8,

    /// Suppress all smited-watch logging. Wrapped command output is unaffected.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Match patterns and log them, but never fire triggers.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Skip the one-line "smited-watch wrapping <cmd>" banner.
    #[arg(long = "no-banner")]
    pub no_banner: bool,

    /// Command and arguments to run. Use `--` to separate smited-watch's options
    /// from the wrapped command's args.
    #[arg(last = true, num_args = 0..)]
    pub command: Vec<OsString>,
}

/// Map `--verbose` count + `--quiet` to a tracing log filter directive.
///
/// Quiet > verbose: passing `-q` always wins.
pub fn tracing_filter(verbose: u8, quiet: bool) -> String {
    if quiet {
        return "off".into();
    }
    let level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    // Restrict to our crate so cargo-test or downstream library noise
    // doesn't bleed through.
    format!("smited_watch={level}")
}
