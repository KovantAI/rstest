//! End-to-end test for memory-aware `-n auto`: a parallel run records the
//! workers' peak RSS in `.rstest_cache/memory.json`, and the next `-n auto`
//! runs fewer workers when that many peaks would not fit in memory.
//!
//! Needs a python with pytest; skips cleanly otherwise (see `tests/audit.rs`).
//! Unix only: Windows reports no peak RSS and no memory limit yet.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

fn run(venv: &Option<PathBuf>, dir: &Path, extra: &[&str]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rstest"));
    cmd.args(extra).current_dir(dir);
    if let Some(v) = venv {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("VIRTUAL_ENV", v)
            .env("PATH", format!("{}:{path}", v.join("bin").display()));
    }
    let out = cmd.output().expect("run rstest");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

#[test]
fn a_recorded_peak_caps_the_next_auto_run() {
    let Some(venv) = common::pytest_env("pytest") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("rstest-memory-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 4s of tests: enough for the time-based sizing to want 2 workers, so
    // the memory cap is what brings it to 1.
    for i in 0..4 {
        std::fs::write(
            dir.join(format!("test_{i}.py")),
            "import time\n\ndef test_x():\n    time.sleep(1)\n",
        )
        .unwrap();
    }
    let out = run(&venv, &dir, &["-n", "2"]);
    let memory = dir.join(".rstest_cache/memory.json");
    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&memory).unwrap_or_default())
            .unwrap_or_else(|e| panic!("no memory.json ({e}):\n{out}"));
    // A real interpreter's peak: tens of MiB.
    let peak = doc["peaks"][0].as_u64().unwrap_or(0);
    assert!(peak > 10 << 20, "{doc}");

    // A peak no machine fits twice: the next auto run uses one worker.
    std::fs::write(&memory, format!("{{\"peaks\": [{}]}}", u64::MAX / 4)).unwrap();
    let out = run(&venv, &dir, &["-n", "auto"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.contains("rstest: -n auto capped at 1 worker by memory"),
        "{out}"
    );
    assert!(out.contains("4 passed"), "{out}");
}
