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

/// A named cache file inside a specific project dir. Monorepo children keep
/// their own `.rstest_cache`, independent of `RSTEST_CACHE`.
pub fn file_in(project: &Path, name: &str) -> PathBuf {
    project.join(DIR_NAME).join(name)
}

/// Atomic write: a uniquely-named tmp file in the same directory, then rename
/// over the target. Returns the IO result so callers writing AUTHORITATIVE state
/// (the shared-cache remote) can react to a failure; the best-effort local caches
/// ignore it with `let _ =`.
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
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{fname}.{}.{nanos:x}.{seq:x}.tmp",
        std::process::id()
    ));
    match std::fs::write(&tmp, bytes) {
        Ok(()) => std::fs::rename(&tmp, path),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
