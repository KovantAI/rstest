//! Per-test duration cache: `.rstest_cache/durations.json` in the cwd.
//! Drives long-pole-first scheduling; absent or stale entries are harmless,
//! unknown tests just keep collection order.
//!
//! Each entry records its test's source file (`src`) and the sha256 of that
//! file's contents (`hash`) when it was timed (issue #18). That is how the cache
//! self-heals: on load an entry whose file no longer hashes the same (edited
//! body) or can't be read (deleted/renamed) is dropped, so a changed test
//! re-times on fresh numbers instead of scheduling on stale ones, and vanished
//! nodeids stop accumulating (the merge-on-write below only ever *added*). The
//! fingerprint is content, not mtime, so it survives a fresh `git clone` / CI
//! checkout and a branch switch that leaves the file's bytes alone. The
//! validation is a pure function of the working tree, which is stable for the
//! duration of a run, so every shard worker restoring the cache derives the
//! same snapshot — the determinism the sharder in `shard.rs` relies on.
//!
//! `src` is stored per entry because nodeids are relative to pytest's rootdir,
//! which only the run that timed a test knows (it varies with `--rootdir`,
//! `-c` and the path arguments); a later load never has to guess it. It is kept
//! relative to the cwd (the project the cache belongs to) so a cache restored
//! into a checkout at another path still resolves. An entry whose source the
//! run could not locate is stored untagged and kept as-is, like the pre-#18
//! bare timings.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::cache;
use crate::reporting::flakes::FlakeStats;
use crate::reporting::report::Run;

pub const FILE: &str = "durations.json";

/// A cached call duration, tagged with its source file and that file's content
/// hash when recorded. Both empty marks an untagged entry (legacy bare float,
/// or a run that could not locate the source), which is never invalidated.
#[derive(Clone, Serialize, Deserialize)]
struct Timing {
    secs: f64,
    /// Source path, relative to the cwd when it lies under the same root
    /// (possibly with `..`), else absolute.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    src: String,
    /// sha256 of the newline-normalized bytes, as `select::current_sha256`, so
    /// an LF checkout and an autocrlf CRLF one hash the same.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    hash: String,
}

impl Timing {
    fn untagged(secs: f64) -> Timing {
        Timing {
            secs,
            src: String::new(),
            hash: String::new(),
        }
    }

    fn is_tagged(&self) -> bool {
        !self.src.is_empty() && !self.hash.is_empty()
    }
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
            StoredTiming::Bare(secs) => Timing::untagged(secs),
        }
    }
}

fn read_map(bytes: &[u8]) -> HashMap<String, Timing> {
    let raw: HashMap<String, StoredTiming> = serde_json::from_slice(bytes).unwrap_or_default();
    raw.into_iter().map(|(k, v)| (k, v.into())).collect()
}

/// Raw on-disk entries at `path` (missing/corrupt = empty), not yet validated.
fn load_raw_from(path: &Path) -> HashMap<String, Timing> {
    std::fs::read(path)
        .map(|b| read_map(&b))
        .unwrap_or_default()
}

/// Content hash of `path`, or None if it can't be read (deleted/renamed).
/// Memoized for the process on `(len, mtime)`: `load` runs several times per
/// run (worker sizing, dispatch, sharding) and collection fingerprints the same
/// files again, so an unchanged file is read once and afterwards only
/// `stat`ed, while an edit (new len or mtime) is re-hashed, which keeps a
/// long-lived watch/serve process current.
fn content_hash(path: &Path) -> Option<String> {
    type Memo = HashMap<PathBuf, (u64, Option<SystemTime>, String)>;
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    let md = std::fs::metadata(path).ok()?;
    let key = (md.len(), md.modified().ok());
    let memo = MEMO.get_or_init(Default::default);
    if let Some((len, mtime, hash)) = memo.lock().ok()?.get(path) {
        if (*len, *mtime) == key {
            return Some(hash.clone());
        }
    }
    let hash = crate::select::current_sha256(path)?;
    if let Ok(mut m) = memo.lock() {
        m.insert(path.to_path_buf(), (key.0, key.1, hash.clone()));
    }
    Some(hash)
}

/// `path` expressed relative to `base` (both absolute), walking up with `..`
/// past their common prefix. None when they share no root (another Windows
/// drive), in which case callers keep the absolute path.
fn relative_to(path: &Path, base: &Path) -> Option<PathBuf> {
    let (p, b): (Vec<Component>, Vec<Component>) =
        (path.components().collect(), base.components().collect());
    let common = p.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if common == 0 {
        return None;
    }
    let mut rel: PathBuf = b[common..].iter().map(|_| Component::ParentDir).collect();
    rel.extend(&p[common..]);
    Some(rel)
}

/// What the run saw at collection time: pytest's own rootdir and the content
/// hash of each collected test file, taken as the file was collected. `save`
/// tags this run's timings with these rather than re-reading the files
/// afterwards, so a file edited mid-run leaves its timings tagged with the
/// pre-edit hash, and the next load drops them as stale. Empty on paths whose
/// worker reports no collection (the single-session run); `save` then reuses
/// the source a previous run recorded for the test, else stores it untagged.
#[derive(Default)]
pub struct Collected {
    rootdir: Option<PathBuf>,
    hashes: HashMap<PathBuf, Option<String>>,
}

impl Collected {
    /// Record pytest's rootdir (`config.rootpath`); the first report wins,
    /// since every worker runs the same session.
    pub fn set_rootdir(&mut self, rootdir: &str) {
        self.rootdir.get_or_insert_with(|| PathBuf::from(rootdir));
    }

    /// Hash the source file of each collected nodeid not seen yet. A no-op
    /// until the rootdir is known, since the ids are relative to it.
    pub fn record<'a>(&mut self, ids: impl IntoIterator<Item = &'a String>) {
        let Some(root) = &self.rootdir else {
            return;
        };
        for id in ids {
            let file = root.join(crate::text::nodeid_file(id));
            if let std::collections::hash_map::Entry::Vacant(slot) = self.hashes.entry(file) {
                let hash = content_hash(slot.key());
                slot.insert(hash);
            }
        }
    }

    /// Absolute source path of `id`, when the rootdir is known.
    fn source(&self, id: &str) -> Option<PathBuf> {
        Some(self.rootdir.as_ref()?.join(crate::text::nodeid_file(id)))
    }
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Keep untagged entries and tagged ones whose source still hashes as
/// recorded; drop edited bodies and vanished files. `base` resolves a relative
/// `src` (the cwd).
fn fresh(map: HashMap<String, Timing>, base: &Path) -> HashMap<String, Timing> {
    map.into_iter()
        .filter(|(_, t)| {
            !t.is_tagged() || content_hash(&base.join(&t.src)).is_some_and(|h| h == t.hash)
        })
        .collect()
}

/// Load→prune→fold `add` (id -> seconds)→write the cache at `path`. Each added
/// entry is tagged with its source: from `collected` when the run reported its
/// rootdir (hash as collected), else the source a previous run recorded for the
/// same id (hash as it is now), else left untagged. `base` is the cwd that
/// relative `src` paths hang off. The caller holds the cache lock. Shared by
/// `save` and `overlay_remote_in`.
fn persist_to(
    path: &Path,
    add: impl Iterator<Item = (String, f64)>,
    collected: &Collected,
    base: &Path,
) {
    let raw = load_raw_from(path);
    let known: HashMap<String, String> = raw
        .iter()
        .filter(|(_, t)| !t.src.is_empty())
        .map(|(id, t)| (id.clone(), t.src.clone()))
        .collect();
    let mut cache = fresh(raw, base);
    for (id, secs) in add {
        let tagged = match collected.source(&id) {
            Some(abs) => {
                let hash = match collected.hashes.get(&abs) {
                    Some(h) => h.clone(),
                    None => content_hash(&abs),
                };
                let src = relative_to(&abs, base).unwrap_or(abs);
                hash.map(|hash| (src.to_string_lossy().into_owned(), hash))
            }
            None => known.get(&id).and_then(|src| {
                let hash = content_hash(&base.join(src))?;
                Some((src.clone(), hash))
            }),
        };
        let t = match tagged {
            Some((src, hash)) => Timing { secs, src, hash },
            None => Timing::untagged(secs),
        };
        cache.insert(id, t);
    }
    if cache.is_empty() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = cache::write_atomic(path, &bytes);
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
    load_from(&cache::file(FILE))
}

/// `load` against an explicit `durations.json` path (missing/corrupt = empty).
pub fn load_from(path: &Path) -> HashMap<String, f64> {
    secs_only(fresh(load_raw_from(path), &cwd()))
}

/// The cache as recorded, without fingerprint validation. The
/// `--durations-regress` baseline: a test in a file this change edited is
/// exactly the one whose old timing the gate must compare against, and a
/// vanished test cannot match any id in the current run anyway.
pub fn load_baseline() -> HashMap<String, f64> {
    secs_only(load_raw_from(&cache::file(FILE)))
}

fn secs_only(map: HashMap<String, Timing>) -> HashMap<String, f64> {
    map.into_iter().map(|(k, t)| (k, t.secs)).collect()
}

/// Record this run's call durations, tagged with the sources and hashes
/// `collected` took at collection time (see `Collected`).
pub fn save(run: &Run, collected: &Collected) {
    // Merge over the previous cache: tests not in this run keep old timings
    // (-k/-m filtered runs must not wipe the rest of the suite's data). `fresh`
    // prunes stale/vanished entries in the same pass, so the file self-heals on
    // disk, not only on read. Hold the cache lock across load→merge→write so a
    // concurrent process/shard sharing this cwd cache can't clobber the merge
    // with a stale snapshot.
    cache::with_lock(|| {
        persist_to(
            &cache::file(FILE),
            run.durations().map(|(id, d)| (id.clone(), d)),
            collected,
            &cwd(),
        );
    });
}

/// Land remote-pulled durations into the cache at `path`. The remote stores
/// bare seconds (fingerprints are local-fs facts, meaningless across machines),
/// so a pulled entry takes the source the local cache already recorded for
/// that test, hashed as the file is now, and then self-heals locally like a
/// natively recorded one; a test the local cache has never timed lands
/// untagged. Remote wins on shared keys; stale locals are pruned in the same
/// pass. The caller holds the cache lock.
pub fn overlay_remote_in(path: &Path, remote: &HashMap<String, f64>) {
    persist_to(
        path,
        remote.iter().map(|(k, v)| (k.clone(), *v)),
        &Collected::default(),
        &cwd(),
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

/// A test with a hard failure or a flake on record: fail-fast pulls it to the
/// front of the queue.
pub fn is_suspect(id: &str, flakes: &HashMap<String, FlakeStats>) -> bool {
    flakes.get(id).is_some_and(|f| f.failed > 0 || f.flaky > 0)
}

/// Most suspects fail-fast pulls to the front (and dispatches one at a time).
/// Bounds the damage of a mass failure: after one bad commit fails thousands
/// of tests, every later run would otherwise single-dispatch all of them for
/// the whole retention window. Suspects past the cap rejoin the clean tail.
pub const FAILFAST_SUSPECT_CAP: usize = 128;

/// Fail-fast dispatch order: surface a red as early as possible. Suspects
/// (see [`is_suspect`]) go first, hard-failed before flaky-only; within each,
/// most recent event first (`last_epoch` desc) so a test that failed last run
/// beats one that failed often but long ago (for hard-failed tests this is the
/// last FAILURE, not a later flake), then event count (desc), then
/// collection order (a same-run cohort stays module-contiguous). At most
/// [`FAILFAST_SUSPECT_CAP`] suspects lead. Everything after is exactly
/// [`dispatch_order`]: long poles longest-first, then collection order, so
/// clean tests keep module locality and workers still pack. The caller
/// dispatches the suspect + long-pole prefix one test at a time. Only ids
/// passing `include` are returned. Pairs with `--maxfail`/`-x` for true early
/// exit.
pub fn failfast_order(
    ids: &[String],
    cache: &HashMap<String, f64>,
    flakes: &HashMap<String, FlakeStats>,
    include: impl Fn(u64) -> bool,
) -> Vec<u64> {
    // Filter (shard / serial) BEFORE ranking and capping, so the cap counts
    // only suspects this run will actually dispatch.
    let (mut suspects, mut clean): (Vec<u64>, Vec<u64>) = (0..ids.len() as u64)
        .filter(|&i| include(i))
        .partition(|&i| is_suspect(&ids[i as usize], flakes));
    // Recency: last hard failure for failed tests, last flake otherwise.
    let recency = |f: &FlakeStats| {
        if f.failed > 0 {
            f.failed_epoch()
        } else {
            f.last_epoch
        }
    };
    // Stable sort: full ties keep collection order.
    suspects.sort_by(|&a, &b| {
        let (fa, fb) = (flakes[&ids[a as usize]], flakes[&ids[b as usize]]);
        (fa.failed == 0)
            .cmp(&(fb.failed == 0))
            .then(recency(&fb).cmp(&recency(&fa)))
            .then(fb.failed.cmp(&fa.failed))
            .then(fb.flaky.cmp(&fa.flaky))
    });
    if suspects.len() > FAILFAST_SUSPECT_CAP {
        clean.extend(suspects.drain(FAILFAST_SUSPECT_CAP..));
        clean.sort_unstable();
    }
    // Clean tail: reuse throughput ordering over the clean subset.
    let clean_ids: Vec<String> = clean.iter().map(|&i| ids[i as usize].clone()).collect();
    suspects
        .into_iter()
        .chain(
            dispatch_order(&clean_ids, cache)
                .into_iter()
                .map(|j| clean[j as usize]),
        )
        .collect()
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
    fn failfast_orders_failed_then_flaky_then_throughput_tail() {
        use crate::reporting::flakes::FlakeStats;
        let names: Vec<String> = ["failed", "flaky", "fast", "slow"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cache = HashMap::from([("fast".to_string(), 0.1), ("slow".to_string(), 9.0)]);
        let flakes = HashMap::from([
            (
                "failed".to_string(),
                FlakeStats {
                    failed: 2,
                    ..Default::default()
                },
            ),
            (
                "flaky".to_string(),
                FlakeStats {
                    flaky: 3,
                    ..Default::default()
                },
            ),
        ]);
        // index 0=failed, 1=flaky, 2=fast, 3=slow
        // hard-failed first, then flaky, then the clean tail in throughput
        // order: the long pole first, then collection order.
        assert_eq!(
            failfast_order(&names, &cache, &flakes, |_| true),
            vec![0, 1, 3, 2]
        );
    }

    #[test]
    fn failfast_prefers_recent_failure_over_stale_repeat_failure() {
        use crate::reporting::flakes::FlakeStats;
        let names: Vec<String> = ["stale", "recent", "recent_flaky"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cache = HashMap::new();
        let flakes = HashMap::from([
            // Failed often, but long ago (and since fixed).
            (
                "stale".to_string(),
                FlakeStats {
                    failed: 5,
                    last_epoch: 100,
                    ..Default::default()
                },
            ),
            // Failed once, last run.
            (
                "recent".to_string(),
                FlakeStats {
                    failed: 1,
                    last_epoch: 900,
                    ..Default::default()
                },
            ),
            // Flaked last run: newer, but a flake still ranks below any failure.
            (
                "recent_flaky".to_string(),
                FlakeStats {
                    flaky: 4,
                    last_epoch: 1000,
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(
            failfast_order(&names, &cache, &flakes, |_| true),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn failfast_caps_suspects_and_keeps_cohort_in_collection_order() {
        use crate::reporting::flakes::FlakeStats;
        // A mass failure: every test failed in the same run (full tie), and a
        // later-collected test failed alone in a newer run.
        let n = FAILFAST_SUSPECT_CAP + 10;
        let names: Vec<String> = (0..n).map(|i| format!("m{}.py::t{i}", i % 3)).collect();
        let mut flakes: HashMap<String, FlakeStats> = names
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    FlakeStats {
                        failed: 1,
                        last_epoch: 100,
                        ..Default::default()
                    },
                )
            })
            .collect();
        flakes.get_mut(&names[n - 1]).unwrap().last_epoch = 200;
        let order = failfast_order(&names, &HashMap::new(), &flakes, |_| true);
        assert_eq!(order.len(), n, "a permutation, nothing lost");
        // Newest failure leads, then the tied cohort in collection order
        // (no duration shuffling), capped; overflow rejoins in collection order.
        assert_eq!(order[0], (n - 1) as u64);
        let expected: Vec<u64> = std::iter::once((n - 1) as u64)
            .chain(0..(n - 1) as u64)
            .collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn failfast_ranks_hard_failures_by_last_failure_not_last_flake() {
        use crate::reporting::flakes::FlakeStats;
        let names: Vec<String> = ["old_fail_new_flake", "new_fail"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let flakes = HashMap::from([
            // Hard-failed long ago, flaked last run.
            (
                "old_fail_new_flake".to_string(),
                FlakeStats {
                    failed: 1,
                    flaky: 1,
                    last_epoch: 1000,
                    last_failed_epoch: 100,
                },
            ),
            // Hard-failed more recently than the other's failure.
            (
                "new_fail".to_string(),
                FlakeStats {
                    failed: 1,
                    last_epoch: 900,
                    last_failed_epoch: 900,
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(
            failfast_order(&names, &HashMap::new(), &flakes, |_| true),
            vec![1, 0]
        );
    }

    #[test]
    fn failfast_caps_after_filtering() {
        use crate::reporting::flakes::FlakeStats;
        // 2*CAP suspects; the filter keeps only odd indices (a shard). The cap
        // must count kept suspects, so all CAP kept ones lead.
        let n = 2 * FAILFAST_SUSPECT_CAP + 2;
        let names: Vec<String> = (0..n).map(|i| format!("a.py::t{i}")).collect();
        let flakes: HashMap<String, FlakeStats> = names
            .iter()
            .enumerate()
            .map(|(i, id)| {
                (
                    id.clone(),
                    FlakeStats {
                        failed: 1,
                        // Even (filtered-out) ones are newest: a pre-filter
                        // cap would spend every slot on them.
                        last_epoch: if i % 2 == 0 { 200 } else { 100 },
                        ..Default::default()
                    },
                )
            })
            .collect();
        let order = failfast_order(&names, &HashMap::new(), &flakes, |i| i % 2 == 1);
        assert_eq!(order.len(), n / 2);
        assert!(order.iter().all(|i| i % 2 == 1));
        let expected: Vec<u64> = (0..n as u64).filter(|i| i % 2 == 1).collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn failfast_no_signal_matches_throughput_order() {
        use crate::reporting::flakes::FlakeStats;
        let names: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        // Cold flakes.json: nothing to pull forward, so the order is exactly
        // the throughput one (long pole b first, then collection order).
        let cache = HashMap::from([("a".to_string(), 0.5), ("b".to_string(), 3.0)]);
        let flakes: HashMap<String, FlakeStats> = HashMap::new();
        assert_eq!(
            failfast_order(&names, &cache, &flakes, |_| true),
            vec![1, 0, 2]
        );
        assert_eq!(
            failfast_order(&names, &cache, &flakes, |_| true),
            dispatch_order(&names, &cache)
        );
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
        // Legacy: bare floats -> untagged.
        let legacy = read_map(br#"{"a::t":1.5}"#);
        assert_eq!(legacy["a::t"].secs, 1.5);
        assert!(!legacy["a::t"].is_tagged());
        // Tagged: full object round-trips.
        let tagged = read_map(br#"{"a::t":{"secs":2.0,"src":"a.py","hash":"ab"}}"#);
        assert_eq!(tagged["a::t"].secs, 2.0);
        assert_eq!(
            (tagged["a::t"].src.as_str(), tagged["a::t"].hash.as_str()),
            ("a.py", "ab")
        );
        // Garbage degrades to empty, never a panic.
        assert!(read_map(b"not json").is_empty());
    }

    /// A unique temp dir standing in for the cwd / rootdir, so tests resolve
    /// real files without depending on the process cwd.
    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rstest-dur-{}-{}-{tag}",
            std::process::id(),
            crate::time::now_epoch_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tagged(secs: f64, src: &str, hash: Option<String>) -> Timing {
        Timing {
            secs,
            src: src.to_string(),
            hash: hash.unwrap_or_default(),
        }
    }

    fn collected_at(root: &Path, ids: &[&String]) -> Collected {
        let mut c = Collected::default();
        c.set_rootdir(root.to_str().unwrap());
        c.record(ids.iter().copied());
        c
    }

    #[test]
    fn fresh_keeps_matching_drops_changed_and_vanished() {
        let root = temp_root("keep");
        let file = root.join("test_a.py");
        std::fs::write(&file, b"def test_a(): pass\n").unwrap();
        let h = content_hash(&file);
        assert!(h.is_some());
        let one = || {
            HashMap::from([(
                "test_a.py::t".to_string(),
                tagged(1.0, "test_a.py", h.clone()),
            )])
        };

        // Matching hash survives.
        assert_eq!(fresh(one(), &root).len(), 1);

        // Same-size edit: only the content hash catches it.
        std::fs::write(&file, b"def test_a(): pas5\n").unwrap();
        assert!(fresh(one(), &root).is_empty());

        // Deleted file -> dropped.
        std::fs::remove_file(&file).unwrap();
        assert!(fresh(one(), &root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fresh_survives_mtime_and_line_ending_changes() {
        // A fresh checkout restamps mtimes; autocrlf rewrites LF as CRLF. Neither
        // changes the (newline-normalized) content hash.
        let root = temp_root("mtime");
        let file = root.join("test_m.py");
        std::fs::write(&file, b"def test_m():\n    pass\n").unwrap();
        let h = content_hash(&file);
        std::fs::write(&file, b"def test_m():\r\n    pass\r\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        let m = HashMap::from([("test_m.py::t".to_string(), tagged(1.0, "test_m.py", h))]);
        assert_eq!(fresh(m, &root).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fresh_keeps_untagged_entries_unconditionally() {
        // No recorded source (legacy, or a run that could not locate it): never
        // invalidated, whatever the working tree looks like.
        let root = temp_root("untagged");
        let m = HashMap::from([("gone.py::t".to_string(), Timing::untagged(1.0))]);
        assert_eq!(fresh(m, &root).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn entries_from_different_rootdirs_coexist() {
        // Two runs with different rootdirs (no ini, different path args) record
        // the same relative nodeid shape; each entry carries its own source, so
        // the second save must not prune the first.
        let base = temp_root("roots");
        for pkg in ["pkg_a/tests", "pkg_b/tests"] {
            std::fs::create_dir_all(base.join(pkg)).unwrap();
        }
        std::fs::write(base.join("pkg_a/tests/test_x.py"), b"a\n").unwrap();
        std::fs::write(base.join("pkg_b/tests/test_y.py"), b"b\n").unwrap();
        let path = base.join(FILE);
        let (x, y) = ("test_x.py::t".to_string(), "test_y.py::t".to_string());
        let run_a = collected_at(&base.join("pkg_a/tests"), &[&x]);
        persist_to(&path, [(x.clone(), 1.0)].into_iter(), &run_a, &base);
        let run_b = collected_at(&base.join("pkg_b/tests"), &[&y]);
        persist_to(&path, [(y.clone(), 2.0)].into_iter(), &run_b, &base);

        let raw = load_raw_from(&path);
        assert_eq!(
            raw[&x].src, "pkg_a/tests/test_x.py",
            "stored relative to the cwd"
        );
        let got = secs_only(fresh(raw, &base));
        assert_eq!((got[&x], got[&y]), (1.0, 2.0));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn source_above_cwd_is_stored_with_parent_components() {
        // Run from a subdirectory with the rootdir above it.
        let root = temp_root("above");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("test_r.py"), b"r\n").unwrap();
        let id = "test_r.py::t".to_string();
        let cwd = root.join("sub");
        let path = cwd.join(FILE);
        persist_to(
            &path,
            [(id.clone(), 1.0)].into_iter(),
            &collected_at(&root, &[&id]),
            &cwd,
        );
        let raw = load_raw_from(&path);
        assert_eq!(raw[&id].src, "../test_r.py");
        assert_eq!(fresh(raw, &cwd).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_without_rootdir_reuses_known_source_else_untagged() {
        // Single-session / remote overlay: no collection report. A test with a
        // previously recorded source keeps being validated; a new one lands
        // untagged and is kept rather than dropped on the next load.
        let root = temp_root("norootdir");
        std::fs::write(root.join("test_k.py"), b"k\n").unwrap();
        let (k, n) = ("test_k.py::t".to_string(), "test_n.py::t".to_string());
        let path = root.join(FILE);
        persist_to(
            &path,
            [(k.clone(), 1.0)].into_iter(),
            &collected_at(&root, &[&k]),
            &root,
        );
        persist_to(
            &path,
            [(k.clone(), 3.0), (n.clone(), 4.0)].into_iter(),
            &Collected::default(),
            &root,
        );
        let raw = load_raw_from(&path);
        assert!(raw[&k].is_tagged() && raw[&k].secs == 3.0);
        assert!(!raw[&n].is_tagged());
        assert_eq!(fresh(raw, &root).len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_tags_with_collection_time_hash() {
        // A file edited after collection: its timing is tagged with the
        // pre-edit hash, so the next load drops it. A touched-but-unchanged
        // file keeps its timing.
        let root = temp_root("collected");
        std::fs::write(root.join("test_e.py"), b"def test_e(): pass\n").unwrap();
        std::fs::write(root.join("test_s.py"), b"def test_s(): pass\n").unwrap();
        let (e, s) = ("test_e.py::t".to_string(), "test_s.py::t".to_string());
        let collected = collected_at(&root, &[&e, &s]);

        std::fs::write(root.join("test_e.py"), b"def test_e(): return 1\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(root.join("test_s.py"))
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();

        let path = root.join(FILE);
        persist_to(
            &path,
            [(e, 1.0), (s.clone(), 2.0)].into_iter(),
            &collected,
            &root,
        );
        let got = secs_only(fresh(load_raw_from(&path), &root));
        assert_eq!(got.len(), 1, "test edited after collection re-times");
        assert_eq!(got[&s], 2.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn collected_ignores_ids_before_rootdir_is_known() {
        let mut c = Collected::default();
        c.record([&"test_x.py::t".to_string()]);
        assert!(c.hashes.is_empty());
    }

    #[test]
    fn relative_to_walks_up_and_down() {
        let r = |p: &str, b: &str| relative_to(Path::new(p), Path::new(b));
        assert_eq!(r("/a/b/c.py", "/a"), Some(PathBuf::from("b/c.py")));
        assert_eq!(r("/a/c.py", "/a/b"), Some(PathBuf::from("../c.py")));
        assert_eq!(
            r("/a/x/c.py", "/a/b/d"),
            Some(PathBuf::from("../../x/c.py"))
        );
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
