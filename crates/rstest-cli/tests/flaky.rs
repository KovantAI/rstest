//! End-to-end tests for `rstest flaky`: a hand-built `history.msgpack` in a
//! temp project, read by the real binary. Interpreter-free, so `PATH` is
//! cleared like `explain`'s tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

fn project(tag: &str, pyproject: Option<&str>) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-flaky-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".rstest_cache")).unwrap();
    if let Some(toml) = pyproject {
        std::fs::write(d.join("pyproject.toml"), toml).unwrap();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Positional encoding (see tests/explain.rs): runs = [seq, epoch, commit],
    // entries = [run, outcome, call, setup], outcome 0 = passed, 1 = failed.
    let runs: Vec<Value> = (0..4u64).map(|s| json!([s, now, "c0"])).collect();
    let strip = |o: [u8; 4]| -> Vec<Value> {
        (0..4u64)
            .map(|s| json!([s, o[s as usize], 0.1, 0.0]))
            .collect()
    };
    let doc = json!([
        1,
        runs,
        { "t.py::flapper": strip([0, 1, 0, 1]), "t.py::green": strip([0; 4]) },
    ]);
    std::fs::write(
        d.join(".rstest_cache").join("history.msgpack"),
        rmp_serde::to_vec(&doc).unwrap(),
    )
    .unwrap();
    d
}

fn flaky(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rstest"))
        .arg("flaky")
        .args(args)
        .current_dir(dir)
        .env_remove("VIRTUAL_ENV")
        .env_remove("RSTEST_CACHE")
        .env_remove("RSTEST_FLAKE_RETENTION_DAYS")
        .env("PATH", "")
        .env("NO_COLOR", "1")
        .output()
        .expect("run rstest");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "stdout={stdout}");
    stdout
}

#[test]
fn json_reads_history_and_config_threshold() {
    let dir = project("json", Some("[tool.rstest]\nauto-quarantine = 2\n"));
    let v: Value = serde_json::from_str(&flaky(&dir, &["--json"])).unwrap();
    assert_eq!(v["auto_quarantine"], 2);
    assert_eq!(
        (v["tests_in_history"].as_u64(), v["flaky_tests"].as_u64()),
        (Some(2), Some(1))
    );
    assert_eq!(v["tests"][0]["nodeid"], "t.py::flapper");
    assert_eq!(v["tests"][0]["flips"], 2);
    assert_eq!(v["tests"][0]["auto_quarantined"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn table_uses_the_default_threshold() {
    let dir = project("table", None);
    let out = flaky(&dir, &[]);
    assert!(
        out.starts_with("1 flaky test(s) of 2 in the history (auto-quarantine at 3+ flips)"),
        "{out}"
    );
    assert!(!out.contains("[auto-quarantined]"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
