//! The run's private temp folder, `$TMPDIR/rstest-<pid>/`. Everything rstest
//! itself puts in the temp dir lives here: the workers' `--basetemp` parents
//! (`gwN/`), the no-cacheprovider scratch cache, bisect and migrate-check work
//! files, the diff-coverage hand-off.
//!
//! [`claim`] holds an exclusive lock on `<dir>/.lock` for the whole process
//! and deletes the folder when dropped. A process that dies without dropping
//! it (SIGKILL, OOM, a second Ctrl+C) leaves the folder behind, but the kernel
//! still releases the lock, so the next run's [`sweep`] can tell an abandoned
//! folder from a live one without trusting pids, which get reused.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

const LOCK: &str = ".lock";

/// A folder with no `.lock` was left by a release before the lock existed, or
/// is a live one still being set up. Only the first gets old.
const UNLOCKED_GRACE: Duration = Duration::from_secs(60 * 60);

/// This process's temp folder, created if missing.
pub(crate) fn dir() -> PathBuf {
    let dir = path();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn path() -> PathBuf {
    std::env::temp_dir().join(format!("rstest-{}", std::process::id()))
}

/// Holds the temp folder's lock; deletes the folder on drop.
pub(crate) struct Guard {
    dir: PathBuf,
    lock: Option<File>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Close (and so unlock) first: Windows can't remove a folder holding an
        // open file. A sweeper that slips in between only deletes it sooner.
        drop(self.lock.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Lock this process's temp folder for the rest of the run. `None` when the
/// lock can't be taken: the run goes on, and the folder is left for a sweep.
pub(crate) fn claim() -> Option<Guard> {
    claim_at(path())
}

fn claim_at(dir: PathBuf) -> Option<Guard> {
    std::fs::create_dir_all(&dir).ok()?;
    // Lock under another name, then rename into place: a sweeper only opens
    // `.lock`, so it never finds one that exists but isn't held yet.
    let pending = dir.join(".lock-pending");
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&pending)
        .ok()?;
    // Busy: another live process with our pid (pid namespaces sharing one
    // /tmp). Its folder is not ours to delete.
    crate::cache::try_lock_exclusive(&f).ok()?;
    std::fs::rename(&pending, dir.join(LOCK)).ok()?;
    Some(Guard { dir, lock: Some(f) })
}

/// Delete the temp folders of rstest processes that are gone.
pub(crate) fn sweep() {
    sweep_in(&std::env::temp_dir(), std::process::id(), UNLOCKED_GRACE);
}

fn sweep_in(root: &Path, own_pid: u32, grace: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let pid = e
            .file_name()
            .to_str()
            .and_then(|n| n.strip_prefix("rstest-"))
            .and_then(|p| p.parse::<u32>().ok());
        if pid.is_none_or(|p| p == own_pid) || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if abandoned(&e.path(), grace) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn abandoned(dir: &Path, grace: Duration) -> bool {
    match File::open(dir.join(LOCK)) {
        // Only a dead owner's lock is free. Our handle closes (unlocking) at the
        // end of this arm, before the delete; flock locks belong to the open
        // file, so closing ours never drops a live owner's.
        Ok(f) => crate::cache::try_lock_exclusive(&f).is_ok(),
        Err(_) => std::fs::metadata(dir)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > grace),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("rstest-scratch-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn guard_drop_deletes_the_folder() {
        let dir = root("drop").join("rstest-1");
        let guard = claim_at(dir.clone()).unwrap();
        std::fs::create_dir_all(dir.join("gw0")).unwrap();
        std::fs::write(dir.join("gw0/f"), "x").unwrap();
        assert!(dir.join(LOCK).is_file());
        assert!(!dir.join(".lock-pending").exists());
        drop(guard);
        assert!(!dir.exists());
    }

    #[test]
    fn sweep_deletes_unlocked_and_keeps_held_own_and_foreign() {
        let root = root("sweep");
        let dead = root.join("rstest-11");
        let live = root.join("rstest-12");
        let own = root.join("rstest-13");
        let legacy = root.join("rstest-14");
        let foreign = [
            root.join("rstest-nocache-1-ab"),
            root.join("rstest-xr-test-a-1"),
        ];
        // A claimed folder whose guard is forgotten: the lock file stays, the
        // lock goes with the handle, as when the owner was SIGKILLed.
        let mut gone = claim_at(dead.clone()).unwrap();
        drop(gone.lock.take());
        std::mem::forget(gone);
        let held = claim_at(live.clone()).unwrap();
        for d in [&own, &legacy].into_iter().chain(&foreign) {
            std::fs::create_dir_all(d).unwrap();
        }
        // A pre-lock folder younger than the grace period may be one being set up.
        sweep_in(&root, 13, Duration::from_secs(3600));
        assert!(!dead.exists(), "abandoned lock: deleted");
        assert!(live.exists(), "held lock: kept");
        assert!(own.exists(), "own pid: kept");
        assert!(legacy.exists(), "young, no lock: kept");
        // Past the grace period a lock-less folder is a leftover from before the lock.
        sweep_in(&root, 13, Duration::ZERO);
        assert!(!legacy.exists(), "old, no lock: deleted");
        assert!(live.exists() && own.exists());
        assert!(
            foreign.iter().all(|d| d.exists()),
            "other rstest-* names: kept"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&root);
    }
}
