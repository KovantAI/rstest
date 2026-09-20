//! Per-test duration cache: `.rstest_cache/durations.json` in the cwd.
//! Drives long-pole-first scheduling; absent or stale entries are harmless,
//! unknown tests just keep collection order.
//!
//! Each entry is tagged with the `(mtime, size)` fingerprint of its test's
//! source file (issue #18). That is how the cache self-heals: on load an entry
//! whose source file no longer matches — edited body (new mtime/size), or a
//! deleted/renamed file (no metadata at all) — is dropped, so a changed test
//! re-times on fresh numbers instead of scheduling on stale ones, and vanished
//! nodeids stop accumulating (the merge-on-write below only ever *added*). The
//! validation is a pure function of the working tree, which is stable for the
//! duration of a run, so every shard worker restoring the cache derives the
//! same snapshot — the determinism the sharder in `shard.rs` relies on.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cache;
use crate::reporting::report::Run;

pub const FILE: &str = "durations.json";

/// A cached call duration, tagged with the source file's fingerprint when
/// recorded. `mtime` is a 1s-resolution clock that a mtime-preserving edit
/// (`touch -r`, some editors) can leave untouched, so it is paired with `size`
/// to catch the rewrite mtime alone would miss — the same pairing
/// `discover::cache` uses for the interpreter probe cache.
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Timing {
    secs: f64,
    /// Source-file mtime (secs since epoch) when recorded. `#[serde(default)]`
    /// keeps a legacy bare-float file readable via the untagged parse below; a
    /// missing fingerprint reads as 0 and never matches a real file's mtime.
    #[serde(default)]
    mtime: u64,
    #[serde(default)]
    size: u64,
}

/// Accepts both the tagged on-disk form and the legacy bare-float one, so an
/// upgrade reads old caches instead of discarding them. A JSON number can only
/// be `Bare` (a struct needs a map); an object can only be `Tagged`.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredTiming {
    Tagged(Timing),
    Bare(f64),
}

impl From<StoredTiming> for Timing {
    fn from(s: StoredTiming) -> Timing {
        match s {
            StoredTiming::Tagged(t) => t,
            StoredTiming::Bare(secs) => Timing {
                secs,
                mtime: 0,
                size: 0,
            },
        }
    }
}

fn read_map(bytes: &[u8]) -> HashMap<String, Timing> {
    let raw: HashMap<String, StoredTiming> = serde_json::from_slice(bytes).unwrap_or_default();
    raw.into_iter().map(|(k, v)| (k, v.into())).collect()
}

fn load_raw() -> HashMap<String, Timing> {
    std::fs::read(cache::file(FILE))
        .map(|b| read_map(&b))
        .unwrap_or_default()
}

/// `(mtime secs, size bytes)` of a source file, or None if it can't be read
/// (deleted/renamed). See `discover::cache::file_fingerprint` for the rationale.
fn source_fingerprint(file: &str) -> Option<(u64, u64)> {
    let md = std::fs::metadata(file).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((mtime, md.len()))
}

/// Keep only entries whose source file still matches the recorded fingerprint;
/// drop changed bodies and vanished files. Fingerprints are memoized per file,
/// so the number of `stat`s is the number of distinct test files, not tests.
/// A legacy entry (no stored fingerprint: `mtime == 0 && size == 0`) is kept
/// while its source file still exists — it loses the deleted-file rot at once
/// and gains full staleness detection the next time it is saved with a real
/// fingerprint.
fn fresh(map: HashMap<String, Timing>) -> HashMap<String, Timing> {
    let mut fp: HashMap<String, Option<(u64, u64)>> = HashMap::new();
    map.into_iter()
        .filter(|(id, t)| {
            let file = crate::text::nodeid_file(id);
            let cur = *fp
                .entry(file.to_string())
                .or_insert_with(|| source_fingerprint(file));
            if t.mtime == 0 && t.size == 0 {
                cur.is_some()
            } else {
                cur == Some((t.mtime, t.size))
            }
        })
        .collect()
}

/// Fold `add` (id -> seconds) into `cache`, tagging each with the current
/// fingerprint of its source file, then persist. Shared by `save` and
/// `overlay_remote`.
fn persist(mut cache: HashMap<String, Timing>, add: impl Iterator<Item = (String, f64)>) {
    let mut fp: HashMap<String, Option<(u64, u64)>> = HashMap::new();
    for (id, secs) in add {
        let file = crate::text::nodeid_file(&id);
        let (mtime, size) = fp
            .entry(file.to_string())
            .or_insert_with(|| source_fingerprint(file))
            .unwrap_or((0, 0));
        cache.insert(id, Timing { secs, mtime, size });
    }
    if cache.is_empty() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = cache::write_atomic(&cache::file(FILE), &bytes);
    }
}

/// Per-project wall-clock cache: `.rstest_cache/wall.json`. Unlike
/// `durations.json` (call phase only), this captures the whole suite's elapsed
/// time — fixture setup/teardown included — so the monorepo planner can weight a
/// fixture-bound project by its real cost rather than its near-zero call time.
/// See `mono::project_cost`.
pub const WALL_FILE: &str = "wall.json";

/// Recorded wall time plus the epoch it was stamped at, for TTL aging. A whole
/// suite aggregate has no per-file source to fingerprint the way durations do,
/// so it ages out on time instead (issue #18).
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Wall {
    secs: f64,
    /// Unix epoch when recorded. `#[serde(default)]` keeps a legacy bare-float
    /// file readable; a 0 epoch (legacy, or clock-before-1970) is never aged out
    /// — the next run restamps it with a real epoch, after which it ages
    /// normally, so an upgrade never drops a still-fresh wall for one run.
    #[serde(default)]
    epoch: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredWall {
    Tagged(Wall),
    Bare(f64),
}

/// Record this run's total wall time for the cwd project.
pub fn save_wall(secs: f64) {
    let w = Wall {
        secs,
        epoch: crate::time::now_epoch_secs(),
    };
    if let Ok(bytes) = serde_json::to_vec(&w) {
        let _ = cache::write_atomic(&cache::file(WALL_FILE), &bytes);
    }
}

/// Seconds a recorded wall time stays usable. Override with
/// `RSTEST_WALL_TTL_DAYS`; `0` disables aging (keep forever).
fn wall_ttl_secs() -> u64 {
    parse_ttl_days(std::env::var("RSTEST_WALL_TTL_DAYS").ok())
}

/// Pure TTL parse: days string -> seconds, defaulting to 30 days when unset or
/// unparseable. Split out so the policy is testable without env.
fn parse_ttl_days(raw: Option<String>) -> u64 {
    const DEFAULT_DAYS: u64 = 30;
    raw.and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DAYS)
        .saturating_mul(24 * 60 * 60)
}

/// `max_age == 0` disables aging. A 0 `epoch` (legacy/unstamped) is never aged
/// out; the next run restamps it. `saturating_sub` keeps a future-epoch (skew)
/// entry rather than wiping it, matching `flakes::retain`.
fn wall_expired(epoch: u64, now: u64, max_age: u64) -> bool {
    max_age != 0 && epoch != 0 && now.saturating_sub(epoch) > max_age
}

/// Last run's wall seconds for `project`, if recorded and still within the TTL.
pub fn load_wall_in(project: &Path) -> Option<f64> {
    let bytes = std::fs::read(cache::file_in(project, WALL_FILE)).ok()?;
    let w: Wall = match serde_json::from_slice::<StoredWall>(&bytes).ok()? {
        StoredWall::Tagged(w) => w,
        StoredWall::Bare(secs) => Wall { secs, epoch: 0 },
    };
    (!wall_expired(w.epoch, crate::time::now_epoch_secs(), wall_ttl_secs())).then_some(w.secs)
}

/// Sum of cached call durations for `project`, tolerant of both on-disk formats.
/// The monorepo planner's fallback cost for caches written before wall tracking.
/// Not fingerprint-validated: `project` need not be the cwd, so its nodeid-
/// relative paths would not resolve here, and the sum is only a coarse weight.
pub fn sum_secs_in(project: &Path) -> Option<f64> {
    let bytes = std::fs::read(cache::file_in(project, FILE)).ok()?;
    Some(read_map(&bytes).values().map(|t| t.secs).sum())
}

/// Load the duration cache for scheduling: nodeid -> call seconds, with stale
/// and vanished entries dropped (see the module and `fresh` docs).
pub fn load() -> HashMap<String, f64> {
    fresh(load_raw())
        .into_iter()
        .map(|(k, t)| (k, t.secs))
        .collect()
}

pub fn save(run: &Run) {
    // Merge over the previous cache: tests not in this run keep old timings
    // (-k/-m filtered runs must not wipe the rest of the suite's data). `fresh`
    // prunes stale/vanished entries in the same pass, so the file self-heals on
    // disk, not only on read.
    persist(
        fresh(load_raw()),
        run.durations().map(|(id, d)| (id.clone(), d)),
    );
}

/// Land remote-pulled durations into the local cache. The remote stores bare
/// seconds (fingerprints are local-fs facts, meaningless across machines), so
/// each pulled entry is tagged with the local source file's current fingerprint
/// as it lands and then self-heals locally like a natively recorded one. Remote
/// wins on shared keys; stale locals are pruned in the same pass.
pub fn overlay_remote(remote: &HashMap<String, f64>) {
    persist(
        fresh(load_raw()),
        remote.iter().map(|(k, v)| (k.clone(), *v)),
    );
}

/// Items with a cached duration above this run first, longest first.
pub const SLOW_THRESHOLD_SECS: f64 = 1.0;

/// --durations-regress rows: (nodeid, baseline, current), worst absolute
/// growth first. Flags when new >= ratio*baseline AND baseline >= 50ms AND
/// growth >= 0.5s (jitter floors). Tests absent from baseline never flag.
pub fn regressions(
    run: &Run,
    baseline: &HashMap<String, f64>,
    ratio: f64,
) -> Vec<(String, f64, f64)> {
    let mut rows: Vec<(String, f64, f64)> = run
        .durations()
        .filter_map(|(id, new)| {
            let &old = baseline.get(id)?;
            (old >= 0.05 && new >= old * ratio && new - old >= 0.5).then(|| (id.clone(), old, new))
        })
        .collect();
    rows.sort_by(|a, b| (b.2 - b.1).total_cmp(&(a.2 - a.1)));
    rows
}

/// Build the dispatch order: slow long-poles first (individually, longest
/// first, so they spread across workers immediately), then everything else
/// in collection order (contiguous = module locality preserved).
pub fn dispatch_order(ids: &[String], cache: &HashMap<String, f64>) -> Vec<u64> {
    if cache.is_empty() {
        return (0..ids.len() as u64).collect();
    }
    let mut slow: Vec<(u64, f64)> = Vec::new();
    let mut rest: Vec<u64> = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        match cache.get(id) {
            Some(&d) if d >= SLOW_THRESHOLD_SECS => slow.push((i as u64, d)),
            _ => rest.push(i as u64),
        }
    }
    slow.sort_by(|a, b| b.1.total_cmp(&a.1));
    slow.into_iter().map(|(i, _)| i).chain(rest).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cache_keeps_collection_order() {
        let ids: Vec<String> = (0..4).map(|i| format!("t{i}")).collect();
        assert_eq!(dispatch_order(&ids, &HashMap::new()), vec![0, 1, 2, 3]);
    }

    #[test]
    fn regression_rows_respect_floors() {
        let mut run = Run::default();
        for (id, d) in [
            ("t/a.py::slow", 2.0),         // 4x over 0.5s baseline -> flags
            ("t/a.py::micro", 0.02),       // baseline under 50ms floor
            ("t/a.py::brand_new", 3.0),    // absent from baseline
            ("t/a.py::small_growth", 0.9), // growth under 0.5s floor
        ] {
            run.record(
                None,
                crate::scheduling::proto::Report {
                    nodeid: id.into(),
                    when: "call".into(),
                    outcome: "passed".into(),
                    duration: d,
                    longrepr: None,
                    wasxfail: false,
                    skip_reason: None,
                    cpu: None,
                    thread_delta: None,
                    fd_delta: None,
                    sections: Vec::new(),
                    lineno: None,
                },
            );
        }
        let mut base = HashMap::new();
        base.insert("t/a.py::slow".to_string(), 0.5);
        base.insert("t/a.py::micro".to_string(), 0.005);
        base.insert("t/a.py::small_growth".to_string(), 0.42);
        let rows = regressions(&run, &base, 2.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "t/a.py::slow");
        assert_eq!(rows[0].1, 0.5);
    }

    #[test]
    fn long_poles_first_longest_first() {
        let ids: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        let mut cache = HashMap::new();
        cache.insert("t1".to_string(), 2.0);
        cache.insert("t3".to_string(), 9.0);
        cache.insert("t0".to_string(), 0.2); // under threshold: stays put
        assert_eq!(dispatch_order(&ids, &cache), vec![3, 1, 0, 2, 4]);
    }

    // --- invalidation --------------------------------------------------------

    #[test]
    fn read_map_accepts_legacy_bare_and_tagged() {
        // Legacy: bare floats, no fingerprint -> mtime/size 0.
        let legacy = read_map(br#"{"a::t":1.5}"#);
        assert_eq!(legacy["a::t"].secs, 1.5);
        assert_eq!((legacy["a::t"].mtime, legacy["a::t"].size), (0, 0));
        // Tagged: full object round-trips.
        let tagged = read_map(br#"{"a::t":{"secs":2.0,"mtime":7,"size":42}}"#);
        assert_eq!(tagged["a::t"].secs, 2.0);
        assert_eq!((tagged["a::t"].mtime, tagged["a::t"].size), (7, 42));
        // Garbage degrades to empty, never a panic.
        assert!(read_map(b"not json").is_empty());
    }

    /// A unique temp file whose path is used as the nodeid's file part, so
    /// `fresh` fingerprints a real file without depending on the cwd.
    fn temp_source(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rstest-dur-{}-{}-{tag}.py",
            std::process::id(),
            crate::time::now_epoch_nanos()
        ));
        std::fs::write(&p, body).unwrap();
        p
    }

    fn tagged(secs: f64, fp: Option<(u64, u64)>) -> Timing {
        let (mtime, size) = fp.unwrap_or((0, 0));
        Timing { secs, mtime, size }
    }

    #[test]
    fn fresh_keeps_matching_drops_changed_and_vanished() {
        let file = temp_source("keep", b"def test_a(): pass\n");
        let id = format!("{}::test_a", file.display());
        let fp = source_fingerprint(file.to_str().unwrap());
        assert!(fp.is_some());

        // Matching fingerprint survives.
        let mut m = HashMap::new();
        m.insert(id.clone(), tagged(1.0, fp));
        assert_eq!(fresh(m).len(), 1);

        // Edited body (size changes) -> stale fingerprint dropped.
        std::fs::write(&file, b"def test_a(): return 12345\n").unwrap();
        let mut m = HashMap::new();
        m.insert(id.clone(), tagged(1.0, fp));
        assert!(fresh(m).is_empty());

        // Deleted file -> dropped even though the entry looks tagged.
        std::fs::remove_file(&file).unwrap();
        let mut m = HashMap::new();
        m.insert(id.clone(), tagged(1.0, fp));
        assert!(fresh(m).is_empty());
    }

    #[test]
    fn fresh_legacy_kept_while_file_exists_dropped_when_gone() {
        let file = temp_source("legacy", b"def test_b(): pass\n");
        let live = format!("{}::test_b", file.display());
        let dead = "does/not/exist_test.py::test_c".to_string();
        let mut m = HashMap::new();
        m.insert(live.clone(), tagged(1.0, None)); // legacy (0,0)
        m.insert(dead, tagged(1.0, None)); // legacy, file gone
        let kept = fresh(m);
        assert_eq!(kept.len(), 1);
        assert!(kept.contains_key(&live));
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn wall_ttl_parse_defaults_and_disables() {
        const DAY: u64 = 24 * 60 * 60;
        assert_eq!(parse_ttl_days(None), 30 * DAY);
        assert_eq!(parse_ttl_days(Some("garbage".into())), 30 * DAY);
        assert_eq!(parse_ttl_days(Some("7".into())), 7 * DAY);
        assert_eq!(parse_ttl_days(Some("0".into())), 0);
    }

    #[test]
    fn wall_expiry_respects_window_zero_and_skew() {
        const DAY: u64 = 24 * 60 * 60;
        let now = 100 * DAY;
        assert!(wall_expired(now - 31 * DAY, now, 30 * DAY)); // older than window
        assert!(!wall_expired(now - 29 * DAY, now, 30 * DAY)); // inside window
        assert!(!wall_expired(now - 31 * DAY, now, 0)); // TTL 0 = keep forever
        assert!(!wall_expired(0, now, 30 * DAY)); // legacy/unstamped epoch kept
        assert!(!wall_expired(now + DAY, now, 30 * DAY)); // future epoch (skew) kept
    }

    #[test]
    fn stored_wall_accepts_legacy_bare_and_tagged() {
        let bare: StoredWall = serde_json::from_slice(b"12.5").unwrap();
        assert!(matches!(bare, StoredWall::Bare(s) if s == 12.5));
        let tag: StoredWall = serde_json::from_slice(br#"{"secs":9.0,"epoch":5}"#).unwrap();
        assert!(matches!(tag, StoredWall::Tagged(w) if w.secs == 9.0 && w.epoch == 5));
    }
}
