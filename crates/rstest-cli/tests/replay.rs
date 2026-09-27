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

/// A conftest that appends `<worker> <nodeid>` to `trace.log` as each test
/// starts. The run summary dedupes by nodeid, so counting real executions
/// (a test run twice still reads "N passed") needs this.
fn write_trace_conftest(dir: &Path) {
    std::fs::write(
        dir.join("conftest.py"),
        "import os\n\ndef pytest_runtest_setup(item):\n    with open(os.path.join(os.path.dirname(__file__), 'trace.log'), 'a') as f:\n        f.write(f\"{os.environ.get('PYTEST_XDIST_WORKER')} {item.nodeid}\\n\")\n",
    )
    .unwrap();
}

/// `(worker, nodeid)` per execution, in the order they started.
fn read_trace(dir: &Path) -> Vec<(String, String)> {
    let trace = std::fs::read_to_string(dir.join("trace.log")).unwrap_or_default();
    trace
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(w, id)| (w.to_string(), id.to_string()))
        .collect()
}

/// The nodeids `worker` ran, in order.
fn ran_on<'a>(trace: &'a [(String, String)], worker: &str) -> Vec<&'a str> {
    trace
        .iter()
        .filter(|(w, _)| w == worker)
        .map(|(_, id)| id.as_str())
        .collect()
}

/// Write a hand-built journal (worker count = `assignment.len()`) to
/// `dir/<name>`, so the schedule under test is fixed rather than emergent.
fn write_journal(dir: &Path, name: &str, assignment: &[&[&str]], collection_size: u64) {
    write_journal_with_args(dir, name, assignment, collection_size, &[]);
}

/// [`write_journal`] with recorded session args.
fn write_journal_with_args(
    dir: &Path,
    name: &str,
    assignment: &[&[&str]],
    collection_size: u64,
    args: &[&str],
) {
    let journal = serde_json::json!({
        "schema": 1,
        "rstest_version": "test",
        "run_uid": name,
        "created_epoch": 0,
        "workers": assignment.len(),
        "dist": "load",
        "shuffle_seed": null,
        "args": args,
        "collection_hash": null,
        "collection_size": collection_size,
        "assignment": assignment,
    });
    std::fs::write(dir.join(name), serde_json::to_vec(&journal).unwrap()).unwrap();
}

/// Regression: replay never builds the dynamic dispatch queue, so when gw0
/// finished first, the "id-carrier died" fallback fired and handed the whole
/// suite to the still-open workers. Checks each test ran exactly once, on its
/// pinned worker, in the pinned order.
#[test]
fn replay_runs_each_pinned_test_once_on_its_worker() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("pinned");
    write_suite(&dir);
    // gw1 gets the slow tests, so gw0 drains and ends while gw1 is still open.
    std::fs::write(
        dir.join("test_b.py"),
        "import time\n\ndef test_b1():\n    time.sleep(0.3)\n\ndef test_b2():\n    time.sleep(0.3)\n\ndef test_b3():\n    time.sleep(0.3)\n",
    )
    .unwrap();
    write_trace_conftest(&dir);
    let gw0: &[&str] = &["test_a.py::test_a1"];
    let gw1: &[&str] = &[
        "test_a.py::test_a2",
        "test_b.py::test_b1",
        "test_b.py::test_b2",
        "test_b.py::test_b3",
        "test_a.py::test_a3",
    ];
    write_journal(&dir, "journal.json", &[gw0, gw1], 6);

    let (code, out) = run(&venv, &dir, &["replay", "--journal", "journal.json"]);
    let trace = read_trace(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(
        !out.contains("id-carrier worker died"),
        "fallback queue must not fire under replay\n{out}"
    );
    assert_eq!(ran_on(&trace, "gw0"), gw0, "{trace:?}");
    assert_eq!(ran_on(&trace, "gw1"), gw1, "{trace:?}");
    assert_eq!(trace.len(), 6, "each test ran exactly once\n{trace:?}");
}

/// The point of replay: an order-dependent failure follows the schedule, not
/// luck. The same suite fails every time under a journal that puts the
/// polluter before the victim on one worker, and passes every time under one
/// that splits them.
#[test]
fn replay_reproduces_an_order_dependent_failure() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("order");
    std::fs::write(dir.join("state.py"), "polluted = []\n").unwrap();
    std::fs::write(
        dir.join("test_order.py"),
        "import state\n\ndef test_polluter():\n    state.polluted.append(1)\n\ndef test_victim():\n    assert not state.polluted\n\ndef test_other():\n    pass\n",
    )
    .unwrap();
    let (p, v, o) = (
        "test_order.py::test_polluter",
        "test_order.py::test_victim",
        "test_order.py::test_other",
    );
    write_journal(&dir, "together.json", &[&[p, v], &[o]], 3);
    write_journal(&dir, "apart.json", &[&[p, o], &[v]], 3);
    for _ in 0..3 {
        let (code, out) = run(&venv, &dir, &["replay", "--journal", "together.json"]);
        assert_eq!(code, 1, "polluter before victim on gw0 must fail\n{out}");
        assert!(out.contains("[gw0] test_order.py::test_victim"), "{out}");
        let (code, out) = run(&venv, &dir, &["replay", "--journal", "apart.json"]);
        assert_eq!(code, 0, "victim alone on gw1 must pass\n{out}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A worker that crashes mid-replay is replaced; the replacement must run only
/// what the dead worker had left. Seeding it with the full recorded list re-ran
/// finished tests and re-crashed until the restart budget ran out (exit 3).
#[test]
fn replay_crash_replacement_runs_only_the_remainder() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("crash");
    std::fs::write(
        dir.join("test_c.py"),
        "import os\n\ndef test_1():\n    pass\n\ndef test_crash():\n    os._exit(1)\n\ndef test_2():\n    pass\n\ndef test_3():\n    pass\n",
    )
    .unwrap();
    std::fs::write(dir.join("test_x.py"), "def test_x():\n    pass\n").unwrap();
    write_trace_conftest(&dir);
    let gw0: &[&str] = &[
        "test_c.py::test_1",
        "test_c.py::test_crash",
        "test_c.py::test_2",
        "test_c.py::test_3",
    ];
    write_journal(&dir, "journal.json", &[gw0, &["test_x.py::test_x"]], 5);

    let (code, out) = run(&venv, &dir, &["replay", "--journal", "journal.json"]);
    let trace = read_trace(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "one crashed test is a plain failure\n{out}");
    // Same slot, fresh process: gw0 is the replacement's name too.
    assert_eq!(
        ran_on(&trace, "gw0"),
        gw0,
        "remainder only, in order\n{trace:?}"
    );
    assert_eq!(trace.len(), 5, "no test ran twice\n{trace:?}");
    assert!(out.contains("4 passed"), "{out}");
}

/// Replay turns reruns off, `@pytest.mark.flaky` included: a retried attempt
/// was requeued onto the dispatch queue replay never builds, so the failure
/// being reproduced vanished and the run went green.
#[test]
fn replay_ignores_flaky_mark_reruns() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("flakymark");
    std::fs::write(
        dir.join("test_f.py"),
        "import pytest\n\n@pytest.mark.flaky(reruns=2)\ndef test_f():\n    assert False\n\ndef test_ok():\n    pass\n",
    )
    .unwrap();
    write_trace_conftest(&dir);
    write_journal(
        &dir,
        "journal.json",
        &[&["test_f.py::test_f"], &["test_f.py::test_ok"]],
        2,
    );
    let (code, out) = run(&venv, &dir, &["replay", "--journal", "journal.json"]);
    let trace = read_trace(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "the failure must surface\n{out}");
    // The exit code alone isn't enough: the worker's own pytest status was 1
    // even while the dropped report left the summary at "1 passed".
    assert!(out.contains("1 failed, 1 passed"), "{out}");
    assert_eq!(ran_on(&trace, "gw0"), ["test_f.py::test_f"], "{trace:?}");
}

/// CI often passes absolute paths (`$GITHUB_WORKSPACE/tests`). The journal
/// stores them relative to the recording cwd, so the project replays from
/// another location (another machine) unchanged.
#[test]
fn journal_args_are_portable_across_checkouts() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("portable");
    write_suite(&dir);
    let abs = std::fs::canonicalize(&dir).unwrap();
    let file_nodeid = format!("{}/test_a.py", abs.display());
    let (code, out) = run(
        &venv,
        &dir,
        &["-n", "2", abs.to_str().unwrap(), &file_nodeid],
    );
    assert_eq!(code, 0, "{out}");
    let latest = dir.join(".rstest_cache").join("replay").join("latest.json");
    let j: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&latest).unwrap()).unwrap();
    assert_eq!(j["args"], serde_json::json!([".", "test_a.py"]), "{j}");

    // "Another machine": same project at a different path, no cache.
    let moved = fresh_dir("portable-moved");
    for f in ["test_a.py", "test_b.py"] {
        std::fs::copy(dir.join(f), moved.join(f)).unwrap();
    }
    std::fs::copy(&latest, moved.join("ci.json")).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let (rcode, rout) = run(&venv, &moved, &["replay", "--journal", "ci.json"]);
    let _ = std::fs::remove_dir_all(&moved);
    assert_eq!(rcode, 0, "{rout}");
    assert!(rout.contains("6 passed"), "{rout}");
}

/// `[tool.rstest] reruns` must not apply under replay either: replay cleared
/// the CLI value to None, which fell back to the config, and the failed attempt
/// was dropped exactly like the @mark.flaky case.
#[test]
fn replay_ignores_config_reruns() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("cfgreruns");
    std::fs::write(dir.join("pyproject.toml"), "[tool.rstest]\nreruns = 2\n").unwrap();
    std::fs::write(
        dir.join("test_f.py"),
        "def test_f():\n    assert False\n\ndef test_ok():\n    pass\n",
    )
    .unwrap();
    write_journal(
        &dir,
        "journal.json",
        &[&["test_f.py::test_f"], &["test_f.py::test_ok"]],
        2,
    );
    let (code, out) = run(&venv, &dir, &["replay", "--journal", "journal.json"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("1 failed, 1 passed"), "{out}");
}

/// `@pytest.mark.serial` tests ran alone in the recording (serial phase), so
/// replay must not start one until every other worker has finished, even when
/// the journal lists it first on its worker.
#[test]
fn replay_runs_serial_tests_alone() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("serial");
    std::fs::write(
        dir.join("conftest.py"),
        "import os, time\n\ndef _log(kind, item):\n    with open(os.path.join(os.path.dirname(__file__), 'trace.log'), 'a') as f:\n        f.write(f\"{time.monotonic()} {kind} {item.name}\\n\")\n\ndef pytest_runtest_setup(item):\n    _log('start', item)\n\ndef pytest_runtest_teardown(item):\n    _log('end', item)\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("test_s.py"),
        "import time, pytest\n\n@pytest.mark.serial\ndef test_serial():\n    pass\n\ndef test_p0():\n    pass\n\ndef test_p1():\n    time.sleep(0.4)\n\ndef test_p2():\n    time.sleep(0.4)\n",
    )
    .unwrap();
    write_journal(
        &dir,
        "journal.json",
        &[
            &["test_s.py::test_serial", "test_s.py::test_p0"],
            &["test_s.py::test_p1"],
            &["test_s.py::test_p2"],
        ],
        4,
    );
    let (code, out) = run(&venv, &dir, &["replay", "--journal", "journal.json"]);
    let trace = std::fs::read_to_string(dir.join("trace.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    let at = |kind: &str, name: &str| -> f64 {
        trace
            .lines()
            .find_map(|l| {
                let mut p = l.split(' ');
                let (t, k, n) = (p.next()?, p.next()?, p.next()?);
                (k == kind && n == name).then(|| t.parse().unwrap())
            })
            .unwrap_or_else(|| panic!("no {kind} {name}\n{trace}"))
    };
    let serial_start = at("start", "test_serial");
    for other in ["test_p0", "test_p1", "test_p2"] {
        assert!(
            at("end", other) <= serial_start,
            "test_serial started before {other} ended\n{trace}"
        );
    }
}

/// A recorded `--lf` must not re-filter the suite by this machine's pytest
/// cache: here the local cache says only `test_ok` failed last, which would
/// drop the CI failure (`test_f`) from the replay.
#[test]
fn replay_drops_recorded_cache_selection_flags() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("lf");
    std::fs::write(
        dir.join("test_f.py"),
        "def test_f():\n    assert False\n\ndef test_ok():\n    pass\n",
    )
    .unwrap();
    let cache = dir.join(".pytest_cache").join("v").join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("lastfailed"), r#"{"test_f.py::test_ok": true}"#).unwrap();
    write_journal_with_args(
        &dir,
        "journal.json",
        &[&["test_f.py::test_f"], &["test_f.py::test_ok"]],
        2,
        &["--lf"],
    );
    let (code, out) = run(&venv, &dir, &["replay", "--journal", "journal.json"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("ignoring recorded --lf"), "{out}");
    assert!(out.contains("1 failed, 1 passed"), "{out}");
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
