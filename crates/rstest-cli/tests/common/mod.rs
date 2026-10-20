//! Interpreter probe shared by the end-to-end tests that need a real python.
//!
//! They run against `$RSTEST_TEST_VENV/bin/python`, else the ambient
//! `python3`, and skip when it cannot import what the test needs. With
//! `RSTEST_TEST_REQUIRE=1` (set in CI) a missing interpreter fails the test
//! instead, so a CI job that lost its python deps cannot pass by skipping.
//!
//! They always skip on Windows: they drive POSIX tooling (`sh -c`,
//! `:`-joined PATH, `<venv>/bin/python`). A Windows runner can still have a
//! `python3` with pytest, so the probe alone would not skip them there.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The test venv (`Some(Some(venv))`), the ambient `python3` (`Some(None)`),
/// or `None` to skip. `modules` is a comma-separated import list.
pub fn pytest_env(modules: &str) -> Option<Option<PathBuf>> {
    if cfg!(windows) {
        return posix_only();
    }
    if let Ok(venv) = std::env::var("RSTEST_TEST_VENV") {
        let py = Path::new(&venv).join("bin").join("python");
        if importable(&py, modules) {
            return Some(Some(PathBuf::from(venv)));
        }
        return skip(&format!("{} cannot import {modules}", py.display()));
    }
    if importable(Path::new("python3"), modules) {
        return Some(None);
    }
    skip(&format!("python3 cannot import {modules}"))
}

/// The interpreter itself rather than its venv, for tests that pass it with
/// `--python`.
pub fn python(modules: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        return posix_only();
    }
    let py = match std::env::var("RSTEST_TEST_VENV") {
        Ok(venv) => Path::new(&venv).join("bin").join("python"),
        Err(_) => PathBuf::from("python3"),
    };
    if importable(&py, modules) {
        return Some(py);
    }
    skip(&format!("{} cannot import {modules}", py.display()))
}

/// Whether `py` can `import <modules>`. The repo's `python/` is put on
/// PYTHONPATH, as the rstest binary does for its workers, so `rstest_worker`
/// resolves from a checkout without a manual PYTHONPATH.
pub fn importable(py: &Path, modules: &str) -> bool {
    importable_with(py, modules, std::env::var_os("PYTHONPATH"))
}

/// [`importable`] with the caller's PYTHONPATH passed in, so a test can probe
/// as if none were set without touching the process env.
pub fn importable_with(py: &Path, modules: &str, existing: Option<OsString>) -> bool {
    Command::new(py)
        .args(["-c", &format!("import {modules}")])
        .env("PYTHONPATH", pythonpath_with_worker(existing))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The repo's `python/` dir, where `rstest_worker` lives in a checkout.
pub fn worker_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python")
}

/// `existing` PYTHONPATH with [`worker_dir`] put first.
pub fn pythonpath_with_worker(existing: Option<OsString>) -> OsString {
    let mut paths = vec![worker_dir()];
    if let Some(existing) = existing {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).unwrap_or_default()
}

/// Skip on Windows even under `RSTEST_TEST_REQUIRE=1`: the harness, not
/// the interpreter, is what is missing there.
fn posix_only<T>() -> Option<T> {
    eprintln!("skipping: POSIX-only end-to-end test");
    None
}

fn skip<T>(why: &str) -> Option<T> {
    skip_or_fail(
        std::env::var("RSTEST_TEST_REQUIRE").is_ok_and(|v| v == "1"),
        why,
    )
}

/// Skip (`None`) with a `skipping:` line, or panic when `required`.
pub fn skip_or_fail<T>(required: bool, why: &str) -> Option<T> {
    if required {
        panic!("RSTEST_TEST_REQUIRE=1 but {why}");
    }
    eprintln!("skipping: {why}");
    None
}
