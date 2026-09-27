//! End-to-end tests for `rstest audit`. Each builds a tiny fixture suite in a
//! temp dir and runs the real binary against it, asserting the exit code (the
//! CI gate), the human report, and the `--audit-json` doc.
//!
//! These need a python with pytest. They skip cleanly otherwise. To run them:
//!
//! - CI: have `python3` on PATH with pytest importable, or
//! - local: `RSTEST_TEST_VENV=/path/to/venv cargo test -p rstest-cli --test audit`

use std::path::{Path, PathBuf};
use std::process::Command;

/// A venv dir to expose as VIRTUAL_ENV, or None to use the ambient PATH
/// python3. Returns None to SKIP if no pytest is reachable.
fn pytest_env() -> Option<Option<PathBuf>> {
    if let Ok(venv) = std::env::var("RSTEST_TEST_VENV") {
        let py = Path::new(&venv).join("bin").join("python");
        if import_pytest(&py) {
            return Some(Some(PathBuf::from(venv)));
        }
        return None;
    }
    if import_pytest(Path::new("python3")) {
        return Some(None);
    }
    None
}

fn import_pytest(py: &Path) -> bool {
    Command::new(py)
        .args(["-c", "import pytest"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-audit-test-{tag}-{}", std::process::id()));
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

fn read_json(path: &Path) -> serde_json::Value {
    let txt = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str(&txt).unwrap_or_else(|e| panic!("bad audit json ({e}):\n{txt}"))
}

#[test]
fn clean_suite_is_parallel_safe() {
    // Two files so -n auto really fans out; repeat 2 drives the merge loop.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("clean");
    for i in 0..2 {
        std::fs::write(
            dir.join(format!("test_clean_{i}.py")),
            "def test_a():\n    assert True\n\ndef test_b():\n    assert True\n",
        )
        .unwrap();
    }
    let jpath = dir.join("audit.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "audit",
            "--audit-repeat",
            "2",
            "--audit-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "clean suite should pass the audit\n{out}");
    assert!(out.contains("running -n auto 2×"), "{out}");
    assert!(out.contains("parallel-safe: all 4 tests"), "{out}");
    assert!(!out.contains("pre-existing"), "{out}");
    assert_eq!(doc["meta"]["kind"], "audit");
    assert_eq!(doc["parallel_safe"], true);
    assert_eq!(doc["tests"], 4);
    assert_eq!(doc["serial_candidates"].as_array().unwrap().len(), 0);
    assert_eq!(doc["preexisting_failures"], 0);
}

#[test]
fn preexisting_failure_is_not_a_parallel_finding() {
    // Fails at -n 0 too: NOT PARALLEL-SPECIFIC, so the gate still passes and
    // the clean summary notes it as pre-existing.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("pre");
    std::fs::write(
        dir.join("test_pre.py"),
        "def test_ok():\n    assert True\n\ndef test_bad():\n    assert False\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["audit"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        code, 0,
        "a pre-existing failure should not fail audit\n{out}"
    );
    assert!(out.contains("parallel-safe"), "{out}");
    assert!(
        out.contains("1 test(s) already fail at -n 0"),
        "expected the pre-existing note:\n{out}"
    );
}

/// Fails only inside a multi-worker pool: `RSTEST_WORKER_ID` is unset at
/// `-n 0` (and `-n 1`), so the serial oracle passes deterministically.
const WORKER_ONLY: &str = "\
import os

def test_needs_serial():
    assert 'RSTEST_WORKER_ID' not in os.environ
";

#[test]
fn parallel_only_failure_gets_a_serial_fix_list() {
    // Several failing files: -n auto and the scoped loadfile run both use a
    // multi-worker pool, so every test passes serially and fails otherwise.
    // Plus one deterministic failure to drive the trailing pre-existing note.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("serial");
    for i in 0..3 {
        std::fs::write(dir.join(format!("test_iso_{i}.py")), WORKER_ONLY).unwrap();
    }
    std::fs::write(
        dir.join("test_bad.py"),
        "def test_bad():\n    assert False\n",
    )
    .unwrap();
    let jpath = dir.join("audit.json");
    let (code, out) = run(
        &venv,
        &dir,
        &["audit", "--audit-json", jpath.to_str().unwrap()],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    // A box that can't run two workers never exposes the failure; only assert
    // the report when it surfaced.
    if !out.contains("fail under -n auto") {
        return;
    }
    assert_eq!(code, 1, "parallel-only failures should fail audit\n{out}");
    assert!(out.contains("3 test(s) pass serially"), "{out}");
    assert!(
        out.contains("test_iso_0.py::test_needs_serial"),
        "each serial candidate should be listed:\n{out}"
    );
    assert!(out.contains("_RSTEST_SERIAL = {"), "{out}");
    assert!(out.contains("item.add_marker(pytest.mark.serial)"), "{out}");
    assert!(out.contains("Serial is a STOPGAP"), "{out}");
    assert!(
        out.contains("1 test(s) already fail at -n 0") && out.contains("not listed"),
        "expected the trailing pre-existing note:\n{out}"
    );
    assert!(!out.contains("INTRINSIC FLAKES"), "{out}");

    assert_eq!(doc["parallel_safe"], false);
    assert_eq!(doc["preexisting_failures"], 1);
    let cands = doc["serial_candidates"].as_array().unwrap();
    assert_eq!(cands.len(), 3, "{doc}");
    // Sorted by nodeid.
    assert_eq!(cands[0]["nodeid"], "test_iso_0.py::test_needs_serial");
    assert!(cands[0]["fix"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(doc["serial_conftest"]
        .as_str()
        .unwrap()
        .contains("test_iso_2.py::test_needs_serial"));
}

/// A deterministic "flake": each test keeps a per-test run counter next to
/// itself and fails on odd runs. Audit runs -n auto (1st, fail), then the
/// serial oracle twice (2nd pass, 3rd fail), so the serial repeats disagree:
/// INTRINSIC FLAKE.
fn flaky_test(i: usize) -> String {
    format!(
        "import pathlib\n\
         \n\
         def test_flip_{i}():\n\
         \x20   p = pathlib.Path(__file__).with_suffix('.count')\n\
         \x20   n = int(p.read_text()) + 1 if p.exists() else 1\n\
         \x20   p.write_text(str(n))\n\
         \x20   assert n % 2 == 0\n"
    )
}

#[test]
fn intrinsic_flakes_are_listed_but_not_serial_fixable() {
    // Nine flakes: more than the eight printed, so the "… and N more" tail runs.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("flake");
    for i in 0..9 {
        std::fs::write(dir.join(format!("test_flip_{i}.py")), flaky_test(i)).unwrap();
    }
    let jpath = dir.join("audit.json");
    let (code, out) = run(
        &venv,
        &dir,
        &["audit", "--audit-json", jpath.to_str().unwrap()],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "intrinsic flakes should fail audit\n{out}");
    assert!(
        out.contains("9 test(s) are INTRINSIC FLAKES"),
        "expected the intrinsic-flake section:\n{out}"
    );
    assert!(out.contains("test_flip_0.py::test_flip_0"), "{out}");
    assert!(out.contains("… and 1 more"), "{out}");
    // Nothing is serial-fixable, so no fix-list is printed.
    assert!(!out.contains("_RSTEST_SERIAL"), "{out}");
    assert_eq!(doc["parallel_safe"], false);
    assert_eq!(doc["serial_candidates"].as_array().unwrap().len(), 0);
    assert_eq!(doc["intrinsic_flakes"].as_array().unwrap().len(), 9);
}

#[test]
fn refused_run_exits_two() {
    // A uuid parametrize id is unstable per collection, so rstest refuses to
    // dispatch under -n auto and writes no snapshot: nothing to diff.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("uuid");
    for i in 0..2 {
        std::fs::write(
            dir.join(format!("test_uuid_{i}.py")),
            "import uuid, pytest\n\
             @pytest.mark.parametrize('u', [str(uuid.uuid4())])\n\
             def test_u(u):\n    assert u\n",
        )
        .unwrap();
    }
    let (code, out) = run(&venv, &dir, &["audit"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 2, "a refused run should exit 2\n{out}");
    assert!(out.contains("produced no run"), "{out}");
    assert!(out.contains("rstest migrate-check"), "{out}");
}

#[test]
fn empty_selection_is_clean_and_writes_json() {
    // A -k that matches nothing: nothing selected is nothing unsafe (exit 0),
    // but the report says so and the json records a run over zero tests.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("empty");
    std::fs::write(dir.join("test_one.py"), "def test_a():\n    assert True\n").unwrap();
    let jpath = dir.join("audit.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "audit",
            "--audit-json",
            jpath.to_str().unwrap(),
            "-k",
            "no_such_test",
        ],
    );
    let doc = read_json(&jpath);
    // Without --audit-json the report is the same and nothing is written.
    std::fs::remove_file(&jpath).unwrap();
    let (plain_code, plain_out) = run(&venv, &dir, &["audit", "-k", "no_such_test"]);
    let wrote = jpath.exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "an empty selection should not fail audit\n{out}");
    assert_eq!(plain_code, 0, "{plain_out}");
    assert!(plain_out.contains("no tests were selected"), "{plain_out}");
    assert!(!wrote);
    assert!(out.contains("no tests were selected"), "{out}");
    assert_eq!(doc["ran"], true);
    assert_eq!(doc["parallel_safe"], true);
    assert_eq!(doc["tests"], 0);
}

#[test]
fn test_missing_from_serial_run_is_inconclusive() {
    // Defined only inside a worker pool, so it fails under -n auto but never
    // collects at -n 0: no serial evidence, so INCONCLUSIVE (and it fails the
    // gate) rather than being read as a serial pass.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("ghost");
    std::fs::write(
        dir.join("test_ghost.py"),
        "import os\n\
         \n\
         if 'RSTEST_WORKER_ID' in os.environ:\n\
         \x20   def test_ghost():\n\
         \x20       assert False\n\
         \n\
         def test_ok():\n\
         \x20   assert True\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("test_other.py"),
        "def test_x():\n    assert True\n",
    )
    .unwrap();
    let jpath = dir.join("audit.json");
    let (code, out) = run(
        &venv,
        &dir,
        &["audit", "--audit-json", jpath.to_str().unwrap()],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    // A box that can't run two workers never defines the test.
    if !out.contains("parallel failure(s)") {
        return;
    }
    assert_eq!(code, 1, "an inconclusive test should fail audit\n{out}");
    assert!(out.contains("1 test(s) are INCONCLUSIVE"), "{out}");
    assert!(out.contains("test_ghost.py::test_ghost"), "{out}");
    assert!(!out.contains("_RSTEST_SERIAL"), "{out}");
    assert_eq!(doc["parallel_safe"], false);
    assert_eq!(doc["inconclusive"][0], "test_ghost.py::test_ghost");
    assert_eq!(doc["serial_candidates"].as_array().unwrap().len(), 0);
}
