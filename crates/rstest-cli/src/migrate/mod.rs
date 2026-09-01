//! `--migrate-check` and `--try`: the pytest→rstest onboarding preflights.
//!
//! This file owns the run-snapshot model (`Outcomes`/`Rec`/`Phase`) and the
//! child-session runner shared by both commands. [`classify`] owns the two
//! classifiers (unstable ids, parallel-only failures); [`check`] is the
//! `--migrate-check` orchestrator; [`try_cmd`] is the `--try` parity+speed run.

mod check;
mod classify;
mod try_cmd;

pub use check::run_migrate_check;
pub use try_cmd::run_try;

use std::path::Path;

use anyhow::Result;

use crate::scheduling::{proto, worker};

/// Per-test record from a run snapshot: pass/fail plus timing (for the
/// wait-bound / wall-clock signal). A test absent from a run isn't in the map.
pub(super) type Outcomes = std::collections::BTreeMap<String, Rec>;

#[derive(Clone, Copy)]
pub(super) struct Rec {
    pub phase: Phase,
    pub wall: f64,        // call-phase wall seconds
    pub cpu: Option<f64>, // call-phase cpu seconds (only with doctor instrumentation)
}

impl Rec {
    /// Wait-bound: spent its time blocked, not computing - the signature of a
    /// wall-clock/timeout test. Needs cpu data (doctor) and a non-trivial wall.
    pub(super) fn wait_bound(&self) -> bool {
        matches!(self.cpu, Some(c) if self.wall >= 0.05 && c < 0.5 * self.wall)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Pass,
    Fail, // failed or errored in any phase
}

pub(super) fn is_fail(entry: &serde_json::Value) -> bool {
    ["setup", "call", "teardown"].iter().any(|p| {
        matches!(
            entry.get(p).and_then(|v| v.as_str()),
            Some("failed") | Some("error")
        )
    })
}

/// The test file of a nodeid (everything before the first `::`).
pub(super) fn file_of(nodeid: &str) -> &str {
    nodeid.split("::").next().unwrap_or(nodeid)
}

/// Run one full session in a child rstest process with the given config flags
/// (e.g. `["-n","0"]`), capture per-test pass/fail from its `--report-json`.
pub(super) fn run_session(config: &[&str], args: &[String]) -> Result<Outcomes> {
    let exe = std::env::current_exe()?;
    let tmp = std::env::temp_dir().join(format!(
        "rstest-migrate-{}-{}.json",
        std::process::id(),
        run_session_seq()
    ));
    let mut cmd = std::process::Command::new(exe);
    cmd.args(config)
        .args(args)
        .arg("--report-json")
        .arg(&tmp)
        // worker-timeout: a fixed-port / deadlock test (httpx, werkzeug) would
        // otherwise hang the preflight; the stuck test becomes a failure.
        .args(["--worker-timeout", "120"])
        // dots off-tty keeps the child quiet & byte-stable; we discard stdout.
        .args(["-q", "--output", "dots"])
        // doctor instrumentation adds per-test cpu time (cheap) so the
        // classifier can tell a wait-bound (wall-clock) failure from a real
        // co-location/isolation one.
        .env("RSTEST_DOCTOR", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.status()?; // non-zero is expected when tests fail; the snapshot is truth
    let mut out = Outcomes::new();
    if let Ok(text) = std::fs::read_to_string(&tmp) {
        if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(tests) = doc.get("tests").and_then(|t| t.as_object()) {
                for (nodeid, entry) in tests {
                    out.insert(
                        nodeid.clone(),
                        Rec {
                            phase: if is_fail(entry) {
                                Phase::Fail
                            } else {
                                Phase::Pass
                            },
                            wall: entry
                                .get("duration")
                                .and_then(|v| v.as_f64())
                                .unwrap_or(0.0),
                            cpu: entry.get("cpu").and_then(|v| v.as_f64()),
                        },
                    );
                }
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Ok(out)
}

fn run_session_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// One fresh collect-only session -> the collected nodeids in session order.
pub(super) fn collect_ids(python: &Path, args: &[String]) -> Result<Vec<String>> {
    // Full id+location payload from pytest_collection_finish (single session).
    std::env::set_var("RSTEST_SEND_IDS", "1");
    let mut collect_args = args.to_vec();
    if !collect_args
        .iter()
        .any(|a| a == "--collect-only" || a == "--co")
    {
        collect_args.push("--collect-only".into());
    }
    let mut w = worker::Worker::spawn_with_io(python, None, worker::Stdio::Null)?;
    w.send(&proto::Command::RunItemsSession { args: collect_args })?;
    let mut ids: Vec<String> = Vec::new();
    loop {
        match w.recv()? {
            proto::Event::CollectionDone { ids: Some(i), .. } => ids = i,
            proto::Event::Done { .. } => break,
            _ => {}
        }
    }
    w.shutdown()?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_of_strips_at_first_colons() {
        assert_eq!(
            file_of("tests/test_x.py::TestC::test_m[param]"),
            "tests/test_x.py"
        );
        assert_eq!(file_of("tests/test_x.py"), "tests/test_x.py");
    }

    #[test]
    fn wait_bound_signal() {
        // 1.0s wall, ~0 cpu -> waiting (wall-clock test).
        let waiting = Rec {
            phase: Phase::Fail,
            wall: 1.0,
            cpu: Some(0.01),
        };
        assert!(waiting.wait_bound());
        // cpu-bound: most of the wall is compute.
        let computing = Rec {
            phase: Phase::Fail,
            wall: 1.0,
            cpu: Some(0.9),
        };
        assert!(!computing.wait_bound());
        // no cpu data (doctor off) -> can't claim wait-bound.
        let no_cpu = Rec {
            phase: Phase::Fail,
            wall: 1.0,
            cpu: None,
        };
        assert!(!no_cpu.wait_bound());
        // trivially short -> not meaningful, don't flag.
        let quick = Rec {
            phase: Phase::Fail,
            wall: 0.001,
            cpu: Some(0.0),
        };
        assert!(!quick.wait_bound());
    }
}
