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

#[cfg(test)]
mod tests {
    use super::*;

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
