//! Per-test run history: `.rstest_cache/history.msgpack` in the cwd.
//! Every other cache keeps the latest value per test; this one keeps a bounded
//! series (the last `RSTEST_HISTORY_RUNS` runs, default 50) so `explain` can
//! show an outcome strip, duration spread and the commit a test broke at.
//!
//! Run-level context (commit, env fingerprint, worker count) is stored once per
//! run; each test entry points at its run by `seq`. Aged by the same
//! `RSTEST_FLAKE_RETENTION_DAYS` window as `flakes.json`.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::cache;
use crate::reporting::report::{Run, TestEntry};

pub const FILE: &str = "history.msgpack";
const SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMeta {
    /// Monotonic id within this file; entries reference it.
    pub seq: u32,
    /// The run's uid (same id as the replay journal / cache segment).
    pub id: String,
    pub epoch: u64,
    /// `git rev-parse HEAD` at run start; `None` outside a git checkout.
    pub commit: Option<String>,
    /// [`crate::incremental::env_fingerprint`] at run start.
    pub env: String,
    pub workers: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// [`RunMeta::seq`] of the run that produced this entry.
    pub run: u32,
    /// [`TestEntry::outcome`] bucket, or `"flaky"` / `"crashed"`.
    pub outcome: String,
    /// Call-phase seconds; `None` when the call phase never ran.
    pub call: Option<f32>,
    pub setup: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct History {
    schema: u32,
    pub runs: Vec<RunMeta>,
    /// Oldest first.
    pub tests: HashMap<String, VecDeque<Entry>>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            runs: Vec::new(),
            tests: HashMap::new(),
        }
    }
}

impl History {
    /// `nodeid`'s entries paired with their run, oldest first.
    pub fn series(&self, nodeid: &str) -> Vec<(&RunMeta, &Entry)> {
        let runs: HashMap<u32, &RunMeta> = self.runs.iter().map(|r| (r.seq, r)).collect();
        self.tests
            .get(nodeid)
            .into_iter()
            .flatten()
            .filter_map(|e| runs.get(&e.run).map(|r| (*r, e)))
            .collect()
    }

    /// Add one run. Each test's series is trimmed to the newest `cap` entries
    /// and runs no entry points at any more are dropped.
    fn append(&mut self, mut meta: RunMeta, entries: Vec<(String, Entry)>, cap: usize) {
        meta.seq = self.runs.last().map_or(0, |r| r.seq.wrapping_add(1));
        for (id, mut e) in entries {
            e.run = meta.seq;
            let s = self.tests.entry(id).or_default();
            s.push_back(e);
            while s.len() > cap {
                s.pop_front();
            }
        }
        self.runs.push(meta);
        self.gc_runs();
    }

    /// Drop entries from runs older than `max_age` (`0` keeps everything).
    /// `saturating_sub`: a future epoch (clock skew) is kept, never wiped.
    fn age(&mut self, now: u64, max_age: u64) {
        if max_age == 0 {
            return;
        }
        let expired: HashSet<u32> = self
            .runs
            .iter()
            .filter(|r| now.saturating_sub(r.epoch) > max_age)
            .map(|r| r.seq)
            .collect();
        if expired.is_empty() {
            return;
        }
        self.tests.retain(|_, s| {
            s.retain(|e| !expired.contains(&e.run));
            !s.is_empty()
        });
        self.gc_runs();
    }

    fn gc_runs(&mut self) {
        let live: HashSet<u32> = self.tests.values().flatten().map(|e| e.run).collect();
        self.runs.retain(|r| live.contains(&r.seq));
    }
}

/// Entries kept per test. `RSTEST_HISTORY_RUNS`, default 50, minimum 1.
fn cap() -> usize {
    parse_cap(std::env::var("RSTEST_HISTORY_RUNS").ok())
}

fn parse_cap(raw: Option<String>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(50)
        .max(1)
}

/// One test's entry for this run, or `None` when the outcome isn't a real
/// result: an interrupt stopped it mid-run, or `--incremental` carried a past
/// pass forward without running it.
fn entry_for(e: &TestEntry) -> Option<Entry> {
    if e.interrupted || e.cached {
        return None;
    }
    let outcome = if e.crashed {
        "crashed"
    } else if e.flaky {
        "flaky"
    } else {
        e.outcome()
    };
    Some(Entry {
        run: 0,
        outcome: outcome.to_string(),
        call: e.duration.map(|d| d as f32),
        setup: e.setup_seconds as f32,
    })
}

pub fn load() -> History {
    load_from(&cache::file(FILE))
}

/// `load` against an explicit path, aged like `flakes.json`. Missing, corrupt
/// or other-schema files read as empty: this is a best-effort cache.
pub fn load_from(path: &std::path::Path) -> History {
    let mut h: History = std::fs::read(path)
        .ok()
        .and_then(|b| rmp_serde::from_slice::<History>(&b).ok())
        .filter(|h| h.schema == SCHEMA)
        .unwrap_or_default();
    h.age(
        crate::time::now_epoch_secs(),
        crate::reporting::flakes::retention_secs(),
    );
    h
}

/// Append this run to the stored history. Best-effort: IO errors are ignored.
pub fn record(run: &Run, id: &str, epoch: u64, commit: Option<&str>, env: &str, workers: usize) {
    let entries: Vec<(String, Entry)> = run
        .tests()
        .iter()
        .filter_map(|(id, e)| entry_for(e).map(|en| (id.clone(), en)))
        .collect();
    if entries.is_empty() {
        return;
    }
    let meta = RunMeta {
        seq: 0,
        id: id.to_string(),
        epoch,
        commit: commit.map(str::to_string),
        env: env.to_string(),
        workers,
    };
    // Locked load→append→write: two concurrent sessions in one cwd would
    // otherwise drop one run's entries.
    cache::with_lock(|| {
        let mut h = load();
        h.append(meta, entries, cap());
        if let Ok(bytes) = rmp_serde::to_vec(&h) {
            let _ = cache::write_atomic(&cache::file(FILE), &bytes);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(epoch: u64) -> RunMeta {
        RunMeta {
            seq: 0,
            id: format!("run{epoch}"),
            epoch,
            commit: Some(format!("c{epoch}")),
            env: "fp".into(),
            workers: 4,
        }
    }

    fn entry(outcome: &str) -> Entry {
        Entry {
            run: 0,
            outcome: outcome.into(),
            call: Some(0.5),
            setup: 0.1,
        }
    }

    #[test]
    fn append_trims_each_test_to_cap_and_drops_orphan_runs() {
        let mut h = History::default();
        for i in 0..5u64 {
            let mut es = vec![("a".to_string(), entry("passed"))];
            // `b` only in the first run: its run must survive while referenced.
            if i == 0 {
                es.push(("b".to_string(), entry("failed")));
            }
            h.append(meta(i), es, 3);
        }
        let a = h.series("a");
        assert_eq!(a.len(), 3);
        assert_eq!(
            a.iter().map(|(r, _)| r.epoch).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        // Run 0 still held by `b`; run 1 referenced by nobody -> gone.
        let epochs: Vec<u64> = h.runs.iter().map(|r| r.epoch).collect();
        assert_eq!(epochs, vec![0, 2, 3, 4]);
        assert_eq!(h.series("b")[0].1.outcome, "failed");
    }

    #[test]
    fn seq_stays_monotonic_after_gc() {
        let mut h = History::default();
        h.append(meta(1), vec![("a".into(), entry("passed"))], 1);
        h.append(meta(2), vec![("a".into(), entry("passed"))], 1);
        assert_eq!(h.runs.len(), 1);
        assert_eq!(h.runs[0].seq, 1);
        h.append(meta(3), vec![("a".into(), entry("failed"))], 1);
        assert_eq!(h.runs[0].seq, 2);
        assert_eq!(h.series("a")[0].0.epoch, 3);
    }

    #[test]
    fn age_drops_old_runs_and_empty_tests() {
        let mut h = History::default();
        h.append(meta(100), vec![("old".into(), entry("passed"))], 50);
        h.append(
            meta(1000),
            vec![
                ("old".into(), entry("passed")),
                ("new".into(), entry("passed")),
            ],
            50,
        );
        h.append(meta(10), vec![("gone".into(), entry("passed"))], 50);
        h.age(1000, 500);
        assert_eq!(h.series("old").len(), 1);
        assert_eq!(h.series("new").len(), 1);
        assert!(!h.tests.contains_key("gone"));
        assert_eq!(h.runs.len(), 1);
        // 0 disables aging.
        let mut h2 = History::default();
        h2.append(meta(1), vec![("a".into(), entry("passed"))], 50);
        h2.age(u64::MAX, 0);
        assert_eq!(h2.runs.len(), 1);
    }

    #[test]
    fn entry_for_skips_unreal_outcomes_and_tags_flaky_crashed() {
        let base = || TestEntry {
            call: Some("passed".into()),
            duration: Some(0.25),
            setup_seconds: 0.5,
            ..Default::default()
        };
        assert!(entry_for(&TestEntry {
            interrupted: true,
            ..base()
        })
        .is_none());
        assert!(entry_for(&TestEntry {
            cached: true,
            ..base()
        })
        .is_none());
        let e = entry_for(&base()).unwrap();
        assert_eq!(e.outcome, "passed");
        assert_eq!(e.call, Some(0.25));
        assert_eq!(e.setup, 0.5);
        let f = entry_for(&TestEntry {
            flaky: true,
            ..base()
        })
        .unwrap();
        assert_eq!(f.outcome, "flaky");
        let c = entry_for(&TestEntry {
            crashed: true,
            call: Some("failed".into()),
            ..base()
        })
        .unwrap();
        assert_eq!(c.outcome, "crashed");
    }

    #[test]
    fn load_roundtrips_and_tolerates_garbage() {
        let dir = std::env::temp_dir().join(format!("rstest-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        assert_eq!(load_from(&path), History::default());
        std::fs::write(&path, b"not msgpack").unwrap();
        assert_eq!(load_from(&path), History::default());

        let mut h = History::default();
        let now = crate::time::now_epoch_secs();
        h.append(meta(now), vec![("a".into(), entry("passed"))], 50);
        std::fs::write(&path, rmp_serde::to_vec(&h).unwrap()).unwrap();
        assert_eq!(load_from(&path), h);

        // A future schema reads as empty, not as garbage data.
        h.schema = SCHEMA + 1;
        std::fs::write(&path, rmp_serde::to_vec(&h).unwrap()).unwrap();
        assert_eq!(load_from(&path), History::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_cap_defaults_and_floors() {
        assert_eq!(parse_cap(None), 50);
        assert_eq!(parse_cap(Some("junk".into())), 50);
        assert_eq!(parse_cap(Some("7".into())), 7);
        assert_eq!(parse_cap(Some("0".into())), 1);
    }
}
