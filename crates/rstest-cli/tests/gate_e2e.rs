//! End-to-end cache round-trip gate — the "full tier" of the mutation-testing
//! plan (see `mutation-testing-plan.md`).
//!
//! `cargo test` alone never executes the cache IO paths (`durations::{load,
//! save,cache_path}`, `flakes::{record,path,now,load}`), so mutating those
//! whole bodies to a stub survives the unit suite. These tests drive the REAL
//! built binary (`CARGO_BIN_EXE_rstest`, which cargo-mutants rebuilds per
//! mutant) across two invocations against a throwaway suite and assert the
//! `.rstest_cache/*.json` artifacts — the only place those functions are
//! observable — killing the stub mutants.
//!
//! Gated behind `RSTEST_RUN_GATE=1` so an ordinary `cargo test` (no worker
//! venv) skips them; the CI mutation job and `mutants.toml` set the flag and
//! provide a prebuilt venv. Override the venv with `RSTEST_GATE_VENV`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_rstest");

fn repo_root() -> PathBuf {
    // <repo>/crates/rstest-cli -> <repo>
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repo root")
}

/// The worker venv. Prebuilt once (pip is too slow to build per mutant);
/// `RSTEST_GATE_VENV` overrides, else the repo's default `.gate-venv`.
fn venv() -> PathBuf {
    std::env::var_os("RSTEST_GATE_VENV")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join(".gate-venv"))
}

/// True unless the gate is explicitly enabled. Skipping keeps a plain
/// `cargo test` (which has no worker venv) green.
fn skip() -> bool {
    std::env::var("RSTEST_RUN_GATE").as_deref() != Ok("1")
}

fn workdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-gate-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mk workdir");
    d
}

fn write(dir: &Path, rel: &str, content: &str) {
    std::fs::write(dir.join(rel), content).expect("write fixture");
}

/// Run the built binary in `dir` with the worker env gate.py uses.
fn run(dir: &Path, args: &[&str]) -> Output {
    let v = venv();
    assert!(
        v.join("bin").join("python").exists() || v.join("Scripts").join("python.exe").exists(),
        "worker venv missing at {} — build it (e.g. run tests/gate.py once) or set RSTEST_GATE_VENV",
        v.display()
    );
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .env("VIRTUAL_ENV", &v)
        .env("RSTEST_WORKER_PATH", repo_root().join("python"))
        .output()
        .expect("spawn rstest")
}

fn read_json(dir: &Path, rel: &str) -> serde_json::Value {
    let p = dir.join(rel);
    let bytes = std::fs::read(&p).unwrap_or_else(|_| panic!("expected {} to exist", p.display()));
    serde_json::from_slice(&bytes).expect("valid json cache")
}

#[test]
fn durations_cache_persists_and_merges_across_runs() {
    if skip() {
        return;
    }
    let dir = workdir("durations");
    write(&dir, "test_a.py", "def test_a():\n    assert True\n");
    write(&dir, "test_b.py", "def test_b():\n    assert True\n");

    // Run 1: full suite -> save() must write the cache (kills `save -> ()` and
    // `cache_path -> Default` which would leave no file at all).
    let r = run(&dir, &[".", "-n", "2"]);
    assert!(
        r.status.success(),
        "run1 failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let after1 = read_json(&dir, ".rstest_cache/durations.json");
    assert!(
        after1.get("test_a.py::test_a").is_some(),
        "run1 must cache test_a"
    );
    assert!(
        after1.get("test_b.py::test_b").is_some(),
        "run1 must cache test_b"
    );

    // Run 2: filtered to test_b -> save() reloads and MERGES, so test_a's
    // timing survives even though it did not run. `load -> HashMap::new()` (or
    // any fixed-map stub) would drop test_a / inject a bogus key here.
    let r = run(&dir, &["test_b.py", "-n", "1"]);
    assert!(
        r.status.success(),
        "run2 failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let after2 = read_json(&dir, ".rstest_cache/durations.json");
    let map: HashMap<String, f64> = serde_json::from_value(after2).expect("durations map");
    assert!(
        map.contains_key("test_a.py::test_a"),
        "merge must keep the unrun test_a"
    );
    assert!(map.contains_key("test_b.py::test_b"));
    assert!(
        !map.contains_key("xyzzy"),
        "no synthetic key from a stubbed load()"
    );
}

const FLAKY: &str = "\
import os
_p = os.path.join(os.path.dirname(__file__), '.fc')
def test_flaky():
    n = int(open(_p).read()) if os.path.exists(_p) else 0
    open(_p, 'w').write(str(n + 1))
    assert n >= 1  # fails on first attempt, passes on rerun
";

fn reset_counter(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(".fc"));
}

#[test]
fn flake_history_recorded_with_real_epoch() {
    if skip() {
        return;
    }
    let dir = workdir("flake-record");
    write(&dir, "test_flaky.py", FLAKY);
    reset_counter(&dir);

    // A flake (fail-then-pass under --reruns) -> record() must write the log
    // with the flaky counter and a real clock. Kills `record -> ()`,
    // `flakes::path -> Default` (no file) and `now -> 0` (epoch would be 0).
    let r = run(&dir, &["test_flaky.py", "--reruns", "1", "-n", "1"]);
    assert!(
        r.status.success(),
        "flaky run failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    let log = read_json(&dir, ".rstest_cache/flakes.json");
    let e = &log["test_flaky.py::test_flaky"];
    assert_eq!(e["flaky"].as_u64(), Some(1), "one flaky run recorded");
    assert!(
        e["last_epoch"].as_u64().unwrap_or(0) > 0,
        "last_epoch must be a real clock reading, not 0"
    );
}

#[test]
fn flake_counts_accumulate_across_runs() {
    if skip() {
        return;
    }
    let dir = workdir("flake-accumulate");
    write(&dir, "test_flaky.py", FLAKY);

    // Two separate flaky runs. record() reloads prior history and increments,
    // so the count reaches 2. `flakes::load -> HashMap::new()` would forget the
    // first run and leave flaky == 1.
    for _ in 0..2 {
        reset_counter(&dir);
        let r = run(&dir, &["test_flaky.py", "--reruns", "1", "-n", "1"]);
        assert!(
            r.status.success(),
            "flaky run failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
    }

    let log = read_json(&dir, ".rstest_cache/flakes.json");
    assert_eq!(
        log["test_flaky.py::test_flaky"]["flaky"].as_u64(),
        Some(2),
        "flake counter must accumulate over stored history"
    );
}

#[test]
fn only_known_flaky_history_gets_reruns() {
    if skip() {
        return;
    }
    let dir = workdir("known-flaky");
    write(&dir, "test_flaky.py", FLAKY);

    // Run 1 records the flake into history.
    reset_counter(&dir);
    let r = run(&dir, &["test_flaky.py", "--reruns", "1", "-n", "1"]);
    assert!(
        r.status.success(),
        "seed run failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    // Run 2 reruns ONLY known-flaky tests. The test is in history, so
    // known_flaky() must surface it -> it is retried -> passes. A stubbed
    // `known_flaky -> {}` / fixed-set would leave it unrecognized, so it would
    // fail its first (n=0) attempt with no rerun.
    reset_counter(&dir);
    let r = run(
        &dir,
        &[
            "test_flaky.py",
            "--reruns",
            "1",
            "--reruns-only-known-flaky",
            "-n",
            "1",
        ],
    );
    assert!(
        r.status.success(),
        "a test with flaky history must be recognized and reran: {}",
        String::from_utf8_lossy(&r.stderr)
    );
}

#[test]
fn stale_flake_history_ages_out_of_the_known_set() {
    if skip() {
        return;
    }
    let dir = workdir("flake-retention");
    write(&dir, "test_flaky.py", FLAKY);
    // Seed an entry whose last event is epoch 1 (1970): far older than the
    // 90-day retention window, so load() must drop it -> the test is NOT known.
    std::fs::create_dir_all(dir.join(".rstest_cache")).unwrap();
    write(
        &dir,
        ".rstest_cache/flakes.json",
        r#"{"test_flaky.py::test_flaky":{"flaky":5,"failed":0,"last_epoch":1}}"#,
    );

    // --reruns-only-known-flaky: the aged-out test is unrecognized, so it is
    // not retried and fails its first attempt. A stubbed `now -> 1` or
    // `retention_secs -> 0` (aging disabled) would keep the stale entry, making
    // it "known", retried, and passing -> this assertion catches both.
    reset_counter(&dir);
    let r = run(
        &dir,
        &[
            "test_flaky.py",
            "--reruns",
            "1",
            "--reruns-only-known-flaky",
            "-n",
            "1",
        ],
    );
    assert!(
        !r.status.success(),
        "a flake older than the retention window must not count as known"
    );
}
