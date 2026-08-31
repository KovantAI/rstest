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

/// Best-effort atomic write: a per-pid tmp file in the same directory, then
/// rename over the target. Errors are ignored, matching the best-effort
/// contract of the caches this serves.
pub fn write_atomic(path: &Path, bytes: &[u8]) {
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let fname = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cache".to_string());
    // Per-pid tmp name so two rstest processes in the same tree don't collide.
    let tmp = parent.join(format!(".{fname}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    } else {
        let _ = std::fs::remove_file(&tmp);
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
        write_atomic(&target, b"{\"x\":1.0}");
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
}
