//! rstest: a fast, parallel, pytest-compatible test runner.

// Every `unsafe` block must carry a `// SAFETY:` justification. All unsafe here
// is thin FFI (libc / windows-sys) around pipe endpoints and process probes.
#![warn(clippy::undocumented_unsafe_blocks)]

mod cache;
mod cli;
mod collect; // D5: single-point collection
mod config;
mod cov_scope;
mod coverage_skip;
mod discover;
mod doctor;
mod explain;
mod incremental;
mod migrate;
mod mono;
mod remote;
mod replay;
mod reporting;
mod run;
mod scheduling;
#[cfg(test)]
mod schema;
mod select;
mod shardverify;
#[cfg(test)]
mod test_env;
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
    // Anchor `.rstest_cache` at pytest's rootdir before anything reads it, so
    // a run from a subdirectory shares the project's cache (as `.pytest_cache`).
    if let Ok(cwd) = std::env::current_dir() {
        let rootdir = config::rootdir(&cwd, &args);
        cache::init(cwd, rootdir);
    }
    // Run-less subcommands (verify-vendor / try / migrate-check / cache-compact)
    // do their own thing and exit before the run pipeline is built.
    if let Some(code) = run::dispatch_command(&cli, &args)? {
        return Ok(code);
    }
    // `-p no:cacheprovider` (argv or addopts) keeps pytest from writing
    // `.pytest_cache`; keep `.rstest_cache` out of the project too, for the
    // runs only (the subcommands above exist to read an existing cache).
    let _scratch_cache = std::env::current_dir()
        .ok()
        .map(|cwd| cov_scope::effective_pytest_args(&cwd, &args))
        .filter(|eff| cache::cacheprovider_disabled(eff))
        .and_then(|_| cache::disable_for_run());
    if cli.watch {
        watch::watch_loop(&cli, &args)?;
        return Ok(0);
    }
    let code = execute(&cli, &args)?;
    // A usage error with a forwarded `--output`: most likely a mistyped style
    // that pytest (with no plugin defining `--output`) rejected.
    if code == 4 {
        if let Some(value) = cli::forwarded_output(&args) {
            eprintln!(
                "rstest: `--output {value}` is not an rstest output style ({}), so it went to \
                 pytest as a plugin option (pytest-playwright's --output). If pytest rejected \
                 it above, use one of the styles.",
                cli::OUTPUT_STYLES.join("|")
            );
        }
    }
    Ok(code)
}
