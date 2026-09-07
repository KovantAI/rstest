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
/// Orchestrator<->worker wire protocol. Re-exported so the decode benchmark
/// (`benches/proto_decode.rs`) and the fuzz target can build frames and decode
/// them against the exact types the orchestrator uses.
pub use scheduling::proto;

/// Entry point shared by the `rstest` binary and integration tests. Parses
/// argv, dispatches to the watch loop or a single run, and returns the process
/// exit status (the bin turns it into `process::exit`).
pub fn run() -> Result<i32> {
    let (own_args, args) = cli::split_argv();
    let cli = Cli::parse_from(&own_args);
    if cli.watch {
        watch::watch_loop(&cli, &args)?;
        return Ok(0);
    }
    execute(&cli, &args)
}
