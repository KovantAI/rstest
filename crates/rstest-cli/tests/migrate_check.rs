//! End-to-end tests for `rstest --migrate-check`. They build a tiny fixture
//! suite in a temp dir and run the real binary against it, asserting the exit
//! code (the CI gate) and the human report.
//!
//! These need a python with pytest. They skip cleanly otherwise. To run them:
//!
//! - CI: have `python3` on PATH with pytest importable, or
//! - local: `RSTEST_TEST_VENV=/path/to/venv cargo test -p rstest-cli --test migrate_check`
//!
//! The venv (or PATH python3) is used for BOTH the top-level collect and the
//! child discriminator runs, via `VIRTUAL_ENV`/`PATH` (the children don't
//! inherit `--python`, only the environment).

use std::path::{Path, PathBuf};
use std::process::Command;

/// (env_for_children) - a venv dir to expose as VIRTUAL_ENV, or None to use the
/// ambient PATH python3. Returns None to SKIP if no pytest is reachable.
fn pytest_env() -> Option<Option<PathBuf>> {
    if let Ok(venv) = std::env::var("RSTEST_TEST_VENV") {
        let py = Path::new(&venv).join("bin").join("python");
        if import_pytest(&py) {
            return Some(Some(PathBuf::from(venv)));
        }
        return None;
    }
    if import_pytest(Path::new("python3")) {
        return Some(None); // ambient python3 has pytest
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
    let d = std::env::temp_dir().join(format!("rstest-mc-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Run the binary in `dir`, routing python via `venv` (VIRTUAL_ENV + PATH) so
/// the top-level and child sessions all use a pytest-capable interpreter.
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

#[test]
fn clean_suite_is_ready() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("clean");
    std::fs::write(
        dir.join("test_clean.py"),
        "def test_a():\n    assert True\n\ndef test_b():\n    assert True\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["--migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "clean suite should be ready (exit 0)\n{out}");
    assert!(out.contains("ready"), "expected 'ready' in:\n{out}");
}

#[test]
fn try_reports_parity_and_speed_on_a_clean_suite() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("try");
    std::fs::write(
        dir.join("test_t.py"),
        "import pytest\n\
         @pytest.mark.parametrize('x', [1, 2, 3])\n\
         def test_x(x):\n    assert x > 0\n\n\
         def test_ok():\n    assert True\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["--try"]);
    let _ = std::fs::remove_dir_all(&dir);
    // Clean suite: rstest -n 0 ≡ pytest, so outcomes are identical -> exit 0.
    assert_eq!(
        code, 0,
        "clean suite should be drop-in ready (exit 0)\n{out}"
    );
    assert!(out.contains("parity"), "expected a parity line:\n{out}");
    assert!(
        out.contains("identical"),
        "expected identical outcomes:\n{out}"
    );
    assert!(out.contains("speed"), "expected a speed line:\n{out}");
}

#[test]
fn uuid_id_is_a_will_bail_blocker() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("uuid");
    // A fresh uuid in the parametrize id => unstable per collection => WILL bail.
    std::fs::write(
        dir.join("test_uuid.py"),
        "import uuid, pytest\n\
         @pytest.mark.parametrize('u', [str(uuid.uuid4())])\n\
         def test_u(u):\n    assert u\n",
    )
    .unwrap();

    let (code, out) = run(&venv, &dir, &["--migrate-check"]);
    assert_eq!(
        code, 1,
        "uuid-id suite should fail the gate (exit 1)\n{out}"
    );
    assert!(out.contains("WILL bail"), "expected 'WILL bail' in:\n{out}");

    // --migrate-check-json writes a versioned doc.
    let jpath = dir.join("mc.json");
    run(
        &venv,
        &dir,
        &["--migrate-check-json", jpath.to_str().unwrap()],
    );
    let txt = std::fs::read_to_string(&jpath).unwrap_or_default();
    assert!(
        txt.contains("\"schema\": 1") && txt.contains("\"will_bail_count\""),
        "migrate-check-json should write a versioned doc:\n{txt}"
    );

    // Allow-listing the site clears the gate.
    let (allow_code, allow_out) = run(
        &venv,
        &dir,
        &["--migrate-check", "--migrate-allow", "test_uuid.py"],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        allow_code, 0,
        "allow-listed suite should pass the gate\n{allow_out}"
    );
    assert!(
        allow_out.contains("gate passes"),
        "expected 'gate passes' in:\n{allow_out}"
    );
}
