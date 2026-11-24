//! Worker command and environment: the `WorkerEnv` a run passes down, the
//! child command it builds, the `RSTEST_*` env it sets, and secret scrubbing.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::Stdio;

/// Run-wide parameters handed to a worker via its environment at spawn time
/// (thread-safe), rather than mutating the orchestrator's own process env with
/// `std::env::set_var` (a data race across threads, `unsafe` in edition 2024).
#[derive(Clone)]
pub struct WorkerEnv {
    /// Shared testrun uid (xdist `testrun_uid` contract); one per run.
    pub run_uid: String,
    /// Enable cpu/fixture instrumentation in the worker's shim plugin.
    pub doctor: bool,
    /// Per-test timeout in seconds (--timeout): the worker interrupts a test
    /// whose call phase overruns. None = disabled.
    pub timeout: Option<f64>,
    /// Enable per-test thread/fd leak measurement (--doctor or --fail-on-leak).
    pub leakcheck: bool,
    /// `--fork-pool=app`: workers record their imports; the zygote imports
    /// the manifest's modules before forking. None = off.
    pub warm: Option<crate::scheduling::warm::WorkerWarm>,
    /// `--warn-on-dead-master-path`: scan plugins and conftests for code that
    /// only runs on an xdist controller.
    pub dead_master_scan: bool,
    /// For a lone worker: ship the full id/location payload from collection
    /// (pooled workers derive this from their index instead).
    pub send_ids: bool,
    /// `--debug`: start debugpy in the worker on this port and wait for an
    /// editor to attach before collecting. Only ever set for the lone
    /// passthrough worker (a debug run forces single-worker mode). None = off.
    pub debug_port: Option<String>,
    /// A streaming consumer (`--output json` / `--stream-json`) is attached, so
    /// the worker ships captured stdout/stderr/log `sections` on **every**
    /// report, not just failures. Off by default to keep the wire lean.
    pub stream_output: bool,
    /// `--junitxml` was given (the user's path, verbatim): the worker runs
    /// pytest's LogXML and streams its testcase elements back. None = off.
    pub junitxml: Option<String>,
    /// A global `--reruns` budget is in play: a pool worker's own `-x` /
    /// `--maxfail` count skips every failed attempt (the orchestrator may
    /// retry it and counts the final outcome).
    pub reruns: bool,
    /// The run's quarantine, for a session's own `-x` / `--maxfail` count.
    pub quarantine: Option<WorkerQuarantine>,
    /// Outbound-network recorder (doctor): lists connections outside loopback
    /// and the allowlist. None = off.
    pub network: Option<NetworkGuard>,
}

/// [`WorkerEnv::quarantine`]: a session never counts a `--quarantine` file
/// failure. A single session (`-n 0`) counts auto-quarantined failures only
/// once more auto tests failed than `max_pct` percent of its tests (the
/// post-run cap); a pool worker never does, the orchestrator holds them.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerQuarantine {
    /// File patterns, anchored regexes one per line (`RSTEST_QUARANTINE`).
    pub file: String,
    /// Auto-quarantined nodeids, one per line (`RSTEST_AUTO_QUARANTINE`).
    pub auto: String,
    /// `--auto-quarantine-max` (`RSTEST_AUTO_QUARANTINE_MAX`).
    pub max_pct: u32,
}

/// See [`WorkerEnv::network`].
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkGuard {
    /// Hostnames and CIDRs a test may reach, beyond loopback.
    pub allow: Vec<String>,
}

/// Set the network guard's variables on a worker command.
pub(super) fn apply_network_env(command: &mut Command, env: &WorkerEnv) {
    if let Some(g) = &env.network {
        command.env("RSTEST_NETWORK", "record");
        command.env("RSTEST_NETWORK_ALLOW", g.allow.join("\n"));
    }
}

/// Orchestrator-to-worker variables, set per run in [`build_worker_command`]
/// and never inherited. Deliberately absent: `RSTEST_RUN_UID` (a monorepo
/// parent hands it to each project's rstest so they share one run id, and
/// every worker gets it set explicitly anyway), `RSTEST_WORKER_PATH` and
/// `RSTEST_MAX_MESSAGE_BYTES` (user settings rstest honors).
pub(super) const INTERNAL_ENV: &[&str] = &[
    "RSTEST_WORKER_ID",
    "RSTEST_WORKER_COUNT",
    "RSTEST_BASETEMP",
    "RSTEST_SEND_IDS",
    "RSTEST_DOCTOR",
    "RSTEST_TIMEOUT",
    "RSTEST_LEAKCHECK",
    "RSTEST_WARM_RECORD",
    "RSTEST_WARM_MANIFEST",
    "RSTEST_WARM_RESULT",
    "RSTEST_DEAD_MASTER_SCAN",
    "RSTEST_DEBUGPY_PORT",
    "RSTEST_STREAM_OUTPUT",
    "RSTEST_JUNITXML",
    "RSTEST_RERUNS",
    "RSTEST_QUARANTINE",
    "RSTEST_AUTO_QUARANTINE",
    "RSTEST_AUTO_QUARANTINE_MAX",
    "RSTEST_NETWORK",
    "RSTEST_NETWORK_ALLOW",
];

/// Credentials rstest itself consumes and test code never needs. Stripped from
/// every process that runs the user's tests so a test or conftest (e.g. in a
/// pull request) can't read the shared cache's write token.
pub(super) const SECRET_ENV: &[&str] = &["RSTEST_CACHE_REMOTE_TOKEN"];

/// Remove [`SECRET_ENV`] from a child that will execute test code.
pub fn scrub_secrets(command: &mut Command) {
    for key in SECRET_ENV {
        command.env_remove(key);
    }
}

/// The orchestrator's version, passed to every worker, which refuses to run
/// under a different one (`rstest_worker/__main__.py`): a `rstest` on PATH and
/// the rstest installed in the project's environment can drift apart.
pub(crate) const ORCHESTRATOR_VERSION_ENV: &str = "RSTEST_ORCHESTRATOR_VERSION";

/// Build the worker's [`Command`] (argv + per-run child environment + stdio)
/// without spawning it. Split out of [`super::Worker::spawn_with_io`] so the arg/env
/// wiring is unit-testable via `Command::get_args`/`get_envs` — no live process.
/// `cmd_fd`/`evt_fd` are the child's raw pipe endpoints (fd on unix, HANDLE on
/// Windows), passed to the worker as numeric argv.
pub(super) fn build_worker_command(
    python: &Path,
    worker: Option<(usize, usize)>,
    io: Stdio,
    env: &WorkerEnv,
    cmd_fd: u64,
    evt_fd: u64,
) -> Command {
    let mut command = Command::new(python);
    // `--debug`: turn off frozen modules so debugpy's breakpoints land
    // reliably (frozen stdlib frames swallow them) and its startup
    // frozen-modules warning stays quiet. An interpreter flag, so it must
    // precede `-m`. Only for a debug run — no cost on the normal path.
    if env.debug_port.is_some() {
        command.args(["-X", "frozen_modules=off"]);
    }
    command
        .args([
            "-m",
            "rstest_worker",
            &cmd_fd.to_string(),
            &evt_fd.to_string(),
        ])
        .env("PYTHONPATH", worker_pythonpath())
        .env(ORCHESTRATOR_VERSION_ENV, env!("CARGO_PKG_VERSION"))
        // Run-wide params ride the CHILD's environment (thread-safe), never
        // process-global `set_var` (which races across threads / is unsafe
        // in edition 2024).
        .env("RSTEST_RUN_UID", &env.run_uid)
        // Worker stdout is not ours to show: output is rendered Rust-side,
        // except passthrough mode which inherits so pytest renders.
        .stdout(match io {
            Stdio::Null => std::process::Stdio::null(),
            Stdio::Inherit => std::process::Stdio::inherit(),
        })
        // stdin is only the worker's in passthrough (pdb, `input()` under
        // -s). Elsewhere it must not inherit: `--watch` keeps a thread
        // blocked reading stdin for `q`, and on Windows a pending synchronous
        // read on a pipe blocks the child interpreter's startup probe of fd 0,
        // hanging every worker.
        .stdin(match io {
            Stdio::Null => std::process::Stdio::null(),
            Stdio::Inherit => std::process::Stdio::inherit(),
        })
        // stderr is forwarded live either way. Outside passthrough it goes
        // through a pipe so its tail can explain a worker that dies
        // ([`StderrTail`]); in passthrough pytest owns the terminal.
        .stderr(match io {
            Stdio::Null => std::process::Stdio::piped(),
            Stdio::Inherit => std::process::Stdio::inherit(),
        });
    // Clear every internal variable first so only what this run sets reaches
    // the worker: a value inherited from the caller's shell, CI, or an outer
    // rstest (a test that itself runs rstest) must not leak in.
    for key in INTERNAL_ENV {
        command.env_remove(key);
    }
    scrub_secrets(&mut command);
    if worker.is_none() {
        // Outside a pool there is no xdist identity; the pool sets its own
        // (rstest_worker assigns these per worker).
        command
            .env_remove("PYTEST_XDIST_WORKER")
            .env_remove("PYTEST_XDIST_WORKER_COUNT")
            .env_remove("PYTEST_XDIST_TESTRUNUID");
    }
    if env.doctor {
        command.env("RSTEST_DOCTOR", "1");
    }
    if let Some(secs) = env.timeout {
        command.env("RSTEST_TIMEOUT", secs.to_string());
    }
    if env.leakcheck {
        command.env("RSTEST_LEAKCHECK", "1");
    }
    if env.warm.is_some() {
        command.env("RSTEST_WARM_RECORD", "1");
    }
    if env.dead_master_scan {
        command.env("RSTEST_DEAD_MASTER_SCAN", "1");
    }
    if let Some(port) = &env.debug_port {
        command.env("RSTEST_DEBUGPY_PORT", port);
    }
    if env.stream_output {
        command.env("RSTEST_STREAM_OUTPUT", "1");
    }
    if let Some(path) = &env.junitxml {
        command.env("RSTEST_JUNITXML", path);
    }
    apply_maxfail_env(&mut command, env);
    apply_network_env(&mut command, env);
    // Exactly one worker ships the full id list (D5); the rest verify their
    // collection by count+hash. Worker 0 in a pool; the lone worker only
    // when the caller asks (collect-only discovery / migrate-check).
    let send_ids = match worker {
        Some((idx, _)) => idx == 0,
        None => env.send_ids,
    };
    command.env("RSTEST_SEND_IDS", if send_ids { "1" } else { "0" });
    if let Some((idx, count)) = worker {
        command
            .env("RSTEST_WORKER_ID", format!("gw{idx}"))
            .env("RSTEST_WORKER_COUNT", count.to_string())
            // Workers get disjoint tmp roots (xdist popen-gwN pattern):
            // pytest's numbered-dir cleanup races when siblings share
            // a basetemp parent.
            .env("RSTEST_BASETEMP", crate::scratch::dir());
    }
    command
}

/// Set the run-wide worker environment shared by every forked child on the
/// zygote's [`Command`]. Per-worker identity (`RSTEST_WORKER_ID`,
/// `RSTEST_SEND_IDS`) is intentionally omitted: all children share this one
/// environment, so each applies its own identity by index post-fork.
#[cfg(unix)]
pub(super) fn apply_shared_worker_env(command: &mut Command, n: usize, env: &WorkerEnv) {
    scrub_secrets(command);
    command
        .env("PYTHONPATH", worker_pythonpath())
        .env(ORCHESTRATOR_VERSION_ENV, env!("CARGO_PKG_VERSION"))
        .env("RSTEST_RUN_UID", &env.run_uid)
        .env("RSTEST_WORKER_COUNT", n.to_string())
        .env("RSTEST_BASETEMP", crate::scratch::dir());
    if env.doctor {
        command.env("RSTEST_DOCTOR", "1");
    }
    if let Some(secs) = env.timeout {
        command.env("RSTEST_TIMEOUT", secs.to_string());
    }
    if env.leakcheck {
        command.env("RSTEST_LEAKCHECK", "1");
    }
    if let Some(warm) = &env.warm {
        command
            .env("RSTEST_WARM_RECORD", "1")
            .env("RSTEST_WARM_RESULT", &warm.result);
        if let Some(manifest) = &warm.manifest {
            command.env("RSTEST_WARM_MANIFEST", manifest);
        }
    }
    if env.dead_master_scan {
        command.env("RSTEST_DEAD_MASTER_SCAN", "1");
    }
    if env.stream_output {
        command.env("RSTEST_STREAM_OUTPUT", "1");
    }
    if let Some(path) = &env.junitxml {
        command.env("RSTEST_JUNITXML", path);
    }
    apply_maxfail_env(command, env);
    apply_network_env(command, env);
}

/// What a session's own `-x` / `--maxfail` count must skip (see
/// `rstest_worker._internal.retry.MaxfailExemptions`): every failed attempt
/// when a global `--reruns` budget may retry it, and quarantined tests.
/// Unset ones are removed, so an outer rstest's values never leak in (the
/// fork-pool zygote does not clear [`INTERNAL_ENV`]).
pub(super) fn apply_maxfail_env(command: &mut Command, env: &WorkerEnv) {
    if env.reruns {
        command.env("RSTEST_RERUNS", "1");
    } else {
        command.env_remove("RSTEST_RERUNS");
    }
    match &env.quarantine {
        Some(q) => command
            .env("RSTEST_QUARANTINE", &q.file)
            .env("RSTEST_AUTO_QUARANTINE", &q.auto)
            .env("RSTEST_AUTO_QUARANTINE_MAX", q.max_pct.to_string()),
        None => command
            .env_remove("RSTEST_QUARANTINE")
            .env_remove("RSTEST_AUTO_QUARANTINE")
            .env_remove("RSTEST_AUTO_QUARANTINE_MAX"),
    };
}

/// Locate the rstest_worker package. Dev layout: exe sits in target/<profile>/,
/// package in <repo>/python/. Installed wheels ship the package inside
/// site-packages instead, making this a no-op.
pub fn worker_pythonpath() -> String {
    build_pythonpath(
        std::env::var("RSTEST_WORKER_PATH").ok().as_deref(),
        std::env::var("PYTHONPATH").ok().as_deref(),
    )
}

/// Pure join logic behind [`worker_pythonpath`], split out so tests can exercise
/// the explicit-override + PYTHONPATH ordering without mutating process-global
/// env (`set_var` races other tests and is `unsafe` in edition 2024). The
/// `current_exe`-derived dev path is read-only, so it stays inline.
pub(super) fn build_pythonpath(
    explicit: Option<&str>,
    existing_pythonpath: Option<&str>,
) -> String {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = explicit {
        paths.push(PathBuf::from(explicit));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(repo) = exe.ancestors().nth(3) {
            let dev = repo.join("python");
            if dev.join("rstest_worker").is_dir() {
                paths.push(dev);
            }
        }
    }
    if let Some(existing) = existing_pythonpath {
        for p in std::env::split_paths(existing) {
            paths.push(p);
        }
    }
    std::env::join_paths(paths)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}
