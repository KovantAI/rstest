//! Test-only access to process-global state: env vars and the CWD.
//!
//! Unit tests share one process, so a test that changes `PATH`, a config env
//! var, or the CWD can break any sibling test running at the same time (a
//! `git` shim on `PATH` once made `select::git` tests flaky this way). The
//! contract, enforced by `clippy::disallowed_methods` outside this module:
//!
//! 1. Hold [`lock`] for the whole time the state is changed, and take it
//!    before any setup that reads that state (e.g. resolving `git` on PATH).
//!    Tests that only *read* state another test changes should hold it too.
//! 2. Change state only through [`set_var`], [`remove_var`] and [`set_cwd`].
//!    They take the held lock as proof, and return a guard that restores the
//!    previous value on drop, so a panicking test can't leak its changes.
#![allow(clippy::disallowed_methods)]

use std::ffi::{OsStr, OsString};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

/// Proof that the caller holds the process-global test lock.
pub(crate) type Held = MutexGuard<'static, ()>;

/// Take the process-global test lock. A poisoned lock (a sibling test
/// panicked while holding it) is still usable: its guards already restored
/// whatever it changed.
pub(crate) fn lock() -> Held {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Restores one env var to its previous value (or absence) on drop. Borrows
/// the lock so it can't outlive it.
#[must_use = "the variable is restored when this guard drops"]
pub(crate) struct VarGuard<'a> {
    key: String,
    saved: Option<OsString>,
    _held: PhantomData<&'a Held>,
}

impl Drop for VarGuard<'_> {
    fn drop(&mut self) {
        match &self.saved {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}

pub(crate) fn set_var<'a>(_held: &'a Held, key: &str, value: impl AsRef<OsStr>) -> VarGuard<'a> {
    let saved = std::env::var_os(key);
    std::env::set_var(key, value);
    VarGuard {
        key: key.to_string(),
        saved,
        _held: PhantomData,
    }
}

pub(crate) fn remove_var<'a>(_held: &'a Held, key: &str) -> VarGuard<'a> {
    let saved = std::env::var_os(key);
    std::env::remove_var(key);
    VarGuard {
        key: key.to_string(),
        saved,
        _held: PhantomData,
    }
}

/// Restores the CWD on drop.
#[must_use = "the CWD is restored when this guard drops"]
pub(crate) struct CwdGuard<'a> {
    orig: PathBuf,
    _held: PhantomData<&'a Held>,
}

impl Drop for CwdGuard<'_> {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.orig);
    }
}

pub(crate) fn set_cwd<'a>(_held: &'a Held, dir: &Path) -> CwdGuard<'a> {
    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    CwdGuard {
        orig,
        _held: PhantomData,
    }
}

/// Write `contents` to `path` as an executable (0755) stand-in script.
///
/// Open file descriptors are process-global too: `std::fs::write` holds a
/// writable fd on the script, and a sibling test that spawns a process in that
/// window hands its child a copy until the child execs. Exec'ing the script
/// while any process holds it open for writing fails on Linux with ETXTBSY
/// ("Text file busy"), which once failed CI on a worker stand-in. Writing
/// through a short-lived `sh` keeps the only writable fd in that child, so no
/// sibling fork can inherit it.
#[cfg(unix)]
pub(crate) fn write_executable(path: &Path, contents: &str) {
    let status = std::process::Command::new("/bin/sh")
        .args(["-c", r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#, "sh"])
        .arg(contents)
        .arg(path)
        .status()
        .expect("run /bin/sh to write the stand-in");
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exec a freshly written script while sibling threads keep spawning
    /// processes (the setup that failed CI with ETXTBSY). A script written
    /// with `std::fs::write` loses this race within a few hundred rounds on
    /// Linux; `write_executable` never exposes a writable fd to inherit.
    #[cfg(unix)]
    #[test]
    fn write_executable_survives_concurrent_spawns() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("rstest-write-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let spawners: Vec<_> = (0..4)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = std::process::Command::new("/bin/sh")
                            .args(["-c", "exit 0"])
                            .status();
                    }
                })
            })
            .collect();
        let result = std::panic::catch_unwind(|| {
            for i in 0..300 {
                let script = dir.join(format!("s{i}.sh"));
                write_executable(&script, "#!/bin/sh\nexit 7\n");
                let status = std::process::Command::new(&script)
                    .status()
                    .unwrap_or_else(|e| panic!("exec round {i}: {e}"));
                assert_eq!(status.code(), Some(7));
            }
        });
        stop.store(true, Ordering::Relaxed);
        for t in spawners {
            t.join().unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn var_guard_restores_previous_value_and_absence() {
        let held = lock();
        let key = "RSTEST_TEST_ENV_GUARD";
        std::env::remove_var(key);
        {
            let _a = set_var(&held, key, "one");
            assert_eq!(std::env::var(key).unwrap(), "one");
            {
                let _b = remove_var(&held, key);
                assert!(std::env::var_os(key).is_none());
            }
            assert_eq!(std::env::var(key).unwrap(), "one");
        }
        assert!(std::env::var_os(key).is_none());
    }

    #[test]
    fn var_guard_restores_on_panic() {
        let key = "RSTEST_TEST_ENV_PANIC";
        let r = std::panic::catch_unwind(|| {
            let held = lock();
            let _g = set_var(&held, key, "leak");
            panic!("boom");
        });
        assert!(r.is_err());
        let _held = lock();
        assert!(std::env::var_os(key).is_none());
    }
}
