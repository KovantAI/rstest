//! End-to-end tests for the run's temp folder (`$TMPDIR/rstest-<pid>/`): a
//! parallel run must not leave it behind, and a run killed too hard to clean
//! up (SIGKILL) has its folder deleted by the next run. Each test points
//! `TMPDIR` at a fresh dir so it sees only its own runs.
//!
//! Needs a python with pytest and the rstest worker; skips cleanly otherwise.
//! Local: `RSTEST_TEST_VENV=/path/to/venv cargo test -p rstest-cli --test temp_cleanup`
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-tc-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("tmp")).unwrap();
    std::fs::create_dir_all(d.join("suite")).unwrap();
    d
}

fn rstest(venv: &Option<PathBuf>, dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rstest"));
    cmd.args(args)
        .current_dir(dir.join("suite"))
        .env("TMPDIR", dir.join("tmp"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(v) = venv {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("VIRTUAL_ENV", v)
            .env("PATH", format!("{}:{path}", v.join("bin").display()));
    }
    cmd
}

/// The `rstest-<pid>` folders in `dir`'s temp dir.
fn run_folders(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir.join("tmp"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.strip_prefix("rstest-")
                .is_some_and(|p| p.parse::<u32>().is_ok())
        })
        .collect()
}

fn write_tmp_path_tests(dir: &Path) {
    std::fs::write(
        dir.join("suite/test_tmp.py"),
        "def test_a(tmp_path):\n    (tmp_path / 'f').write_text('x')\n\n\
         def test_b(tmp_path):\n    (tmp_path / 'f').write_text('x')\n",
    )
    .unwrap();
}

fn wait_for(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn parallel_run_deletes_its_temp_folder() {
    let Some(venv) = common::pytest_env("pytest, rstest_worker") else {
        return;
    };
    let dir = fresh_dir("clean");
    write_tmp_path_tests(&dir);
    let out = rstest(&venv, &dir, &["-n", "2", "-q"]).output().unwrap();
    let left = run_folders(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(left, Vec::<String>::new(), "temp folder left behind");
}

#[test]
fn user_basetemp_is_kept() {
    let Some(venv) = common::pytest_env("pytest, rstest_worker") else {
        return;
    };
    let dir = fresh_dir("user");
    write_tmp_path_tests(&dir);
    let bt = dir.join("bt");
    let bt_arg = format!("--basetemp={}", bt.display());
    let out = rstest(&venv, &dir, &["-n", "2", "-q", &bt_arg])
        .output()
        .unwrap();
    let kept = bt.join("gw0").is_dir();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(kept, "the user's --basetemp was removed");
}

#[test]
fn next_run_deletes_a_sigkilled_runs_folder() {
    let Some(venv) = common::pytest_env("pytest, rstest_worker") else {
        return;
    };
    let dir = fresh_dir("kill");
    // A test that parks in tmp_path, so the folder exists when rstest is killed.
    std::fs::write(
        dir.join("suite/test_hang.py"),
        "import os, time\n\ndef test_hang(tmp_path):\n    \
         (tmp_path / 'f').write_text('x')\n    \
         open('hang.pid', 'w').write(str(os.getpid()))\n    time.sleep(300)\n",
    )
    .unwrap();
    std::fs::write(dir.join("suite/test_ok.py"), "def test_ok():\n    pass\n").unwrap();
    let mut child = rstest(&venv, &dir, &["-n", "2", "-q"]).spawn().unwrap();
    let pidfile = dir.join("suite/hang.pid");
    let started = wait_for(Duration::from_secs(60), || {
        std::fs::read_to_string(&pidfile).is_ok_and(|s| !s.is_empty())
    });
    let _ = child.kill(); // SIGKILL: no cleanup code runs
    let _ = child.wait();
    if let Ok(worker) = std::fs::read_to_string(&pidfile) {
        let _ = Command::new("kill").args(["-9", &worker]).status();
    }
    let killed = format!("rstest-{}", child.id());
    let left_by_kill = run_folders(&dir).contains(&killed);

    std::fs::remove_file(dir.join("suite/test_hang.py")).unwrap();
    let out = rstest(&venv, &dir, &["-n", "2", "-q"]).output().unwrap();
    let left = run_folders(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(started, "the hanging test never started");
    assert!(left_by_kill, "SIGKILL should leave {killed} for the sweep");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        left,
        Vec::<String>::new(),
        "the killed run's folder survived the next run"
    );
}
