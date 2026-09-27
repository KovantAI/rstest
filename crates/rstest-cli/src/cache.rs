//! The per-project `.rstest_cache/` directory — durations, flake history, and
//! the coverage index all live here. CWD-relative by default; `RSTEST_CACHE`
//! overrides the full path for the current process (tests, sandboxed runs, and
//! the shared-cache backend's staging dir). Distinct from `RSTEST_CACHE_DIR`,
//! which steers the machine-global interpreter-probe cache in `discover.rs`.
//!
//! One helper so the path isn't duplicated across the artifact modules, and one
//! atomic writer so a reader (or a concurrent CI writer) never sees a
//! half-written file — the same tmp+rename discipline covtool already uses for
//! the coverage index.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Name of the per-project cache directory (durations, flake history, and the
/// `--changed` coverage index live here). Committed-tree-relative.
pub const DIR_NAME: &str = ".rstest_cache";

/// The cache directory for the current working tree.
pub fn dir() -> PathBuf {
    match std::env::var_os("RSTEST_CACHE") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(DIR_NAME),
    }
}

/// A named file inside the cwd cache dir (e.g. `"durations.json"`).
pub fn file(name: &str) -> PathBuf {
    dir().join(name)
}

/// A named cache file inside a specific project dir's `.rstest_cache`,
/// independent of `RSTEST_CACHE`.
pub fn file_in(project: &Path, name: &str) -> PathBuf {
    project.join(DIR_NAME).join(name)
}

/// The `RSTEST_CACHE` a monorepo root hands the child for project `slug`:
/// `<RSTEST_CACHE>/<slug>` (a relative value anchors at `root`), so projects
/// never share one dir and their ids never collide. `None` when RSTEST_CACHE
/// is unset: each child keeps its own `.rstest_cache`.
pub fn mono_override(root: &Path, slug: &str) -> Option<PathBuf> {
    let base = std::env::var_os("RSTEST_CACHE")?;
    Some(root.join(base).join(slug))
}

/// Atomic, crash-durable write: fully write + `fsync` a uniquely-named tmp file
/// in the same directory, rename it over the target, then best-effort `fsync`
/// the directory so the rename itself survives a crash. Returns the IO result so
/// callers writing AUTHORITATIVE state (the shared-cache remote) can react to a
/// failure; the best-effort local caches ignore it with `let _ =`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let fname = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cache".to_string());
    // Unique tmp name: pid + nanos + a process-local sequence, so two writers of
    // the SAME target (e.g. two hosts compacting base.json on a shared mount,
    // which can share a pid) never land on the same tmp path and tear the file.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = crate::time::now_epoch_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{fname}.{}.{nanos:x}.{seq:x}.tmp",
        std::process::id()
    ));
    // Any early return past this point must not leak the tmp file.
    if let Err(e) = write_synced(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // The data is durable (tmp was fsync'd before the rename); this makes the
    // rename entry itself durable too. Best-effort: the write already succeeded,
    // and only a crash in the window before the next dir flush could lose it.
    sync_dir(parent);
    Ok(())
}

/// Sentinel file whose OS advisory lock serializes a load→modify→write across
/// processes/shards sharing one cache dir. Never read or written — only locked.
pub const LOCK_FILE: &str = ".lock";

/// Run `f` while holding an exclusive advisory lock over the cwd cache dir.
/// `write_atomic` stops torn files but not *lost updates*: two processes (or two
/// shards sharing one cwd cache) each load→modify→write and the last rename wins,
/// silently dropping the other's merge. Wrapping the whole read-modify-write in
/// this lock serializes them so each sees the other's committed state.
pub fn with_lock<T>(f: impl FnOnce() -> T) -> T {
    with_lock_in(&dir(), f)
}

/// `with_lock` against a specific cache dir. Best-effort: if the lock can't be
/// taken (dir uncreatable, unsupported filesystem, permission), `f` still runs
/// unlocked — degrading to the pre-lock last-writer-wins rather than blocking or
/// dropping a run's data. The lock releases when the guard's file handle drops.
pub fn with_lock_in<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = CacheLock::acquire(dir);
    f()
}

/// A held cache-dir lock, released when its file handle drops (both `flock` and
/// `LockFileEx` unlock on close). `file == None` means unlocked (acquire failed);
/// the caller still proceeds, so correctness only degrades to the old behavior.
struct CacheLock {
    #[allow(dead_code)]
    file: Option<std::fs::File>,
}

impl CacheLock {
    fn acquire(dir: &Path) -> Self {
        if std::fs::create_dir_all(dir).is_err() {
            return Self { file: None };
        }
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join(LOCK_FILE))
        {
            Ok(f) if lock_exclusive(&f).is_ok() => Some(f),
            _ => None,
        };
        Self { file }
    }
}

/// Take a blocking exclusive advisory lock on `f`. The lock is per open-file
/// handle, so a second `with_lock` on the same dir — even in this process —
/// blocks until the first releases; callers must not nest it (they don't:
/// `save`/`record`/`write_local` each lock a self-contained read-modify-write).
#[cfg(unix)]
fn lock_exclusive(f: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `f` owns a valid fd for the duration of the call.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    rc_to_result(rc)
}

/// Map a libc-style return code (0 = success) to `io::Result`, reading errno
/// on failure. Split out so the error arm is testable without a failing flock.
#[cfg(unix)]
fn rc_to_result(rc: libc::c_int) -> std::io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn lock_exclusive(f: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
    use windows_sys::Win32::System::IO::OVERLAPPED;
    // SAFETY: a zeroed OVERLAPPED means "lock from offset 0"; the handle is valid
    // for the call. Lock the whole range so it behaves like Unix's flock.
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            f.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut ov,
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(unix, windows)))]
fn lock_exclusive(_f: &std::fs::File) -> std::io::Result<()> {
    // No advisory-lock primitive on this platform: run unlocked (best-effort).
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "file locking unsupported on this platform",
    ))
}

/// Write `bytes` to `path` and flush data+metadata to disk before returning, so
/// a following rename can't expose a name pointing at unflushed (zero/torn)
/// blocks after a crash.
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Best-effort `fsync` of a directory so a rename into it is durable. Unix only:
/// Windows has no directory-handle fsync (opening a dir as a file fails), and
/// NTFS journals the rename, so this is a no-op there.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_override_namespaces_rstest_cache_per_project() {
        let held = crate::test_env::lock();
        let root = Path::new("/repo");
        {
            let _unset = crate::test_env::remove_var(&held, "RSTEST_CACHE");
            // Unset: children keep their own .rstest_cache.
            assert_eq!(mono_override(root, "libs-a"), None);
        }
        {
            let abs = if cfg!(windows) {
                r"C:\ci\cache"
            } else {
                "/ci/cache"
            };
            let _abs = crate::test_env::set_var(&held, "RSTEST_CACHE", abs);
            assert_eq!(
                mono_override(root, "libs-a"),
                Some(Path::new(abs).join("libs-a"))
            );
        }
        let _rel = crate::test_env::set_var(&held, "RSTEST_CACHE", "cache");
        // A relative value anchors at the monorepo root, not each child's cwd.
        assert_eq!(
            mono_override(root, "libs-a"),
            Some(PathBuf::from("/repo/cache/libs-a"))
        );
    }

    #[test]
    fn file_in_uses_project_dir_and_name() {
        let p = file_in(Path::new("/repo/libs/a"), "durations.json");
        assert_eq!(p, Path::new("/repo/libs/a/.rstest_cache/durations.json"));
    }

    #[test]
    fn write_atomic_creates_dirs_writes_and_leaves_no_tmp() {
        let base = std::env::temp_dir().join(format!("rstest-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let target = base.join("nested").join("durations.json");
        write_atomic(&target, b"{\"x\":1.0}").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"{\"x\":1.0}");
        // No leftover per-pid tmp sidecar next to the target.
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp sidecar leaked: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn with_lock_serializes_concurrent_read_modify_write() {
        // Model the lost-update race: N threads each load a counter, +1, store it
        // back. Without the lock, interleaved load→store lose increments and the
        // final value is < N; the lock must serialize them so it lands exactly N.
        let dir = std::env::temp_dir().join(format!("rstest-lock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("counter");
        std::fs::write(&counter, b"0").unwrap();

        const N: usize = 16;
        std::thread::scope(|s| {
            for _ in 0..N {
                let dir = dir.clone();
                let counter = counter.clone();
                s.spawn(move || {
                    with_lock_in(&dir, || {
                        let cur: u64 = std::fs::read_to_string(&counter)
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        // Widen the window the racy version would lose in.
                        std::thread::yield_now();
                        std::fs::write(&counter, (cur + 1).to_string()).unwrap();
                    });
                });
            }
        });

        let final_val: u64 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(final_val, N as u64, "lock must serialize every increment");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_lock_returns_closure_value_and_creates_dir() {
        let dir = std::env::temp_dir().join(format!("rstest-lock-ret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let got = with_lock_in(&dir, || 42);
        assert_eq!(got, 42);
        assert!(
            dir.join(LOCK_FILE).exists(),
            "lock sentinel must be created"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_reports_error_on_unwritable_path() {
        // A write that can't succeed must return Err (so remote writers can react)
        // rather than silently swallowing it.
        let base = std::env::temp_dir().join(format!("rstest-cache-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("afile");
        std::fs::write(&file, b"x").unwrap();
        // Parent component `afile` is a regular file, so create_dir_all fails.
        let target = file.join("nested").join("durations.json");
        assert!(write_atomic(&target, b"{}").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn with_lock_runs_unlocked_when_dir_uncreatable() {
        // Cache dir under a regular file can't be created: acquire degrades to
        // unlocked and the closure still runs.
        let base = std::env::temp_dir().join(format!("rstest-lock-nodir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("afile");
        std::fs::write(&file, b"x").unwrap();
        let dir = file.join("cache");
        let lock = CacheLock::acquire(&dir);
        assert!(lock.file.is_none(), "uncreatable dir must yield no lock");
        assert_eq!(with_lock_in(&dir, || 7), 7);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn with_lock_runs_unlocked_when_lock_file_unopenable() {
        // Lock sentinel path is a directory, so opening it as a file fails:
        // acquire degrades to unlocked and the closure still runs.
        let dir = std::env::temp_dir().join(format!("rstest-lock-noopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(LOCK_FILE)).unwrap();
        let lock = CacheLock::acquire(&dir);
        assert!(
            lock.file.is_none(),
            "unopenable lock file must yield no lock"
        );
        assert_eq!(with_lock_in(&dir, || "ran"), "ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rc_to_result_maps_nonzero_to_err() {
        assert!(rc_to_result(0).is_ok());
        assert!(rc_to_result(-1).is_err());
    }
}
