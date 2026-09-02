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
    let d = super::lock(disk());
    let e = d.entries.get(&key)?;
    (e.mtime == mtime && e.size == size).then(|| e.probe.clone())
}

pub(super) fn disk_cache_put(candidate: &Path, mtime: u64, size: u64, probe: &Probe) {
    let key = candidate.to_string_lossy().into_owned();
    // Update the in-process cache, snapshot it, then RELEASE the lock before any
    // disk IO — serializing + a blocking write under the global lock would stall
    // every other thread probing an interpreter.
    let snapshot = {
        let mut d = super::lock(disk());
        d.entries.insert(
            key,
            CacheEntry {
                mtime,
                size,
                probe: probe.clone(),
            },
        );
        d.entries.clone()
    };
    let Some(path) = cache_path() else {
        return;
    };
    // Read-merge-write instead of overwriting the whole file with our snapshot:
    // this cache is machine-global, so a concurrent process may have added its
    // own freshly-probed entries since we loaded ours. Fold our entries OVER
    // whatever is on disk now (ours win on key collision, disk-only entries
    // survive), so parallel invocations don't clobber each other's probes.
    // A tiny read→write race window remains, but it loses at most the entries
    // added in that window, not the whole file (the prior last-writer-wins bug).
    let on_disk = std::fs::read(&path).ok();
    let merged = merge_entries(on_disk.as_deref(), snapshot);
    let _ = write_cache(&path, &merged); // best-effort; never fail the run on cache IO
}

/// Fold `snapshot` (this process's entries) over whatever was last persisted
/// (`on_disk` bytes, if any parses): ours win on key collision, disk-only
/// entries from concurrent writers survive. Corrupt/absent disk data degrades
/// to just our snapshot.
fn merge_entries(on_disk: Option<&[u8]>, snapshot: HashMap<String, CacheEntry>) -> DiskCache {
    let mut merged = on_disk.and_then(read_cache).unwrap_or_default();
    merged.entries.extend(snapshot);
    merged
}

pub(super) fn read_cache(bytes: &[u8]) -> Option<DiskCache> {
    serde_json::from_slice(bytes).ok()
}

/// Write the cache via temp-file + rename so a concurrent reader never sees a
/// half-written file.
pub(super) fn write_cache(path: &Path, cache: &DiskCache) -> std::io::Result<()> {
    // Serialize BEFORE touching the filesystem: on a serialize error, return it
    // rather than renaming an empty/default doc over the real cache (that would
    // silently corrupt it to `[]`/`{}`).
    let bytes = serde_json::to_vec(cache).map_err(std::io::Error::other)?;
    // Delegate the tmp+rename to the hardened writer. Its tmp name adds nanos +
    // a process-local sequence on top of the pid, so two writers of this
    // MACHINE-GLOBAL cache that happen to share a pid (containers / a shared
    // mount) can't collide on the tmp path and tear the file — the corruption a
    // bare `.<pid>.tmp` name allowed.
    crate::cache::write_atomic(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(size: u64) -> CacheEntry {
        CacheEntry {
            mtime: 1,
            size,
            probe: Probe {
                executable: PathBuf::from("/usr/bin/python3"),
                version: (3, 12, 0),
                implementation: "cpython".into(),
                freethreaded: false,
                worker_importable: true,
            },
        }
    }

    fn disk_bytes(pairs: &[(&str, u64)]) -> Vec<u8> {
        let mut c = DiskCache::default();
        for (k, s) in pairs {
            c.entries.insert((*k).into(), entry(*s));
        }
        serde_json::to_vec(&c).unwrap()
    }

    #[test]
    fn merge_keeps_concurrent_disk_entries_and_ours_win() {
        // Disk already has another process's fresh probe (`b`) plus an older
        // copy of a key we also hold (`a`, size 10).
        let on_disk = disk_bytes(&[("a", 10), ("b", 20)]);
        let mut snapshot = HashMap::new();
        snapshot.insert("a".to_string(), entry(11)); // our newer `a`
        snapshot.insert("c".to_string(), entry(30)); // our new `c`

        let merged = merge_entries(Some(&on_disk), snapshot);

        // b survives (would be dropped by the old full-overwrite), c added,
        // and our a (size 11) wins over disk's a (size 10).
        assert_eq!(merged.entries.len(), 3);
        assert_eq!(merged.entries["a"].size, 11);
        assert_eq!(merged.entries["b"].size, 20);
        assert_eq!(merged.entries["c"].size, 30);
    }

    #[test]
    fn merge_tolerates_absent_and_corrupt_disk() {
        let mut snapshot = HashMap::new();
        snapshot.insert("a".to_string(), entry(1));
        assert_eq!(merge_entries(None, snapshot.clone()).entries.len(), 1);
        assert_eq!(merge_entries(Some(b"not json"), snapshot).entries.len(), 1);
    }
}
