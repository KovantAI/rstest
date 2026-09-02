//! On-disk probe cache: positive probes persisted keyed on interpreter path
//! plus (mtime, size) fingerprint, so repeat discovery skips subprocesses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::probe::Probe;

/// (mtime seconds, size bytes) — the pair that gates a cache hit. mtime alone
/// is a 1-second-resolution clock that an in-place rewrite or a mtime-preserving
/// restore (`cp -p`, `touch -r`, tar/rsync `--times`, reinstalling the same
/// version) leaves untouched; a genuinely different interpreter binary almost
/// always differs in size, so the pair catches the swap mtime would miss.
pub(super) fn file_fingerprint(p: &Path) -> Option<(u64, u64)> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((mtime, md.len()))
}

/// On-disk probe cache. The filename carries a schema version so a format
/// change starts a fresh file rather than mis-parsing an old one.
#[derive(Default, Serialize, Deserialize)]
pub(super) struct DiskCache {
    pub(super) entries: HashMap<String, CacheEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct CacheEntry {
    pub(super) mtime: u64,
    /// Interpreter file size in bytes. `#[serde(default)]` keeps old v1 files
    /// readable: a pre-size entry loads as 0, never matches a real file's size,
    /// so it misses and re-probes rather than serving a stale binary.
    #[serde(default)]
    pub(super) size: u64,
    pub(super) probe: Probe,
}

/// `<cache dir>/rstest/interp-probes-v1.json`, or None if no cache dir is
/// resolvable (probing simply isn't persisted then).
fn cache_path() -> Option<PathBuf> {
    let base = if let Some(d) = std::env::var_os("RSTEST_CACHE_DIR") {
        PathBuf::from(d)
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?
    };
    Some(base.join("rstest").join("interp-probes-v1.json"))
}

/// The process-wide cache, loaded from disk on first use.
fn disk() -> &'static Mutex<DiskCache> {
    static D: OnceLock<Mutex<DiskCache>> = OnceLock::new();
    D.get_or_init(|| {
        let loaded = cache_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| read_cache(&b));
        Mutex::new(loaded.unwrap_or_default())
    })
}

pub(super) fn disk_cache_get(candidate: &Path, mtime: u64, size: u64) -> Option<Probe> {
    let key = candidate.to_string_lossy().into_owned();
    let d = disk().lock().unwrap();
    let e = d.entries.get(&key)?;
    (e.mtime == mtime && e.size == size).then(|| e.probe.clone())
}

pub(super) fn disk_cache_put(candidate: &Path, mtime: u64, size: u64, probe: &Probe) {
    let key = candidate.to_string_lossy().into_owned();
    let mut d = disk().lock().unwrap();
    d.entries.insert(
        key,
        CacheEntry {
            mtime,
            size,
            probe: probe.clone(),
        },
    );
    if let Some(path) = cache_path() {
        let _ = write_cache(&path, &d); // best-effort; never fail the run on cache IO
    }
}

pub(super) fn read_cache(bytes: &[u8]) -> Option<DiskCache> {
    serde_json::from_slice(bytes).ok()
}

/// Write the cache via temp-file + rename so a concurrent reader never sees a
/// half-written file. The temp name is pid-scoped to avoid clobbering between
/// concurrent rstest processes.
pub(super) fn write_cache(path: &Path, cache: &DiskCache) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Serialize BEFORE touching the filesystem: on a serialize error, return it
    // rather than renaming an empty/default doc over the real cache (that would
    // silently corrupt it to `[]`/`{}`).
    let bytes = serde_json::to_vec(cache).map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
