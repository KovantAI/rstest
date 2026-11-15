//! End-to-end guard: the run's post-run saves overlap worker teardown, yet the
//! run never exits before its workers do.
//!
//! A test registers an `atexit` handler that keeps its worker alive for a
//! while, then writes a marker file. `durations.json` must be written before
//! the marker (the save didn't wait for teardown), and the marker must exist
//! once rstest returns (rstest still waited for the worker).
//!
//! Needs a python with pytest and the rstest worker; skips cleanly otherwise.
//! Local: `RSTEST_TEST_VENV=/path/to/venv cargo test -p rstest-cli --test worker_exit`
#![cfg(unix)]

use std::process::Command;
use std::time::SystemTime;

mod common;

fn mtime(p: &std::path::Path) -> SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn assert_saves_overlap_teardown(tag: &str, args: &[&str]) {
    let Some(venv) = common::pytest_env("pytest, rstest_worker") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("rstest-worker-exit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("worker-exited");
    std::fs::write(
        dir.join("test_a.py"),
        "import atexit, os, time\n\n\
         def _late_exit():\n    \
         time.sleep(1.5)\n    \
         open(os.environ['EXIT_MARKER'], 'w').close()\n\n\
         def test_a():\n    \
         atexit.register(_late_exit)\n",
    )
    .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rstest"));
    cmd.args(args)
        .arg("-q")
        .current_dir(&dir)
        .env("EXIT_MARKER", &marker)
        .env("PYTHONPATH", common::pythonpath_with_worker(None));
    if let Some(v) = &venv {
        let mut path = vec![v.join("bin")];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        cmd.env("PATH", std::env::join_paths(path).unwrap())
            .env("VIRTUAL_ENV", v);
    }
    let out = cmd.output().expect("spawn rstest");
    assert!(
        out.status.success(),
        "rstest failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(marker.exists(), "rstest returned before its worker exited");
    let durations = dir.join(".rstest_cache").join("durations.json");
    assert!(
        mtime(&durations) < mtime(&marker),
        "durations.json was saved only after the worker finished exiting"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn single_worker_saves_while_the_worker_exits() {
    assert_saves_overlap_teardown("single", &["-n", "1"]);
}

#[test]
fn pool_saves_while_workers_exit() {
    assert_saves_overlap_teardown("pool", &["-n", "2", "--collect", "full"]);
}

#[test]
fn lazy_pool_saves_while_workers_exit() {
    assert_saves_overlap_teardown("lazy", &["-n", "2", "--collect", "lazy"]);
}
