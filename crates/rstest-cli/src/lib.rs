mod cache;
mod cli;
#[allow(dead_code)]
mod collect; // D5: single-point collection
#[allow(dead_code)]
mod config;
mod discover;
mod doctor;
mod migrate;
mod mono;
mod remote;
mod reporting;
mod run;
mod scheduling;
mod select;
mod text;
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
    if cli.watch {
        watch::watch_loop(&cli, &args)?;
        return Ok(0);
    }
    execute(&cli, &args)
}
