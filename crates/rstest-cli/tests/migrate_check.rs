//! End-to-end tests for `rstest migrate-check`. They build a tiny fixture
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
    let (code, out) = run(&venv, &dir, &["migrate-check"]);
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
    let (code, out) = run(&venv, &dir, &["try"]);
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

    let (code, out) = run(&venv, &dir, &["migrate-check"]);
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
        &[
            "migrate-check",
            "--migrate-check-json",
            jpath.to_str().unwrap(),
        ],
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
        &["migrate-check", "--migrate-allow", "test_uuid.py"],
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

#[test]
fn preexisting_failure_is_not_a_parallelism_issue() {
    // A test that fails deterministically (serial too) is a pre-existing bug,
    // not a parallelism finding: it drives phase 2 (-n auto), the discriminator
    // runs, and the NotParallel verdict, then the "ready + preexisting" summary.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("pre");
    std::fs::write(
        dir.join("test_pre.py"),
        "def test_ok():\n    assert True\n\ndef test_bad():\n    assert False\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    // The only failure is pre-existing, so no migration blocker -> gate passes.
    assert_eq!(code, 0, "pre-existing-only failure should pass gate\n{out}");
    assert!(out.contains("PARALLEL"), "expected phase-2 output:\n{out}");
    assert!(
        out.contains("pre-existing"),
        "expected a pre-existing summary:\n{out}"
    );
}

#[test]
fn time_stamped_id_is_a_may_bail_not_a_blocker() {
    // A now()-derived parametrize id is unstable across collections but matches
    // the time pattern -> MAY bail (timing), not WILL bail. It must NOT force
    // -n 0; the check proceeds into the parallel phase.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("time");
    std::fs::write(
        dir.join("test_time.py"),
        "import datetime, pytest\n\
         @pytest.mark.parametrize('d', [datetime.datetime.now().isoformat()])\n\
         def test_d(d):\n    assert d\n",
    )
    .unwrap();
    let (_code, out) = run(&venv, &dir, &["migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.contains("may bail (timing)"),
        "expected a may-bail (timing) verdict, not WILL bail:\n{out}"
    );
    // may-bail does not short-circuit; the parallel phase still runs.
    assert!(
        out.contains("PARALLEL"),
        "may-bail should proceed to the parallel phase:\n{out}"
    );
}

#[test]
fn concurrent_resource_race_is_a_parallel_only_finding() {
    // Several files whose one test each binds the SAME fixed port and holds it
    // briefly. Serial (-n 0): no overlap, all pass. Under -n auto: files land on
    // different workers, run concurrently, and collide on the port -> a
    // parallel-only failure. This exercises the full phase-2 report: the
    // discriminator runs, the per-test classification, the polluter bisect
    // (concurrent races are not serially reproducible), the JSON findings doc,
    // and the allow-list gate. Multiple FILES are required: `-n auto` never uses
    // more workers than test files, so a one-file suite would stay serial.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("race");
    let body = "import socket, time\n\
                def test_bind():\n\
                \x20   s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)\n\
                \x20   try:\n\
                \x20       s.bind(('127.0.0.1', 55321))\n\
                \x20       time.sleep(0.15)\n\
                \x20   finally:\n\
                \x20       s.close()\n";
    for i in 0..6 {
        std::fs::write(dir.join(format!("test_race_{i}.py")), body).unwrap();
    }
    let jpath = dir.join("mc.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "migrate-check",
            "--migrate-check-json",
            jpath.to_str().unwrap(),
        ],
    );
    // A single-core box may never run two files at once (no collision); only
    // assert the report WHEN a parallel-only failure actually surfaced.
    if out.contains("fail only under parallelism") {
        assert_eq!(
            code, 1,
            "parallel-only findings should fail the gate\n{out}"
        );
        // A concurrent-resource race doesn't reproduce under serial replay.
        assert!(
            out.contains("not reproducible serially"),
            "a port collision should read as a non-reproducible race:\n{out}"
        );
        let txt = std::fs::read_to_string(&jpath).unwrap_or_default();
        assert!(
            txt.contains("\"ready\": false") && txt.contains("\"nodeid\""),
            "the json doc should record the findings:\n{txt}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn try_on_a_red_suite_notes_preexisting_failures() {
    // `try` on a suite with a deterministic failure: pytest and rstest agree
    // (both red on the same test) -> outcomes identical, exit 0, but the report
    // must flag the failure as pre-existing rather than caused by rstest.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("tryred");
    std::fs::write(
        dir.join("test_red.py"),
        "def test_a():\n    assert True\n\ndef test_b():\n    assert False\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["try"]);
    let _ = std::fs::remove_dir_all(&dir);
    // Same failure under both runners -> identical outcomes -> drop-in ready.
    assert_eq!(
        code, 0,
        "identical (both red) outcomes should exit 0\n{out}"
    );
    assert!(
        out.contains("pre-existing"),
        "a red pytest run should be flagged pre-existing:\n{out}"
    );
    assert!(
        out.contains("drop-in ready"),
        "identical outcomes should still read drop-in ready:\n{out}"
    );
}

#[test]
fn try_reports_divergent_outcomes() {
    // A test whose result depends on running inside an rstest worker: it passes
    // under plain pytest but fails under rstest (the worker sets RSTEST_RUN_UID),
    // so `try` sees one differing outcome. Drives the non-identical report branch
    // and the exit-1 "some tests differ" path.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("trydiff");
    std::fs::write(
        dir.join("test_div.py"),
        "import os\n\
         def test_ok():\n    assert True\n\n\
         def test_worker_env():\n    assert 'RSTEST_RUN_UID' not in os.environ\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["try"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "a differing outcome should exit 1\n{out}");
    assert!(
        out.contains("differ"),
        "expected a divergent parity line:\n{out}"
    );
    assert!(
        out.contains("some tests differ"),
        "expected the 'some tests differ' guidance:\n{out}"
    );
}

#[test]
fn try_reports_time_saved_when_parallel_wins() {
    // Several slow, independent files: pytest runs them serially, rstest fans
    // them across workers. The >1s wall-clock saving drives the "saves … per
    // run" projection line. The fixture dir is not a git repo, so the commit
    // cadence is unknown and the plain per-run form is printed.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("saves");
    for i in 0..6 {
        std::fs::write(
            dir.join(format!("test_slow_{i}.py")),
            "import time\ndef test_slow():\n    time.sleep(0.5)\n    assert True\n",
        )
        .unwrap();
    }
    let (code, out) = run(&venv, &dir, &["try"]);
    let _ = std::fs::remove_dir_all(&dir);
    // Identical outcomes (all pass) -> drop-in ready, exit 0.
    assert_eq!(code, 0, "clean slow suite should be drop-in ready\n{out}");
    // Wall-clock parallel win over the serial pytest baseline.
    if out.contains("saves") {
        assert!(
            out.contains("per run"),
            "the saves line should quote a per-run figure:\n{out}"
        );
    }
}
