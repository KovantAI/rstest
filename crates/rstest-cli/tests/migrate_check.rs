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

mod common;

fn pytest_env() -> Option<Option<PathBuf>> {
    common::pytest_env("pytest")
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
    // not a parallelism finding: it drives phase 2 (parallel), the discriminator
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
    // briefly. Serial (-n 0): no overlap, all pass. In parallel: files land on
    // different workers, run concurrently, and collide on the port -> a
    // parallel-only failure. This exercises the full phase-2 report: the
    // discriminator runs, the per-test classification, the polluter bisect
    // (concurrent races are not serially reproducible), the JSON findings doc,
    // and the allow-list gate. Multiple FILES are required: a worker is
    // seeded with two items, so one or two tests would share a worker.
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
fn warm_duration_cache_still_checks_on_two_workers() {
    // A9: phase 2 ran at `-n auto`, which caps by the duration cache. Once a
    // run had warmed it, this tiny suite got one worker, where the test
    // passes, and migrate-check reported the suite ready.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("warm");
    for i in 0..2 {
        std::fs::write(
            dir.join(format!("test_iso_{i}.py")),
            "import os\n\ndef test_needs_serial():\n    \
             assert 'RSTEST_WORKER_ID' not in os.environ\n",
        )
        .unwrap();
    }
    let (warm_code, warm) = run(&venv, &dir, &["-n", "0"]);
    assert_eq!(warm_code, 0, "the serial warm-up run should pass\n{warm}");
    let (code, out) = run(&venv, &dir, &["migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.contains("running -n 2 to check"), "{out}");
    assert_eq!(code, 1, "a warm cache must not hide the failures\n{out}");
    assert!(out.contains("fail only under parallelism"), "{out}");
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

/// A suite whose `tests/test_b_victim.py::test_victim` fails in any pool
/// worker (a deterministic stand-in for a parallel-only leak) and after
/// `tests/test_y_polluter.py` ran in the same process. Plain pytest runs the
/// victim first, so it passes serially; the bisect can only find the polluter
/// by running it before the victim.
fn polluter_suite(tag: &str, addopts: &str) -> PathBuf {
    let dir = fresh_dir(tag);
    let tests = dir.join("tests");
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(
        dir.join("pyproject.toml"),
        format!("[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\naddopts = \"{addopts}\"\n"),
    )
    .unwrap();
    std::fs::write(
        tests.join("conftest.py"),
        "import os, sys\nsys.path.insert(0, os.path.dirname(__file__))\n",
    )
    .unwrap();
    std::fs::write(tests.join("state.py"), "FLAG = []\n").unwrap();
    std::fs::write(
        tests.join("test_y_polluter.py"),
        "import state\ndef test_pollute():\n    state.FLAG.append(1)\n",
    )
    .unwrap();
    std::fs::write(
        tests.join("test_b_victim.py"),
        "import os, state\n\
         def test_victim():\n\
         \x20   assert 'PYTEST_XDIST_WORKER' not in os.environ and not state.FLAG\n",
    )
    .unwrap();
    for i in 0..4 {
        std::fs::write(
            tests.join(format!("test_m{i}.py")),
            "def test_ok():\n    pass\n",
        )
        .unwrap();
    }
    dir
}

#[test]
fn polluter_bisect_from_a_subdirectory_names_the_polluter() {
    // Regression: bisect_polluter passed rootdir-relative files to children
    // that resolve them from the cwd, so from `tests/` it selected
    // `tests/tests/test_b_victim.py`, collected nothing, and called every
    // victim "not reproducible serially".
    let Some(venv) = pytest_env() else { return };
    let dir = polluter_suite("polsub", "");
    let (_, out) = run(&venv, &dir.join("tests"), &["migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.contains("POLLUTED BY: tests/test_y_polluter.py"),
        "the bisect should find the polluter from a subdirectory:\n{out}"
    );
}

#[test]
fn polluter_bisect_lifts_exitfirst() {
    // Regression: bisect_polluter kept `-x` from addopts, so an unrelated
    // failing file run before the victim ended the session early and the
    // victim read as "not reproducible serially".
    let Some(venv) = pytest_env() else { return };
    let dir = polluter_suite("polx", "-x");
    std::fs::write(
        dir.join("tests/test_c_broken.py"),
        "def test_broken():\n    assert False\n",
    )
    .unwrap();
    let (_, out) = run(&venv, &dir, &["migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.contains("POLLUTED BY: tests/test_y_polluter.py"),
        "the bisect should run past an unrelated -x failure:\n{out}"
    );
}

#[test]
fn try_lifts_exitfirst_on_both_sides() {
    // Regression: with `-x`, pytest stopped at the first failure while the
    // rstest workers had already run the rest, so `try` reported those as
    // "only in rstest" and exited 1 on a suite where both runners agree.
    // The pytest side alone ran 1 test, so the 7-test assertion fails on any
    // core count, even where a single worker stopped rstest early too.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("tryx");
    std::fs::write(
        dir.join("pyproject.toml"),
        "[tool.pytest.ini_options]\naddopts = \"-x\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("test_a.py"), "def test_bad():\n    assert False\n").unwrap();
    for i in 0..6 {
        std::fs::write(
            dir.join(format!("test_o{i}.py")),
            "import time\ndef test_ok():\n    time.sleep(0.1)\n",
        )
        .unwrap();
    }
    let (code, out) = run(&venv, &dir, &["try"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.contains("7 tests — identical outcomes"),
        "both runners should run all 7 tests and agree:\n{out}"
    );
    assert_eq!(code, 0, "identical outcomes should exit 0\n{out}");
}

#[test]
fn try_pins_reruns_off() {
    // Regression: `try` let `[tool.rstest] reruns` apply to its rstest run, so
    // a failure pytest reported was rerun into a pass and read as a parity
    // difference, or, for a parallel-only failure, as "identical".
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("tryrr");
    std::fs::write(dir.join("pyproject.toml"), "[tool.rstest]\nreruns = 2\n").unwrap();
    std::fs::write(
        dir.join("test_flaky.py"),
        "ATTEMPTS = []\n\
         def test_first_attempt_fails():\n\
         \x20   ATTEMPTS.append(1)\n\
         \x20   assert len(ATTEMPTS) > 1\n",
    )
    .unwrap();
    std::fs::write(dir.join("test_ok.py"), "def test_ok():\n    pass\n").unwrap();
    let (code, out) = run(&venv, &dir, &["try"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.contains("identical outcomes"),
        "with reruns off, rstest fails the test exactly as pytest does:\n{out}"
    );
    assert_eq!(code, 0, "identical outcomes should exit 0\n{out}");
}

#[test]
fn parallel_refusal_names_the_cause() {
    // Regression: when the two collections agreed but the pool still refused
    // (ids that differ per worker, or a set order both collections happened
    // to share), migrate-check printed only "could not capture outcomes (no
    // snapshot)" and dropped the pool's error naming the cause. An id built
    // from the worker id matches across the two collections (one process
    // each, same id) but differs on every pool worker, so the pool always
    // refuses.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("refused");
    std::fs::write(
        dir.join("test_w.py"),
        "import os, pytest\n\
         @pytest.mark.parametrize('w', [os.environ.get('PYTEST_XDIST_WORKER', 'none')])\n\
         def test_w(w):\n    pass\n",
    )
    .unwrap();
    for i in 0..3 {
        std::fs::write(
            dir.join(format!("test_o{i}.py")),
            "def test_ok():\n    pass\n",
        )
        .unwrap();
    }
    let (code, out) = run(&venv, &dir, &["migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 2, "a refused parallel pass can't be judged\n{out}");
    assert!(
        out.contains("workers collected different test sets"),
        "the pool's refusal should be shown:\n{out}"
    );
}

#[test]
fn empty_selection_says_nothing_was_selected() {
    // Regression: a selection matching no test wrote a report with zero
    // tests, yet migrate-check said "no snapshot", as if the run had failed.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("empty");
    std::fs::write(dir.join("test_a.py"), "def test_a():\n    pass\n").unwrap();
    let (code, out) = run(&venv, &dir, &["migrate-check", "-k", "nomatch"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 2, "nothing selected is nothing to judge\n{out}");
    assert!(
        out.contains("no tests were selected") && !out.contains("no snapshot"),
        "an empty selection should say so:\n{out}"
    );
}

#[test]
fn unstable_header_counts_only_per_process_ids() {
    // Regression: the UNSTABLE NODEIDS header's "N per-process" counted the
    // order-unstable ids too. A uuid id (a new one per collection, so two
    // per-process ids across the two) plus ten ids shuffled by pid (same ids,
    // a new order in every process) must read "2", not 12.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("header");
    std::fs::write(
        dir.join("test_u.py"),
        "import os, random, uuid, pytest\n\
         @pytest.mark.parametrize('u', [str(uuid.uuid4())])\n\
         def test_uuid(u):\n    pass\n\n\
         _ORDER = list(range(10))\n\
         random.Random(os.getpid()).shuffle(_ORDER)\n\
         @pytest.mark.parametrize('n', _ORDER)\n\
         def test_order(n):\n    pass\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "both sites block\n{out}");
    assert!(
        out.contains("UNSTABLE ORDER"),
        "the shuffled site should be order-unstable:\n{out}"
    );
    assert!(
        out.contains("(2 per-process => WILL bail"),
        "only the uuid ids are per-process:\n{out}"
    );
}

/// A suite whose victim already fails under plain pytest: 100 files each
/// leave state behind, and `test_z_victim.py` (collected last) asserts there
/// is none. In parallel the victim shares a worker with some of them (more
/// files than cores), so it fails there too.
fn preexisting_pollution_suite(tag: &str) -> PathBuf {
    let dir = fresh_dir(tag);
    let tests = dir.join("tests");
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(
        dir.join("pyproject.toml"),
        "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
    )
    .unwrap();
    std::fs::write(
        tests.join("conftest.py"),
        "import os, sys\nsys.path.insert(0, os.path.dirname(__file__))\n",
    )
    .unwrap();
    std::fs::write(tests.join("state.py"), "FLAG = []\n").unwrap();
    for i in 0..100 {
        std::fs::write(
            tests.join(format!("test_a_pol{i:03}.py")),
            "import state\ndef test_pollute():\n    state.FLAG.append(1)\n",
        )
        .unwrap();
    }
    std::fs::write(
        tests.join("test_z_victim.py"),
        "import state\ndef test_victim():\n    assert not state.FLAG\n",
    )
    .unwrap();
    dir
}

#[test]
fn migrate_check_counts_a_plain_pytest_failure_as_preexisting() {
    // Regression: the -n 0 oracle ran only the failing files, so a victim
    // polluted by another file passed it, and a test plain pytest already
    // fails was reported as failing "only under parallelism". On a machine
    // with more cores than files the victim passes in parallel and there is
    // nothing to classify; the assertions still hold.
    let Some(venv) = pytest_env() else { return };
    let dir = preexisting_pollution_suite("mcpre");
    let (code, out) = run(&venv, &dir, &["migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !out.contains("fail only under parallelism"),
        "a failure plain pytest has too is not parallel-only:\n{out}"
    );
    assert_eq!(code, 0, "only a pre-existing failure: ready\n{out}");
}

#[test]
fn audit_counts_a_plain_pytest_failure_as_preexisting() {
    // Regression: as above for audit, which also listed the victim in its
    // serial fix-list, a fix that can't work (it fails at -n 0 as well).
    let Some(venv) = pytest_env() else { return };
    let dir = preexisting_pollution_suite("auditpre");
    let (code, out) = run(&venv, &dir, &["audit"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !out.contains("mark them serial"),
        "a failure plain pytest has too gets no serial fix:\n{out}"
    );
    assert_eq!(code, 0, "only a pre-existing failure: parallel-safe\n{out}");
}

#[test]
fn empty_migrate_allow_allows_nothing() {
    // Regression: `--migrate-allow ""` (an unset CI variable) matched every
    // finding as a substring, so a WILL-bail blocker passed the gate.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("allowempty");
    std::fs::write(
        dir.join("test_u.py"),
        "import uuid, pytest\n\
         @pytest.mark.parametrize('u', [str(uuid.uuid4())])\n\
         def test_uuid(u):\n    pass\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["migrate-check", "--migrate-allow", ""]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        code, 1,
        "an empty allow entry must not allow the blocker\n{out}"
    );
    assert!(!out.contains("allow-listed"), "{out}");
}

#[test]
fn ready_line_does_not_count_skipped_tests_as_passing() {
    // Regression: "ready — N tests pass in parallel" counted skipped tests.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("readyskip");
    std::fs::write(
        dir.join("test_s.py"),
        "import pytest\n\
         def test_ok():\n    pass\n\n\
         @pytest.mark.skip\n\
         def test_skipped():\n    pass\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["migrate-check"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("PARALLEL: ready") && !out.contains("2 tests pass"),
        "a skipped test did not pass:\n{out}"
    );
}
