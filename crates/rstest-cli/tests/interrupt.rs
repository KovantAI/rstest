//! End-to-end test for SIGTERM on a parallel run: a CI job killed by its
//! timeout must still get a summary naming the hung test, a replay journal and
//! the requested reports, and must leave no worker running.
//!
//! Needs a python with pytest and the rstest worker; skips cleanly otherwise.
//! Local: `RSTEST_TEST_VENV=/path/to/venv cargo test -p rstest-cli --test interrupt`
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn pytest_env() -> Option<Option<PathBuf>> {
    if let Ok(venv) = std::env::var("RSTEST_TEST_VENV") {
        let py = Path::new(&venv).join("bin").join("python");
        return import_worker(&py).then(|| Some(PathBuf::from(venv)));
    }
    import_worker(Path::new("python3")).then_some(None)
}

fn import_worker(py: &Path) -> bool {
    Command::new(py)
        .args(["-c", "import pytest, rstest_worker"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-interrupt-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Poll `cond` until it holds or `limit` passes.
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

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn sigterm_stops_the_pool(tag: &str, extra: &[&str]) {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir(tag);
    // The hung test writes its worker's pid, so the test can both wait for it
    // to be running and check the worker is gone afterwards.
    std::fs::write(
        dir.join("test_hang.py"),
        "import os, time\n\ndef test_hang():\n    \
         open('hang.pid', 'w').write(str(os.getpid()))\n    time.sleep(300)\n",
    )
    .unwrap();
    std::fs::write(dir.join("test_ok.py"), "def test_ok():\n    pass\n").unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rstest"));
    cmd.args([
        "-n",
        "2",
        "-q",
        "--junitxml",
        "j.xml",
        "--report-json",
        "r.json",
    ])
    .args(extra)
    .current_dir(&dir)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if let Some(v) = &venv {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("VIRTUAL_ENV", v)
            .env("PATH", format!("{}:{path}", v.join("bin").display()));
    }
    let mut child = cmd.spawn().expect("spawn rstest");
    let pidfile = dir.join("hang.pid");
    let started = wait_for(Duration::from_secs(60), || {
        std::fs::read_to_string(&pidfile).is_ok_and(|s| !s.is_empty())
    });
    if !started {
        let _ = child.kill();
        panic!("the hanging test never started");
    }
    let worker_pid = std::fs::read_to_string(&pidfile).unwrap();
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let exited = wait_for(Duration::from_secs(20), || {
        child.try_wait().is_ok_and(|s| s.is_some())
    });
    if !exited {
        let _ = child.kill();
        let _ = Command::new("kill").args(["-9", &worker_pid]).status();
        panic!("rstest did not exit after SIGTERM");
    }
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let worker_gone = wait_for(Duration::from_secs(5), || !alive(&worker_pid));
    if !worker_gone {
        let _ = Command::new("kill").args(["-9", &worker_pid]).status();
    }
    let journal = dir.join(".rstest_cache/replay/latest.json").is_file();
    let junit = std::fs::read_to_string(dir.join("j.xml")).unwrap_or_default();
    let report = dir.join("r.json").is_file();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("interrupted by SIGTERM"), "{text}");
    assert!(text.contains("test_hang.py::test_hang"), "{text}");
    assert!(worker_gone, "worker {worker_pid} outlived rstest\n{text}");
    assert!(journal, "no replay journal written\n{text}");
    assert!(junit.contains("interrupted by SIGTERM"), "{junit}");
    assert!(report, "no --report-json written\n{text}");
}

#[test]
fn sigterm_stops_the_eager_pool_and_keeps_the_artifacts() {
    sigterm_stops_the_pool("eager", &["--collect", "full"]);
}

#[test]
fn sigterm_stops_the_lazy_pool_and_keeps_the_artifacts() {
    sigterm_stops_the_pool("lazy", &["--collect", "lazy"]);
}
