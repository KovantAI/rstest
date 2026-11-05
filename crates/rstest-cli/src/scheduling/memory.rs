//! Memory-aware `-n auto`. Every worker reports its peak RSS when its session
//! ends; the run keeps the largest in `.rstest_cache/memory.json` (the last
//! [`KEPT`] runs). The next `-n auto` caps the worker count so that many peaks
//! fit in [`HEADROOM`] of the memory the run may use: the cgroup limit (a
//! container or CI job) or physical RAM. Without it a too-wide pool on a small
//! runner looks like a run full of worker crashes, not a sizing problem.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cache;

pub const FILE: &str = "memory.json";
/// Runs whose peak is kept; the cap uses the largest, so one narrowed run
/// (`-k`, `--changed`) with a small peak can't widen the next full run.
const KEPT: usize = 5;
/// Share of the limit the workers may fill together: the rest is for the
/// orchestrator, the OS, and what the tests spawn.
const HEADROOM: f64 = 0.8;

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    /// Peak worker RSS in bytes, one per run, oldest first.
    peaks: Vec<u64>,
}

/// Record the largest worker peak RSS of this run.
pub fn record(peak: u64) {
    record_in(&cache::file(FILE), peak);
}

fn record_in(path: &Path, peak: u64) {
    let mut s = load(path);
    s.peaks.push(peak);
    let excess = s.peaks.len().saturating_sub(KEPT);
    s.peaks.drain(..excess);
    if let Ok(bytes) = serde_json::to_vec(&s) {
        let _ = cache::write_atomic(path, &bytes);
    }
}

fn load(path: &Path) -> Stored {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Why `-n auto` ran fewer workers than it would have.
#[derive(Debug, PartialEq)]
pub struct Cap {
    pub workers: usize,
    pub peak: u64,
    pub limit: u64,
}

impl Cap {
    /// The run-start note.
    pub fn note(&self) -> String {
        format!(
            "rstest: -n auto capped at {} worker{} by memory (peak worker RSS {} \
             recorded, {} available); pass -n N to override",
            self.workers,
            if self.workers == 1 { "" } else { "s" },
            gib(self.peak),
            gib(self.limit),
        )
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

/// The cap on `n` auto workers, when the recorded peak RSS and the memory
/// limit allow fewer.
pub fn cap(n: usize) -> Option<Cap> {
    let peak = load(&cache::file(FILE)).peaks.into_iter().max()?;
    cap_for(n, peak, limit()?)
}

fn cap_for(n: usize, peak: u64, limit: u64) -> Option<Cap> {
    if peak == 0 {
        return None;
    }
    let fit = ((limit as f64 * HEADROOM) / peak as f64).floor() as usize;
    let workers = fit.max(1);
    (workers < n).then_some(Cap {
        workers,
        peak,
        limit,
    })
}

/// The memory this process may use: the tightest cgroup limit on its path
/// (v2 `memory.max`, v1 `memory.limit_in_bytes`), else physical RAM.
#[cfg(target_os = "linux")]
fn limit() -> Option<u64> {
    let total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| meminfo_total(&m));
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|c| cgroup_limit(Path::new("/sys/fs/cgroup"), &c));
    match (cgroup, total) {
        (Some(c), Some(t)) => Some(c.min(t)),
        (c, t) => c.or(t),
    }
}

#[cfg(target_os = "macos")]
fn limit() -> Option<u64> {
    let mut size: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a u64; `size` and `len` are live and sized for it.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut size as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && size > 0).then_some(size)
}

// ponytail: no limit off Linux/macOS, so -n auto stays CPU-sized there;
// GlobalMemoryStatusEx (windows-sys) when Windows needs it.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn limit() -> Option<u64> {
    None
}

/// `MemTotal` from /proc/meminfo, in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn meminfo_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// The tightest memory limit on this process's cgroup path under `root`,
/// walking up from its own cgroup (a limit on a parent applies too). `cgroup`
/// is /proc/self/cgroup: `0::/path` on v2, `N:memory:/path` on v1.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cgroup_limit(root: &Path, cgroup: &str) -> Option<u64> {
    let (base, rel, file) = cgroup.lines().find_map(|l| {
        let mut parts = l.splitn(3, ':');
        let (_, controllers, path) = (parts.next()?, parts.next()?, parts.next()?);
        if controllers.is_empty() {
            Some((root.to_path_buf(), path, "memory.max"))
        } else if controllers.split(',').any(|c| c == "memory") {
            Some((root.join("memory"), path, "memory.limit_in_bytes"))
        } else {
            None
        }
    })?;
    let mut dir = base.join(rel.trim_start_matches('/'));
    let mut best: Option<u64> = None;
    loop {
        // "max" (v2) or a page-rounded i64::MAX (v1) means unlimited: no parse
        // or no effect against physical RAM.
        if let Some(v) = std::fs::read_to_string(dir.join(file))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            best = Some(best.map_or(v, |b| b.min(v)));
        }
        if dir == base || !dir.pop() || !dir.starts_with(&base) {
            break;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn caps_only_when_memory_fits_fewer_workers() {
        // 16 GiB * 0.8 / 2 GiB = 6.4 -> 6 of 10.
        let c = cap_for(10, 2 * GIB, 16 * GIB).unwrap();
        assert_eq!(c.workers, 6);
        assert!(c.note().contains("capped at 6 workers"), "{}", c.note());
        assert!(c.note().contains("2.0 GiB recorded, 16.0 GiB available"));
        // Room for all of them: no cap.
        assert_eq!(cap_for(4, 2 * GIB, 16 * GIB), None);
        // Never below one worker, however big the peak.
        assert_eq!(cap_for(8, 64 * GIB, 16 * GIB).unwrap().workers, 1);
        assert_eq!(cap_for(8, 0, 16 * GIB), None);
    }

    #[test]
    fn record_keeps_the_last_runs_and_cap_uses_their_largest() {
        let dir = std::env::temp_dir().join(format!("rstest-mem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        for peak in [9, 1, 2, 3, 4, 5] {
            record_in(&path, peak);
        }
        // The oldest (9) aged out; the largest kept is 5.
        assert_eq!(load(&path).peaks, [1, 2, 3, 4, 5]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn meminfo_total_reads_kib() {
        let m = "MemTotal:       16318412 kB\nMemFree:          123 kB\n";
        assert_eq!(meminfo_total(m), Some(16_318_412 * 1024));
        assert_eq!(meminfo_total("nothing"), None);
    }

    #[test]
    fn cgroup_limit_takes_the_tightest_on_the_path() {
        let root = std::env::temp_dir().join(format!("rstest-cg-{}", std::process::id()));
        let leaf = root.join("ci.slice/job");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(root.join("memory.max"), "max\n").unwrap();
        std::fs::write(root.join("ci.slice/memory.max"), "4294967296\n").unwrap();
        std::fs::write(leaf.join("memory.max"), "max\n").unwrap();
        // v2: the parent slice's 4 GiB applies to the job below it.
        assert_eq!(cgroup_limit(&root, "0::/ci.slice/job\n"), Some(4 * GIB));
        // No limit anywhere: none.
        std::fs::write(root.join("ci.slice/memory.max"), "max\n").unwrap();
        assert_eq!(cgroup_limit(&root, "0::/ci.slice/job\n"), None);
        // v1: the memory controller's own hierarchy.
        let v1 = root.join("memory/docker/abc");
        std::fs::create_dir_all(&v1).unwrap();
        std::fs::write(v1.join("memory.limit_in_bytes"), "2147483648\n").unwrap();
        assert_eq!(
            cgroup_limit(&root, "4:cpu,cpuacct:/docker/abc\n3:memory:/docker/abc\n"),
            Some(2 * GIB)
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
