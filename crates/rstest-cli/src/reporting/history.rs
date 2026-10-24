//! Per-test run history: `.rstest_cache/history.msgpack` in the cwd.
//! Every other cache keeps the latest value per test; this one keeps a bounded
//! series (the last `RSTEST_HISTORY_RUNS` runs, default 50) so `explain` can
//! show an outcome strip, duration spread and the commit a test broke at.
//!
//! Run-level context (epoch, commit, env fingerprint) is stored once per run;
//! each test entry points at its run by `seq`. Aged by the same
//! `RSTEST_FLAKE_RETENTION_DAYS` window as `flakes.json`.
//!
//! `--cache-push` publishes each run as a [`RunSlice`]; pull and compaction
//! fold slices back with [`History::merged`], which dedupes by run id, so the
//! same run arriving from the local file, a segment and the base lands once.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::cache;
use crate::reporting::report::{Run, TestEntry};

pub const FILE: &str = "history.msgpack";
const SCHEMA: u32 = 1;

/// Outcome names, indexed by [`Entry::outcome`]. Append only: the index is
/// what's on disk.
const OUTCOMES: [&str; 9] = [
    "passed",
    "failed",
    "errors",
    "skipped",
    "xfailed",
    "xpassed",
    "quarantined",
    "flaky",
    "crashed",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMeta {
    /// Monotonic id within this file; entries reference it. `u64` so it never
    /// wraps into a still-referenced id.
    pub seq: u64,
    /// The run's uid: what [`History::merged`] dedupes on, so one run pulled
    /// from several places lands once.
    pub id: String,
    pub epoch: u64,
    /// `git rev-parse HEAD` at run start, suffixed `-dirty` when the working
    /// tree had uncommitted changes then; `None` outside a git checkout.
    pub commit: Option<String>,
    /// [`crate::incremental::env_fingerprint`] at run start. Last and
    /// defaulted, so a file or segment written before it existed still loads.
    #[serde(default)]
    pub env: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// [`RunMeta::seq`] of the run that produced this entry.
    pub run: u64,
    /// Index into [`OUTCOMES`]: a [`TestEntry::outcome`] bucket, or `flaky` /
    /// `crashed`. A byte, not the name: this file holds tests x runs entries.
    pub outcome: u8,
    /// Call-phase seconds; `None` when the call phase never ran.
    pub call: Option<f32>,
    pub setup: f32,
}

/// One run's contribution: what `record` adds locally and what a shared-cache
/// segment carries. `tests[..].run` is ignored; the run is `run`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSlice {
    pub run: RunMeta,
    pub tests: HashMap<String, Entry>,
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

/// The [`Entry::outcome`] byte for an outcome name.
pub fn outcome_code(name: &str) -> Option<u8> {
    OUTCOMES.iter().position(|o| *o == name).map(|i| i as u8)
}

impl Entry {
    pub fn outcome_name(&self) -> &'static str {
        OUTCOMES
            .get(usize::from(self.outcome))
            .copied()
            .unwrap_or("unknown")
    }
}

impl History {
    /// `nodeid`'s entries paired with their run, oldest first.
    pub fn series(&self, nodeid: &str) -> Vec<(&RunMeta, &Entry)> {
        let runs: HashMap<u64, &RunMeta> = self.runs.iter().map(|r| (r.seq, r)).collect();
        self.tests
            .get(nodeid)
            .into_iter()
            .flatten()
            .filter_map(|e| runs.get(&e.run).map(|r| (*r, e)))
            .collect()
    }

    /// Tests whose recorded series flipped at least `min` times, sorted. A
    /// flip is a run rescued by `--reruns` (`flaky`) or a failure right after
    /// a pass on the same clean commit (non-pass/non-fail outcomes like
    /// `skipped` are stepped over). A test that broke and stays broken flips
    /// once, so a real regression is never auto-quarantined at the default
    /// `min`; one broken and fixed across edits doesn't flip at all. A test
    /// [`failing_now`] is left out whatever its past flips: a flapper that
    /// then broke for real must fail the run again.
    pub fn flippers(&self, min: u32) -> Vec<String> {
        let commits: HashMap<u64, Option<&str>> = self
            .runs
            .iter()
            .map(|r| (r.seq, r.commit.as_deref()))
            .collect();
        let mut out: Vec<String> = self
            .tests
            .iter()
            .filter(|(_, s)| {
                !failing_now(s.iter().map(Entry::outcome_name))
                    && flips(
                        s.iter()
                            .filter_map(|e| commits.get(&e.run).map(|c| (*c, e))),
                    ) >= min
            })
            .map(|(id, _)| id.clone())
            .collect();
        out.sort();
        out
    }

    /// Every run with its entries: the inverse of `append`.
    pub fn into_slices(self) -> Vec<RunSlice> {
        let mut by_seq: HashMap<u64, RunSlice> = self
            .runs
            .into_iter()
            .map(|r| {
                (
                    r.seq,
                    RunSlice {
                        run: r,
                        tests: HashMap::new(),
                    },
                )
            })
            .collect();
        for (id, series) in self.tests {
            for e in series {
                if let Some(s) = by_seq.get_mut(&e.run) {
                    s.tests.insert(id.clone(), e);
                }
            }
        }
        by_seq.into_values().collect()
    }

    /// Build a history from runs gathered from several places (local file,
    /// shared-cache base, segments): the first copy of each run id wins, runs
    /// are replayed oldest first (`epoch`, then id, so every reader merges the
    /// same way), and each test keeps its newest `cap` entries.
    pub fn merged(slices: impl IntoIterator<Item = RunSlice>, cap: usize) -> History {
        let mut seen = HashSet::new();
        let mut runs: Vec<RunSlice> = slices
            .into_iter()
            .filter(|s| seen.insert(s.run.id.clone()))
            .collect();
        runs.sort_by(|a, b| {
            a.run
                .epoch
                .cmp(&b.run.epoch)
                .then_with(|| a.run.id.cmp(&b.run.id))
        });
        let mut h = History::default();
        for s in runs {
            h.append(s.run, s.tests.into_iter().collect(), cap);
        }
        h
    }

    /// Add one run. Each test's series is trimmed to the newest `cap` entries
    /// and runs no entry points at any more are dropped.
    fn append(&mut self, mut meta: RunMeta, entries: Vec<(String, Entry)>, cap: usize) {
        meta.seq = self.runs.last().map_or(0, |r| r.seq + 1);
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
    pub fn age(&mut self, now: u64, max_age: u64) {
        if max_age == 0 {
            return;
        }
        let expired: HashSet<u64> = self
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
        let live: HashSet<u64> = self.tests.values().flatten().map(|e| e.run).collect();
        self.runs.retain(|r| live.contains(&r.seq));
    }
}

/// A failing outcome, as flips and [`failing_now`] read it.
pub fn is_fail(outcome: &str) -> bool {
    matches!(outcome, "failed" | "errors" | "crashed" | "quarantined")
}

/// The latest 3 pass/fail results (oldest-first `outcomes`) all failed: it
/// broke and isn't flapping now.
pub fn failing_now<'a>(outcomes: impl DoubleEndedIterator<Item = &'a str>) -> bool {
    let tail: Vec<&str> = outcomes
        .rev()
        .filter(|o| is_fail(o) || matches!(*o, "passed" | "flaky"))
        .take(3)
        .collect();
    tail.len() == 3 && tail.iter().all(|o| is_fail(o))
}

fn flips<'a>(series: impl IntoIterator<Item = (Option<&'a str>, &'a Entry)>) -> u32 {
    flip_positions(series).len() as u32
}

/// Indices (oldest first) of the flips in one test's `(commit, entry)`
/// series: a `flaky` pass, or a fail right after a pass. The pass-then-fail
/// counts only when both runs share a commit with no uncommitted edits:
/// across a code change, the fail is the change, not flakiness. Skips,
/// xfails etc. are stepped over.
pub fn flip_positions<'a>(
    series: impl IntoIterator<Item = (Option<&'a str>, &'a Entry)>,
) -> Vec<usize> {
    let mut out = Vec::new();
    // The commit of the latest run, when that run passed.
    let mut last_pass: Option<Option<&str>> = None;
    for (i, (commit, e)) in series.into_iter().enumerate() {
        match e.outcome_name() {
            "flaky" => {
                out.push(i);
                last_pass = Some(commit);
            }
            "passed" => last_pass = Some(commit),
            "failed" | "errors" | "crashed" | "quarantined" => {
                let clean = commit.is_some_and(|c| !c.ends_with("-dirty"));
                if clean && last_pass == Some(commit) {
                    out.push(i);
                }
                last_pass = None;
            }
            _ => {}
        }
    }
    out
}

/// Entries kept per test. `RSTEST_HISTORY_RUNS`, default 50, minimum 1.
pub fn cap() -> usize {
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
        outcome: outcome_code(outcome)?,
        // A run --durations-regress flagged keeps its time out of the
        // history, as out of durations.json: the gate's baseline must not
        // drift toward the regression and silence it.
        call: e
            .duration
            .filter(|_| !e.duration_regressed)
            .map(|d| d as f32),
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

/// This run's slice, or `None` when no test produced a real result. Its id is
/// fresh per process, never `RSTEST_RUN_UID`: CI shards, monorepo projects and
/// an exported uid all share that one, and [`History::merged`] would keep only
/// the first of them.
pub fn slice(run: &Run, epoch: u64, commit: Option<&str>, env: &str) -> Option<RunSlice> {
    let tests: HashMap<String, Entry> = run
        .tests()
        .iter()
        .filter_map(|(id, e)| entry_for(e).map(|en| (id.clone(), en)))
        .collect();
    if tests.is_empty() {
        return None;
    }
    Some(RunSlice {
        run: RunMeta {
            seq: 0,
            id: crate::run::new_run_uid(),
            epoch,
            commit: commit.map(str::to_string),
            env: env.to_string(),
        },
        tests,
    })
}

/// Append this run to the stored history. Best-effort: IO errors are ignored.
pub fn record(slice: RunSlice) {
    record_in(&cache::dir(), slice, cap());
}

/// `record` against an explicit cache dir and per-test cap.
fn record_in(dir: &std::path::Path, slice: RunSlice, cap: usize) {
    // Locked load→append→write: two concurrent sessions in one cwd would
    // otherwise drop one run's entries.
    let path = dir.join(FILE);
    cache::with_lock_in(dir, || {
        let mut h = load_from(&path);
        h.append(slice.run, slice.tests.into_iter().collect(), cap);
        save_to(&path, &h);
    });
}

/// Fold runs pulled from the shared cache into the history file at `path`
/// (the caller holds the cache lock). Runs the file already has are kept
/// as they are.
pub fn overlay_in(path: &std::path::Path, pulled: History) {
    if pulled.runs.is_empty() {
        return;
    }
    let local = load_from(path).into_slices();
    save_to(
        path,
        &History::merged(local.into_iter().chain(pulled.into_slices()), cap()),
    );
}

fn save_to(path: &std::path::Path, h: &History) {
    if let Ok(bytes) = rmp_serde::to_vec(h) {
        let _ = cache::write_atomic(path, &bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(epoch: u64) -> RunMeta {
        RunMeta {
            seq: 0,
            id: format!("r{epoch}"),
            epoch,
            commit: Some(format!("c{epoch}")),
            env: "fp".into(),
        }
    }

    fn entry(outcome: &str) -> Entry {
        Entry {
            run: 0,
            outcome: outcome_code(outcome).unwrap(),
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
        assert_eq!(h.series("b")[0].1.outcome_name(), "failed");
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
    fn seq_past_u32_does_not_collide_with_live_run() {
        let mut h = History::default();
        h.append(meta(1), vec![("b".into(), entry("passed"))], 50);
        h.runs.push(RunMeta {
            seq: u64::from(u32::MAX),
            ..meta(2)
        });
        h.tests.entry("a".into()).or_default().push_back(Entry {
            run: u64::from(u32::MAX),
            ..entry("passed")
        });
        h.append(meta(3), vec![("a".into(), entry("failed"))], 50);
        // A wrapped seq would be 0, already owned by `b`'s run (epoch 1).
        assert_eq!(h.series("a").last().unwrap().0.epoch, 3);
        assert_eq!(h.series("b")[0].0.epoch, 1);
    }

    /// The file holds tests x runs entries and is rewritten every run, so an
    /// entry must stay small (a 10k-test suite at 50 runs is 500k entries).
    #[test]
    fn entries_encode_compactly() {
        let mut h = History::default();
        let tests: Vec<String> = (0..100).map(|i| format!("t{i}")).collect();
        for r in 0..50u64 {
            let es = tests.iter().map(|t| (t.clone(), entry("passed"))).collect();
            h.append(meta(r), es, 50);
        }
        let bytes = rmp_serde::to_vec(&h).unwrap().len();
        let per_entry = bytes / (100 * 50);
        assert!(per_entry <= 14, "{per_entry} bytes/entry");
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
        // Exactly max_age old is still in the window.
        h.append(meta(500), vec![("edge".into(), entry("passed"))], 50);
        h.age(1000, 500);
        assert_eq!(h.series("old").len(), 1);
        assert_eq!(h.series("new").len(), 1);
        assert_eq!(h.series("edge").len(), 1);
        assert!(!h.tests.contains_key("gone"));
        assert_eq!(h.runs.len(), 2);
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
        assert_eq!(e.outcome_name(), "passed");
        assert_eq!(e.call, Some(0.25));
        assert_eq!(e.setup, 0.5);
        let f = entry_for(&TestEntry {
            flaky: true,
            ..base()
        })
        .unwrap();
        assert_eq!(f.outcome_name(), "flaky");
        // A --durations-regress flagged run keeps its time out.
        let r = entry_for(&TestEntry {
            duration_regressed: true,
            ..base()
        })
        .unwrap();
        assert_eq!((r.outcome_name(), r.call), ("passed", None));
        let c = entry_for(&TestEntry {
            crashed: true,
            call: Some("failed".into()),
            ..base()
        })
        .unwrap();
        assert_eq!(c.outcome_name(), "crashed");
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
    fn flippers_catch_intermittent_not_steadily_broken() {
        let mut h = History::default();
        let series = |s: &str| {
            s.chars()
                .map(|c| match c {
                    'p' => "passed",
                    'f' => "failed",
                    '~' => "flaky",
                    'q' => "quarantined",
                    _ => "skipped",
                })
                .collect::<Vec<_>>()
        };
        let tests = [
            ("broke", "ppppffff"),    // 1 flip: a real regression
            ("flap", "pfpfpf"),       // 3 flips
            ("rescued", "p~p~p~"),    // 3 reruns rescued it
            ("skips", "pfsfpsf"),     // skip between fails is stepped over: 2
            ("quar", "pqpqpq"),       // quarantined failures still count
            ("born_red", "ffffffff"), // never passed: 0
        ];
        for i in 0..8 {
            let es = tests
                .iter()
                .filter_map(|(id, s)| series(s).get(i).map(|o| (id.to_string(), entry(o))))
                .collect();
            // One commit throughout: every run tests the same code.
            let m = RunMeta {
                commit: Some("c".into()),
                ..meta(i as u64)
            };
            h.append(m, es, 50);
        }
        assert_eq!(h.flippers(3), ["flap", "quar", "rescued"]);
        assert_eq!(h.flippers(2), ["flap", "quar", "rescued", "skips"]);
        // `broke` flipped once but is failing now: released even at 1.
        assert_eq!(h.flippers(1), ["flap", "quar", "rescued", "skips"]);
    }

    #[test]
    fn flipper_that_broke_for_real_is_released() {
        let mut h = History::default();
        // Flapped 3 times, then 3 straight fails (skips stepped over).
        for (i, o) in "pfpfpffsff".chars().enumerate() {
            let o = if o == 'p' {
                "passed"
            } else if o == 's' {
                "skipped"
            } else {
                "failed"
            };
            let m = RunMeta {
                commit: Some("c".into()),
                ..meta(i as u64)
            };
            h.append(m, vec![("t".into(), entry(o))], 50);
        }
        assert!(h.flippers(3).is_empty());
        // Passing again: flapping, caught again.
        let m = RunMeta {
            commit: Some("c".into()),
            ..meta(10)
        };
        h.append(m, vec![("t".into(), entry("passed"))], 50);
        assert_eq!(h.flippers(3), ["t"]);
    }

    #[test]
    fn flips_need_the_same_clean_commit() {
        let mut h = History::default();
        // (commit, flap outcome, rescued outcome) per run.
        let runs = [
            (Some("a"), "passed", "flaky"),
            (Some("b"), "failed", "flaky"), // fail on a new commit: an edit
            (Some("b"), "passed", "flaky"),
            (Some("b-dirty"), "failed", "passed"), // uncommitted edits
            (Some("b-dirty"), "passed", "passed"),
            (Some("b-dirty"), "failed", "passed"), // dirty on both sides
            (None, "passed", "passed"),
            (None, "failed", "passed"), // no git: no commit to compare
        ];
        for (i, (commit, flap, rescued)) in runs.into_iter().enumerate() {
            let m = RunMeta {
                commit: commit.map(str::to_string),
                ..meta(i as u64)
            };
            let es = vec![
                ("flap".to_string(), entry(flap)),
                ("rescued".to_string(), entry(rescued)),
            ];
            h.append(m, es, 50);
        }
        // Reruns rescuing a run is flakiness on any commit; pass->fail isn't.
        assert_eq!(h.flippers(1), ["rescued"]);
        assert_eq!(h.flippers(3), ["rescued"]);
    }

    fn run_of(tests: &[(&str, &str)]) -> Run {
        let mut run = Run::default();
        for (nodeid, outcome) in tests {
            run.record(
                None,
                crate::scheduling::proto::Report {
                    nodeid: (*nodeid).into(),
                    when: "call".into(),
                    outcome: (*outcome).into(),
                    duration: 0.5,
                    longrepr: None,
                    wasxfail: false,
                    skip_reason: None,
                    cpu: None,
                    thread_delta: None,
                    fd_delta: None,
                    sections: Vec::new(),
                    lineno: None,
                    subtest: false,
                    crash: None,
                    xfail_reason: None,
                    skip_location: None,
                },
            );
        }
        run
    }

    #[test]
    fn record_appends_each_run_under_the_cap() {
        let dir = std::env::temp_dir().join(format!("rstest-history-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = crate::time::now_epoch_secs();
        // A run with no real results writes nothing.
        assert!(super::slice(&Run::default(), now, Some("c1"), "fp").is_none());

        let rec = |run: &Run, at: u64, commit: Option<&str>| {
            record_in(&dir, super::slice(run, at, commit, "fp").unwrap(), 2);
        };
        rec(&run_of(&[("a", "passed")]), now, Some("c1"));
        rec(&run_of(&[("a", "failed"), ("b", "passed")]), now + 1, None);
        rec(&run_of(&[("a", "passed")]), now + 2, Some("c3"));
        let h = load_from(&dir.join(FILE));
        let a = h.series("a");
        let got: Vec<_> = a
            .iter()
            .map(|(r, e)| (r.epoch, r.commit.as_deref(), e.outcome_name()))
            .collect();
        assert_eq!(
            got,
            [(now + 1, None, "failed"), (now + 2, Some("c3"), "passed")]
        );
        assert_eq!(a[1].1.call, Some(0.5));
        assert_eq!(h.series("b").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CI shards (and monorepo projects, and runs under an exported
    /// `RSTEST_RUN_UID`) share one run uid; each one's slice must still
    /// survive the dedupe in `merged`, or all but one lose their history.
    #[test]
    fn slices_of_runs_sharing_a_run_uid_all_survive_merge() {
        let now = crate::time::now_epoch_secs();
        let shard1 = super::slice(&run_of(&[("a", "passed")]), now, Some("c1"), "fp").unwrap();
        let shard2 = super::slice(&run_of(&[("b", "failed")]), now, Some("c1"), "fp").unwrap();
        assert_ne!(shard1.run.id, shard2.run.id);
        let h = History::merged([shard1, shard2], 50);
        assert_eq!(h.runs.len(), 2);
        assert_eq!(outcomes(&h, "a"), ["passed"]);
        assert_eq!(outcomes(&h, "b"), ["failed"]);
    }

    fn slice(id: &str, epoch: u64, tests: &[(&str, &str)]) -> RunSlice {
        RunSlice {
            run: RunMeta {
                id: id.into(),
                ..meta(epoch)
            },
            tests: tests
                .iter()
                .map(|(t, o)| (t.to_string(), entry(o)))
                .collect(),
        }
    }

    fn outcomes(h: &History, id: &str) -> Vec<String> {
        h.series(id)
            .iter()
            .map(|(_, e)| e.outcome_name().to_string())
            .collect()
    }

    #[test]
    fn merged_dedupes_runs_by_id_and_replays_oldest_first() {
        // Two machines' views: `b` is in both; `c` arrives out of epoch order.
        let mine = [
            slice("a", 1, &[("t", "passed")]),
            slice("b", 2, &[("t", "failed")]),
        ];
        let theirs = [
            slice("c", 3, &[("t", "passed"), ("u", "passed")]),
            // Same run id: the first copy (mine) wins, never counted twice.
            slice("b", 2, &[("t", "passed")]),
        ];
        let h = History::merged(mine.into_iter().chain(theirs), 50);
        assert_eq!(outcomes(&h, "t"), ["passed", "failed", "passed"]);
        assert_eq!(outcomes(&h, "u"), ["passed"]);
        let ids: Vec<&str> = h.runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);

        // Same-epoch runs order by id, whatever order they arrive in.
        let x = History::merged(
            [
                slice("z", 5, &[("t", "failed")]),
                slice("y", 5, &[("t", "passed")]),
            ],
            50,
        );
        assert_eq!(outcomes(&x, "t"), ["passed", "failed"]);

        // The cap keeps each test's newest runs.
        let capped = History::merged(
            (0..5).map(|i| slice(&format!("r{i}"), i, &[("t", "passed")])),
            2,
        );
        let kept: Vec<u64> = capped.series("t").iter().map(|(r, _)| r.epoch).collect();
        assert_eq!(kept, [3, 4]);
    }

    #[test]
    fn into_slices_round_trips_through_merged() {
        let mut h = History::default();
        h.append(
            meta(1),
            vec![("a".into(), entry("passed")), ("b".into(), entry("failed"))],
            50,
        );
        h.append(meta(2), vec![("a".into(), entry("flaky"))], 50);
        let back = History::merged(h.clone().into_slices(), 50);
        assert_eq!(back, h);
    }

    #[test]
    fn overlay_in_keeps_local_runs_and_adds_pulled_ones() {
        let dir =
            std::env::temp_dir().join(format!("rstest-history-overlay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        let now = crate::time::now_epoch_secs();
        let local = History::merged([slice("mine", now - 10, &[("t", "failed")])], 50);
        save_to(&path, &local);
        // The pull carries this machine's own pushed run plus a CI run.
        let pulled = History::merged(
            [
                slice("mine", now - 10, &[("t", "failed")]),
                slice("ci", now - 5, &[("t", "passed")]),
            ],
            50,
        );
        overlay_in(&path, pulled);
        let h = load_from(&path);
        assert_eq!(outcomes(&h, "t"), ["failed", "passed"]);
        // Empty pull: file untouched.
        overlay_in(&path, History::default());
        assert_eq!(load_from(&path), h);
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
