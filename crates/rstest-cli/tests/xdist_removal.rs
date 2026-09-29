//! End-to-end tests for `rstest xdist-removal-check`: a tiny project in a temp
//! dir, the real binary, asserting the exit code (the CI gate), the report and
//! the JSON document. They need a python with pytest (not pytest-xdist: the
//! scan is static) and skip cleanly otherwise. Local:
//! `RSTEST_TEST_VENV=/path/to/venv cargo test -p rstest-cli --test xdist_removal`

use std::path::{Path, PathBuf};
use std::process::Command;

fn python() -> Option<PathBuf> {
    let py = match std::env::var("RSTEST_TEST_VENV") {
        Ok(venv) => Path::new(&venv).join("bin").join("python"),
        Err(_) => PathBuf::from("python3"),
    };
    Command::new(&py)
        .args(["-c", "import pytest"])
        .status()
        .is_ok_and(|s| s.success())
        .then_some(py)
}

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-xr-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(py: &Path, dir: &Path, extra: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rstest"))
        .arg("xdist-removal-check")
        .arg("--python")
        .arg(py)
        .args(extra)
        .current_dir(dir)
        .env_remove("PYTEST_ADDOPTS")
        .output()
        .expect("run rstest");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

#[test]
fn clean_project_is_ready() {
    let Some(py) = python() else { return };
    let dir = fresh_dir("clean");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\naddopts = -q\n").unwrap();
    std::fs::write(
        dir.join("test_a.py"),
        "def test_a(worker_id):\n    assert worker_id\n",
    )
    .unwrap();
    let (code, out) = run(&py, &dir, &[]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("==> ready"), "{out}");
}

#[test]
fn xdist_leftovers_block_and_land_in_json() {
    let Some(py) = python() else { return };
    let dir = fresh_dir("blocked");
    std::fs::write(
        dir.join("pytest.ini"),
        "[pytest]\naddopts = -n 4 --dist loadgroup --tx popen\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("conftest.py"),
        "from xdist import is_xdist_worker\n\ndef pytest_testnodedown(node, error):\n    pass\n",
    )
    .unwrap();
    std::fs::write(dir.join("test_a.py"), "def test_a():\n    pass\n").unwrap();
    let json = dir.join("out.json");
    let (code, out) = run(&py, &dir, &["--xdist-removal-json", json.to_str().unwrap()]);
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("[BLOCKS] pytest.ini addopts: -n 4"), "{out}");
    assert!(out.contains("dist = \"loadgroup\""), "{out}");
    assert!(out.contains("conftest.py:1"), "{out}");
    assert!(out.contains("unknown hook 'pytest_testnodedown'"), "{out}");
    assert_eq!(doc["meta"]["kind"], "xdist-removal-check");
    assert_eq!(doc["ready"], false);
    // Blocking ones only: installed plugins add environment-dependent gates.
    let kinds: Vec<&str> = doc["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["blocking"] == true)
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "addopts_ignored",
            "addopts_ignored",
            "addopts_flag",
            "import",
            "hook"
        ],
        "{doc}"
    );
}

#[test]
fn allow_list_passes_the_gate() {
    let Some(py) = python() else { return };
    let dir = fresh_dir("allow");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\naddopts = -n 4\n").unwrap();
    std::fs::write(dir.join("test_a.py"), "def test_a():\n    pass\n").unwrap();
    let (code, out) = run(&py, &dir, &["--migrate-allow", "pytest.ini"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("[ALLOWED]"), "{out}");
}
