//! End-to-end tests for `rstest replay`. They build a tiny suite in a temp dir,
//! run the real binary under the parallel pool to journal a schedule, then
//! replay it and assert the journal shape + replay behavior.
//!
//! These need a python with pytest and the rstest worker. They skip cleanly
//! otherwise. To run them:
//!
//! - CI: have `python3` on PATH with pytest importable, or
//! - local: `RSTEST_TEST_VENV=/path/to/venv cargo test -p rstest-cli --test replay`

use std::path::{Path, PathBuf};
use std::process::Command;

/// A venv dir to expose as VIRTUAL_ENV, or None to use the ambient PATH python3.
/// Returns None to SKIP if no pytest is reachable.
fn pytest_env() -> Option<Option<PathBuf>> {
    if let Ok(venv) = std::env::var("RSTEST_TEST_VENV") {
        let py = Path::new(&venv).join("bin").join("python");
        if import_worker(&py) {
            return Some(Some(PathBuf::from(venv)));
        }
        return None;
    }
    if import_worker(Path::new("python3")) {
        return Some(None);
    }
    None
}

/// The replay pool needs the rstest worker too (not just pytest), since it
/// dispatches items into worker sessions.
fn import_worker(py: &Path) -> bool {
    Command::new(py)
        .args(["-c", "import pytest, rstest_worker"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-replay-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(venv: &Option<PathBuf>, dir: &Path, extra: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rstest"));
    cmd.args(extra).current_dir(dir);
    if let Some(v) = venv {
        let bin = v.join("bin");
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("VIRTUAL_ENV", v)
            .env("PATH", format!("{}:{}", bin.display(), path));
    }
    let out = cmd.output().expect("run rstest");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

fn write_suite(dir: &Path) {
    std::fs::write(
        dir.join("test_a.py"),
        "def test_a1():\n    assert True\n\ndef test_a2():\n    assert True\n\ndef test_a3():\n    assert True\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("test_b.py"),
        "def test_b1():\n    assert True\n\ndef test_b2():\n    assert True\n\ndef test_b3():\n    assert True\n",
    )
    .unwrap();
}

#[test]
fn pool_run_journals_then_replays_green() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("green");
    write_suite(&dir);

    // Record: a pool run writes a journal.
    let (code, out) = run(&venv, &dir, &["-n", "2"]);
    assert_eq!(code, 0, "clean suite should pass\n{out}");

    let latest = dir.join(".rstest_cache").join("replay").join("latest.json");
    let txt = std::fs::read_to_string(&latest).expect("journal written");
    // Shape: schema-stamped, right worker count, all 6 tests journaled once,
    // keyed by nodeid.
    let j: serde_json::Value = serde_json::from_str(&txt).unwrap();
    assert_eq!(j["schema"], 1, "journal schema\n{txt}");
    assert_eq!(j["workers"], 2, "recorded worker count\n{txt}");
    assert_eq!(j["collection_size"], 6, "all tests collected\n{txt}");
    let assigned: usize = j["assignment"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_array().unwrap().len())
        .sum();
    assert_eq!(
        assigned, 6,
        "every test assigned to exactly one worker\n{txt}"
    );

    // Replay latest: same suite, pinned schedule, still green.
    let (rcode, rout) = run(&venv, &dir, &["replay"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(rcode, 0, "replay of a green run should be green\n{rout}");
    assert!(
        rout.contains("replay: run"),
        "expected the replay banner\n{rout}"
    );
    assert!(rout.contains("6 passed"), "all tests replayed\n{rout}");
}

#[test]
fn replay_preserves_a_failure() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("fail");
    write_suite(&dir);
    std::fs::write(dir.join("test_c.py"), "def test_c():\n    assert False\n").unwrap();

    let (code, _) = run(&venv, &dir, &["-n", "2"]);
    assert_eq!(code, 1, "a failing test fails the record run");

    let (rcode, rout) = run(&venv, &dir, &["replay"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(rcode, 1, "replay preserves the failure exit code\n{rout}");
}

#[test]
fn replay_reports_drift_when_a_test_is_removed() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("drift");
    write_suite(&dir);
    let (code, _) = run(&venv, &dir, &["-n", "2"]);
    assert_eq!(code, 0);

    // Remove a whole file after journaling: replay must note the drift + the
    // missing tests, and still run the survivors green.
    std::fs::remove_file(dir.join("test_b.py")).unwrap();
    let (rcode, rout) = run(&venv, &dir, &["replay"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(rcode, 0, "survivors still pass\n{rout}");
    assert!(
        rout.contains("suite changed since the journal"),
        "expected a drift note\n{rout}"
    );
    assert!(
        rout.contains("no longer collected"),
        "expected a missing-tests note\n{rout}"
    );
    assert!(rout.contains("3 passed"), "the 3 survivors ran\n{rout}");
}

#[test]
fn opt_out_writes_no_journal() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("optout");
    write_suite(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rstest"));
    cmd.args(["-n", "2"]).current_dir(&dir);
    cmd.env("RSTEST_NO_REPLAY_JOURNAL", "1");
    if let Some(v) = &venv {
        let bin = v.join("bin");
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("VIRTUAL_ENV", v)
            .env("PATH", format!("{}:{}", bin.display(), path));
    }
    let status = cmd.status().expect("run rstest");
    assert!(status.success());
    let replay_dir = dir.join(".rstest_cache").join("replay");
    let existed = replay_dir.exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!existed, "opt-out must write no journal");
}
