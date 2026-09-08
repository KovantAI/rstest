//! rstest: a fast, parallel, pytest-compatible test runner.

// Every `unsafe` block must carry a `// SAFETY:` justification. All unsafe here
// is thin FFI (libc / windows-sys) around pipe endpoints and process probes.
#![warn(clippy::undocumented_unsafe_blocks)]

mod cache;
mod cli;
#[allow(dead_code)]
mod collect; // D5: single-point collection
#[allow(dead_code)]
mod config;
mod coverage_skip;
mod discover;
mod doctor;
mod incremental;
mod migrate;
mod mono;
mod remote;
mod reporting;
mod run;
mod scheduling;
mod select;
mod text;
mod time;
mod vendor;
mod watch;

use anyhow::Result;
use clap::Parser;

pub use cli::Cli;
pub use run::execute;

/// Entry point shared by the `rstest` binary and integration tests. Parses
/// argv, dispatches to the watch loop or a single run, and returns the process
/// exit status (the bin turns it into `process::exit`).
pub fn run() -> Result<i32> {
    let (own_args, args) = cli::split_argv();
    let cli = Cli::parse_from(&own_args);
    // Run-less subcommands (verify-vendor / try / migrate-check / cache-compact)
    // do their own thing and exit before the run pipeline is built.
    if let Some(code) = run::dispatch_command(&cli, &args)? {
        return Ok(code);
    }
    if cli.watch {
        watch::watch_loop(&cli, &args)?;
        return Ok(0);
    }
    execute(&cli, &args)
}
