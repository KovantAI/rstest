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
mod watch;

use anyhow::Result;
use clap::Parser;

pub use cli::Cli;
pub use run::execute;

fn main() -> Result<()> {
    let (own_args, args) = cli::split_argv();
    let cli = Cli::parse_from(&own_args);
    if cli.watch {
        return watch::watch_loop(&cli, &args);
    }
    let status = execute(&cli, &args)?;
    std::process::exit(status);
}
