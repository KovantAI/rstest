//! `rstest --doctor`: why is this suite slow? Surfaces suite-content costs
//! from runner timing data (sleep/wait-bound tests, repeated fixtures). One
//! pass feeds the terminal report and a versioned JSON doc (`--doctor-json`).
//!
//! Split across the module: this file owns the report types and the [`analyze`]
//! pass; [`gate`] owns the `--doctor-fail-on` threshold gate; [`render`] owns
//! the terminal / markdown / CI-summary output. Sub-report structs stay private
//! here and are read by the child modules via descendant visibility.

mod gate;
mod render;

pub use gate::{evaluate, parse_conditions, GateCondition};
pub(crate) use render::leak_delta;
pub use render::{append_ci_summary, render, write_markdown};

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use crate::reporting::report::Run;
use crate::scheduling::proto::FixtureStat;
use crate::select::CoverageIndex;

/// Bump when the JSON shape changes incompatibly.
const SCHEMA_VERSION: u32 = 3;

/// Only tests at least this slow are worth flagging as coverage waste: deleting
/// a fast redundant test frees no meaningful time.
const WASTE_MIN_SECONDS: f64 = 0.5;

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DoctorReport {
    schema: u32,
    rstest_version: &'static str,
    workers: usize,
    wall_seconds: f64,
    tests: usize,
    test_time_seconds: f64,
    /// Sum of call-phase CPU time, over tests where it was measured.
    cpu_time_seconds: f64,
    wait_bound: Option<WaitBound>,
    parallel_floor: Option<ParallelFloor>,
    parallel_efficiency: Option<ParallelEfficiency>,
    fixtures: Vec<FixtureEntry>,
    slowest_files: Vec<FileEntry>,
    /// Slow tests that can be deleted together without dropping any covered
    /// line - delete/merge candidates. `None` unless THIS run wrote a per-test
    /// coverage index (`--cov --cov-context=test` alongside `--doctor`; an index
    /// left by an earlier run is ignored as stale) and at least one test qualified.
    coverage_waste: Option<CoverageWaste>,
    /// Tests that leaked threads / fds (net positive after teardown). Empty
    /// unless leak-check instrumentation ran (`--doctor` / `--fail-on-leak`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leaks: Vec<Leak>,
}

/// Slow tests that add no unique coverage: chosen greedily (slowest first) so
/// that deleting ALL of them together still leaves every covered line covered by
/// some kept test. Of two tests covering identical lines, only one is listed.
/// Pure suite bloat on the time axis.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct CoverageWaste {
    /// Sum of the durations of the whole deletable set (not just the shown
    /// ones) - the time reclaimable by pruning them together.
    wasted_seconds: f64,
    /// Size of the deletable set (`tests` shows the slowest of them).
    redundant_tests: usize,
    /// The slowest redundant tests, worst first (capped).
    tests: Vec<WasteTest>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct WasteTest {
    nodeid: String,
    duration: f64,
    /// Product lines this test covered, all also covered by a kept test.
    covered_lines: u64,
    /// Distinct KEPT tests (not themselves in the deletable set) that between
    /// them also cover those lines.
    also_covered_by: u64,
}

/// A test that ended with more threads / open fds than it started — a resource
/// it opened and never released (its own teardown included).
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Leak {
    pub nodeid: String,
    /// Net threads leaked (0 if only fds leaked).
    pub threads: i64,
    /// Net open fds leaked (0 if only threads leaked).
    pub fds: i64,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct WaitBound {
    wait_seconds: f64,
    wait_pct: f64,
    tests: Vec<WaitTest>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct WaitTest {
    nodeid: String,
    duration: f64,
    wait: f64,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct ParallelFloor {
    longest_seconds: f64,
    ideal_share_seconds: f64,
    gate_tests: Vec<GateTest>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct GateTest {
    nodeid: String,
    duration: f64,
}

/// Realized parallel speedup measured from an actual run. Unlike
/// `ParallelFloor` (a static pre-run estimate), this is the after-the-fact
/// "why isn't `-n auto` faster?". Only for multi-worker pool runs.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct ParallelEfficiency {
    /// test_time / wall. May exceed `ideal_speedup` for wait-bound suites,
    /// where overlapping sleeps/IO run more tests at once than there are
    /// cores.
    realized_speedup: f64,
    /// Worker count (`-n`) - the ceiling for a purely CPU-bound suite.
    ideal_speedup: usize,
    /// 100 * realized / ideal. >100% signals wait-bound overlap.
    efficiency_pct: f64,
    /// Busy time summed per worker, descending - the load-balance picture.
    workers_busy: Vec<WorkerLoad>,
    /// 100 * (busiest - idlest) / busiest. High = uneven distribution.
    imbalance_pct: f64,
    /// Slowest single test: the hard floor no worker count beats.
    long_pole_seconds: f64,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct WorkerLoad {
    worker: String,
    busy_seconds: f64,
    tests: usize,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct FixtureEntry {
    name: String,
    scope: String,
    count: u64,
    total_seconds: f64,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct FileEntry {
    file: String,
    total_seconds: f64,
    pct: f64,
}

pub fn analyze(
    run: &Run,
    fixtures: &[FixtureStat],
    wall: f64,
    workers: usize,
    coverage: Option<&CoverageIndex>,
) -> DoctorReport {
    let tests = run.tests();
    let mut durations: Vec<(&String, f64, Option<f64>)> = tests
        .iter()
        .filter_map(|(id, e)| e.duration.map(|d| (id, d, e.cpu)))
        .collect();
    let test_time: f64 = durations.iter().map(|(_, d, _)| d).sum();
    let cpu_time: f64 = durations.iter().filter_map(|(_, _, c)| *c).sum();
    let n_cpu = durations.iter().filter(|(_, _, c)| c.is_some()).count();

    // -- Wait-bound: wall vs cpu ---------------------------------------
    let wait_bound = if n_cpu > 0 {
        let wait = (test_time - cpu_time).max(0.0);
        let pct = 100.0 * wait / test_time.max(f64::EPSILON);
        if pct >= 20.0 && wait >= 1.0 {
            let mut waiters: Vec<WaitTest> = durations
                .iter()
                .filter_map(|(id, d, c)| {
                    c.map(|c| WaitTest {
                        nodeid: (*id).clone(),
                        duration: *d,
                        wait: d - c,
                    })
                })
                .filter(|t| t.duration >= 0.2 && t.wait / t.duration >= 0.6)
                .collect();
            waiters.sort_by(|a, b| b.wait.total_cmp(&a.wait));
            waiters.truncate(50);
            Some(WaitBound {
                wait_seconds: wait,
                wait_pct: pct,
                tests: waiters,
            })
        } else {
            None
        }
    } else {
        None
    };

    // -- Parallel floor --------------------------------------------------
    durations.sort_by(|a, b| b.1.total_cmp(&a.1));
    let parallel_floor = durations.first().and_then(|&(_, longest, _)| {
        let ideal = test_time / workers.max(1) as f64;
        (longest > ideal.max(1.0)).then(|| ParallelFloor {
            longest_seconds: longest,
            ideal_share_seconds: ideal,
            gate_tests: durations
                .iter()
                .take(10)
                .filter(|(_, d, _)| *d > ideal.max(1.0))
                .map(|(id, d, _)| GateTest {
                    nodeid: (*id).clone(),
                    duration: *d,
                })
                .collect(),
        })
    });

    // -- Parallel efficiency (realized speedup + worker load balance) ------
    // Multi-worker only. Groups already-collected per-test durations by
    // recorded worker to expose load imbalance without new timeline data.
    let parallel_efficiency = (workers > 1 && test_time > 0.0).then(|| {
        let mut by_worker: BTreeMap<&str, (f64, usize)> = BTreeMap::new();
        for e in tests.values() {
            if let Some(d) = e.duration {
                let w = e.worker.as_deref().unwrap_or("serial");
                let slot = by_worker.entry(w).or_default();
                slot.0 += d;
                slot.1 += 1;
            }
        }
        let mut workers_busy: Vec<WorkerLoad> = by_worker
            .into_iter()
            .map(|(worker, (busy, n))| WorkerLoad {
                worker: worker.to_string(),
                busy_seconds: busy,
                tests: n,
            })
            .collect();
        workers_busy.sort_by(|a, b| b.busy_seconds.total_cmp(&a.busy_seconds));
        let max_busy = workers_busy.first().map_or(0.0, |w| w.busy_seconds);
        // Idle workers are absent from `by_worker` but still in the pool
        // (busy 0). Using the smallest *observed* load instead hides the
        // worst case (all work on one worker would read 0%, not ~100%).
        let min_busy = if workers_busy.len() < workers {
            0.0
        } else {
            workers_busy.last().map_or(0.0, |w| w.busy_seconds)
        };
        let imbalance_pct = if max_busy > 0.0 {
            100.0 * (max_busy - min_busy) / max_busy
        } else {
            0.0
        };
        let realized = test_time / wall.max(f64::EPSILON);
        ParallelEfficiency {
            realized_speedup: realized,
            ideal_speedup: workers,
            efficiency_pct: 100.0 * realized / workers as f64,
            workers_busy,
            imbalance_pct,
            // durations was sorted descending by the parallel-floor block.
            long_pole_seconds: durations.first().map_or(0.0, |(_, d, _)| *d),
        }
    });

    // -- Fixtures ----------------------------------------------------------
    let mut fx: Vec<FixtureEntry> = fixtures
        .iter()
        .map(|f| FixtureEntry {
            name: f.name.clone(),
            scope: f.scope.clone(),
            count: f.count,
            total_seconds: f.total,
        })
        .collect();
    fx.sort_by(|a, b| b.total_seconds.total_cmp(&a.total_seconds));
    fx.truncate(50);

    // -- Slowest files ------------------------------------------------------
    let mut by_file: BTreeMap<&str, f64> = BTreeMap::new();
    for (id, d, _) in &durations {
        let file = crate::text::nodeid_file(id);
        *by_file.entry(file).or_default() += d;
    }
    let mut files: Vec<FileEntry> = by_file
        .into_iter()
        .map(|(file, total)| FileEntry {
            file: file.to_string(),
            total_seconds: total,
            pct: 100.0 * total / test_time.max(f64::EPSILON),
        })
        .collect();
    files.sort_by(|a, b| b.total_seconds.total_cmp(&a.total_seconds));
    files.truncate(20);

    // -- Coverage waste (slow tests adding no unique coverage) --------------
    let coverage_waste = coverage.and_then(|index| {
        let duration_of: HashMap<&str, f64> = durations
            .iter()
            .map(|(id, d, _)| (id.as_str(), *d))
            .collect();
        coverage_waste(&duration_of, index, WASTE_MIN_SECONDS)
    });

    let leaks = detect_leaks(run);

    DoctorReport {
        schema: SCHEMA_VERSION,
        rstest_version: env!("CARGO_PKG_VERSION"),
        workers,
        wall_seconds: wall,
        tests: durations.len(),
        test_time_seconds: test_time,
        cpu_time_seconds: cpu_time,
        wait_bound,
        parallel_floor,
        parallel_efficiency,
        fixtures: fx,
        slowest_files: files,
        coverage_waste,
        leaks,
    }
}

/// Slow tests whose every covered line is ALSO covered by another test - pure
/// redundant suite cost, deletable/mergeable without losing any covered line.
/// `duration_of` maps nodeid -> call duration; only tests at least `min_seconds`
/// slow qualify. `None` when the index is empty or nothing qualifies.
fn coverage_waste(
    duration_of: &HashMap<&str, f64>,
    index: &CoverageIndex,
    min_seconds: f64,
) -> Option<CoverageWaste> {
    // Measure redundancy over PRODUCT code only. Under `--cov=.` the index also
    // records each test's OWN file, whose body lines only that test executes -
    // counting them would make every test trivially "unique" and hide real
    // waste. A test file is exactly a file that some nodeid lives in, so the set
    // of covered test files is derivable from the nodeids with no project config.
    let mut test_files: HashSet<&str> = HashSet::new();
    for cov in index.files.values() {
        for ids in cov.lines.values() {
            for id in ids {
                test_files.insert(crate::text::nodeid_file(id));
            }
        }
    }
    let is_source = |file: &str| !test_files.contains(file);

    // Pass 1, O(total coverage entries): per test, total covered PRODUCT lines
    // and lines it ALONE covers (a line's coverer list is exactly the tests that
    // hit it). Lines in test files are skipped (see above).
    let mut total: HashMap<&str, u64> = HashMap::new();
    let mut unique: HashMap<&str, u64> = HashMap::new();
    for (file, cov) in &index.files {
        if !is_source(file) {
            continue;
        }
        for ids in cov.lines.values() {
            let solo = ids.len() == 1;
            for id in ids {
                *total.entry(id.as_str()).or_default() += 1;
                if solo {
                    *unique.entry(id.as_str()).or_default() += 1;
                }
            }
        }
    }
    let dur = |id: &str| duration_of.get(id).copied();
    // Zero unique lines is necessary but not sufficient: two tests covering
    // exactly the same lines both have zero unique lines, yet deleting BOTH
    // loses that coverage. Candidates: covered >= 1 line, no solo line, slow
    // enough to matter - worst first (duration, then nodeid for stability).
    let mut candidates: Vec<&str> = total
        .iter()
        .filter(|(id, tot)| **tot > 0 && unique.get(**id).copied().unwrap_or(0) == 0)
        .filter(|(id, _)| dur(id).is_some_and(|d| d >= min_seconds))
        .map(|(id, _)| *id)
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| {
        dur(b)
            .unwrap_or(0.0)
            .total_cmp(&dur(a).unwrap_or(0.0))
            .then(a.cmp(b))
    });
    // Pass 2, bounded to the candidates: each one's product lines, plus a live
    // coverer count per line (starts at the full coverer list length).
    let cand_set: HashSet<&str> = candidates.iter().copied().collect();
    let mut live: HashMap<(&str, u32), usize> = HashMap::new();
    let mut lines_of: HashMap<&str, Vec<(&str, u32)>> = HashMap::new();
    for (file, cov) in &index.files {
        if !is_source(file) {
            continue;
        }
        for (ln, ids) in &cov.lines {
            let key = (file.as_str(), *ln);
            for id in ids {
                if cand_set.contains(id.as_str()) {
                    live.insert(key, ids.len());
                    lines_of.entry(id.as_str()).or_default().push(key);
                }
            }
        }
    }
    // Greedy, slowest first: a candidate is removable only if every line it
    // covers still has another live coverer after the earlier removals. Removing
    // it drops each of its lines' live count, so a duplicate pair yields ONE
    // redundant test, never both.
    let mut removed: Vec<&str> = Vec::new();
    for id in candidates {
        let lines = lines_of.get(id).map_or(&[][..], Vec::as_slice);
        if lines.iter().all(|k| live.get(k).copied().unwrap_or(0) >= 2) {
            for k in lines {
                if let Some(n) = live.get_mut(k) {
                    *n -= 1;
                }
            }
            removed.push(id);
        }
    }
    if removed.is_empty() {
        return None;
    }
    let redundant_tests = removed.len();
    // Headline reclaimable time spans ALL removable slow tests, not just shown.
    let wasted_seconds: f64 = removed.iter().filter_map(|id| dur(id)).sum();
    let removed_set: HashSet<&str> = removed.iter().copied().collect();
    removed.truncate(20);
    let candidates = removed;
    let shown: HashSet<&str> = candidates.iter().copied().collect();
    // Pass 3, bounded to the shown tests: distinct KEPT tests sharing each one's
    // lines - "covers only lines N other tests already hit". Other removable
    // tests are excluded, since they are proposed for deletion too.
    let mut co: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (file, cov) in &index.files {
        if !is_source(file) {
            continue;
        }
        for ids in cov.lines.values() {
            if ids.len() < 2 {
                continue; // a solo line has no co-coverer (and no candidate is solo)
            }
            for id in ids {
                if shown.contains(id.as_str()) {
                    let set = co.entry(id.as_str()).or_default();
                    set.extend(
                        ids.iter()
                            .map(String::as_str)
                            .filter(|o| !removed_set.contains(o)),
                    );
                }
            }
        }
    }
    let tests = candidates
        .iter()
        .map(|id| WasteTest {
            nodeid: (*id).to_string(),
            duration: dur(id).unwrap_or(0.0),
            covered_lines: total.get(id).copied().unwrap_or(0),
            also_covered_by: co.get(id).map_or(0, |s| s.len() as u64),
        })
        .collect();
    Some(CoverageWaste {
        wasted_seconds,
        redundant_tests,
        tests,
    })
}

/// Tests that leaked threads/fds (net positive after teardown), worst first.
/// A resource the test opened and never released — its own teardown included.
/// Empty unless leak-check instrumentation ran. Shared by the doctor report and
/// the `--fail-on-leak` gate.
pub fn detect_leaks(run: &Run) -> Vec<Leak> {
    let mut leaks: Vec<Leak> = run
        .tests()
        .iter()
        .filter_map(|(id, e)| {
            let threads = e.thread_delta.unwrap_or(0).max(0);
            let fds = e.fd_delta.unwrap_or(0).max(0);
            (threads > 0 || fds > 0).then(|| Leak {
                nodeid: id.clone(),
                threads,
                fds,
            })
        })
        .collect();
    // Worst first: total leaked resources, then threads, then name for stability.
    leaks.sort_by(|a, b| {
        (b.threads + b.fds)
            .cmp(&(a.threads + a.fds))
            .then(b.threads.cmp(&a.threads))
            .then(a.nodeid.cmp(&b.nodeid))
    });
    leaks
}

pub fn write_json(path: &std::path::Path, report: &DoctorReport) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
}

/// Report builders shared by the `analyze`/`gate`/`render` test modules. A
/// descendant of the types' module, so it can populate their private fields.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use crate::reporting::report::Run;

    pub fn report(tests: usize) -> DoctorReport {
        DoctorReport {
            schema: SCHEMA_VERSION,
            rstest_version: "test",
            workers: 4,
            wall_seconds: 9.0,
            tests,
            test_time_seconds: 30.0,
            cpu_time_seconds: 6.0,
            wait_bound: Some(WaitBound {
                wait_seconds: 24.0,
                wait_pct: 80.0,
                tests: vec![WaitTest {
                    nodeid: "tests/test_a.py::test_sleepy".into(),
                    duration: 5.1,
                    wait: 5.0,
                }],
            }),
            parallel_floor: Some(ParallelFloor {
                longest_seconds: 8.4,
                ideal_share_seconds: 7.5,
                gate_tests: vec![GateTest {
                    nodeid: "tests/test_a.py::test_long".into(),
                    duration: 8.4,
                }],
            }),
            parallel_efficiency: Some(ParallelEfficiency {
                realized_speedup: 3.3,
                ideal_speedup: 4,
                efficiency_pct: 82.5,
                workers_busy: vec![
                    WorkerLoad {
                        worker: "gw0".into(),
                        busy_seconds: 16.0,
                        tests: 6,
                    },
                    WorkerLoad {
                        worker: "gw1".into(),
                        busy_seconds: 14.0,
                        tests: 6,
                    },
                ],
                imbalance_pct: 12.5,
                long_pole_seconds: 8.4,
            }),
            fixtures: vec![FixtureEntry {
                name: "db".into(),
                scope: "session".into(),
                count: 4,
                total_seconds: 6.1,
            }],
            slowest_files: vec![FileEntry {
                file: "tests/test_a.py".into(),
                total_seconds: 20.0,
                pct: 66.7,
            }],
            coverage_waste: Some(CoverageWaste {
                wasted_seconds: 12.0,
                redundant_tests: 1,
                tests: vec![WasteTest {
                    nodeid: "tests/test_a.py::test_redundant".into(),
                    duration: 12.0,
                    covered_lines: 40,
                    also_covered_by: 3,
                }],
            }),
            leaks: Vec::new(),
        }
    }

    /// Record one completed test (setup/call/teardown) on `worker` with the
    /// given call duration, mirroring what the pool feeds `Run::record`.
    pub fn record_test(run: &mut Run, nodeid: &str, worker: usize, dur: f64) {
        let r = |when: &str, duration: f64| crate::scheduling::proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: "passed".into(),
            duration,
            longrepr: None,
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            thread_delta: None,
            fd_delta: None,
            sections: Vec::new(),
            lineno: None,
        };
        run.record(Some(worker), r("setup", 0.0));
        run.record(Some(worker), r("call", dur));
        run.record(Some(worker), r("teardown", 0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::record_test;
    use super::*;

    fn teardown_with_leak(run: &mut Run, nodeid: &str, threads: Option<i64>, fds: Option<i64>) {
        let rep = |when: &str, td: Option<i64>, fd: Option<i64>| crate::scheduling::proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: "passed".into(),
            duration: 0.1,
            longrepr: None,
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            thread_delta: td,
            fd_delta: fd,
            sections: Vec::new(),
            lineno: None,
        };
        run.record(None, rep("setup", None, None));
        run.record(None, rep("call", None, None));
        run.record(None, rep("teardown", threads, fds));
    }

    #[test]
    fn detect_leaks_flags_positive_deltas_worst_first() {
        let mut run = Run::default();
        teardown_with_leak(&mut run, "t.py::clean", None, None);
        teardown_with_leak(&mut run, "t.py::released", Some(0), Some(0)); // opened+closed
        teardown_with_leak(&mut run, "t.py::one_fd", None, Some(1));
        teardown_with_leak(&mut run, "t.py::big", Some(3), Some(2)); // worst
        teardown_with_leak(&mut run, "t.py::negative", Some(-1), None); // freed, not a leak

        let leaks = detect_leaks(&run);
        let ids: Vec<&str> = leaks.iter().map(|l| l.nodeid.as_str()).collect();
        assert_eq!(
            ids,
            vec!["t.py::big", "t.py::one_fd"],
            "only real leaks, worst first"
        );
        assert_eq!((leaks[0].threads, leaks[0].fds), (3, 2));
    }

    #[test]
    fn all_work_on_one_worker_reports_max_imbalance() {
        // -n 8 but every test lands on gw0: the seven idle workers are
        // absent from the per-worker map, yet imbalance must read ~100%,
        // not 0%.
        let mut run = Run::default();
        for i in 0..4 {
            record_test(&mut run, &format!("t.py::t{i}"), 0, 2.0);
        }
        let pe = analyze(&run, &[], 8.0, 8, None)
            .parallel_efficiency
            .expect("multi-worker run has efficiency");
        assert_eq!(pe.workers_busy.len(), 1);
        assert!(
            (pe.imbalance_pct - 100.0).abs() < 1e-6,
            "imbalance {} should be ~100%",
            pe.imbalance_pct
        );
        // test_time 8.0 over wall 8.0 => 1× realized of 8× possible.
        assert!((pe.realized_speedup - 1.0).abs() < 1e-6);
        assert_eq!(pe.ideal_speedup, 8);
        assert!((pe.efficiency_pct - 12.5).abs() < 1e-6);
        assert!((pe.long_pole_seconds - 2.0).abs() < 1e-6);
    }

    #[test]
    fn balanced_workers_report_low_imbalance() {
        let mut run = Run::default();
        record_test(&mut run, "t.py::a", 0, 10.0);
        record_test(&mut run, "t.py::b", 1, 10.0);
        let pe = analyze(&run, &[], 10.0, 2, None)
            .parallel_efficiency
            .expect("multi-worker run has efficiency");
        assert_eq!(pe.workers_busy.len(), 2);
        assert!(
            pe.imbalance_pct.abs() < 1e-6,
            "imbalance {} should be 0%",
            pe.imbalance_pct
        );
        // 20.0s test time over 10.0s wall => 2× of 2× possible.
        assert!((pe.realized_speedup - 2.0).abs() < 1e-6);
        assert!((pe.efficiency_pct - 100.0).abs() < 1e-6);
    }

    #[test]
    fn some_idle_workers_still_counted() {
        // 4 workers configured, 3 active, one loaded heavier: min must be
        // the idle 0, so imbalance reflects the heaviest vs idle gap.
        let mut run = Run::default();
        record_test(&mut run, "t.py::a", 0, 8.0);
        record_test(&mut run, "t.py::b", 1, 4.0);
        record_test(&mut run, "t.py::c", 2, 4.0);
        let pe = analyze(&run, &[], 9.0, 4, None)
            .parallel_efficiency
            .expect("multi-worker run has efficiency");
        assert_eq!(pe.workers_busy.len(), 3);
        // max 8.0, min 0.0 (idle gw3) => 100%.
        assert!((pe.imbalance_pct - 100.0).abs() < 1e-6);
    }

    /// Line -> nodeids that covered it (test fixture shorthand).
    type LineSpec<'a> = (u32, &'a [&'a str]);
    /// (file path, lines) for one file in a test index.
    type FileSpec<'a> = (&'a str, &'a [LineSpec<'a>]);

    /// Build a coverage index from `file -> [(line, [nodeids])]` (hash unused
    /// by the waste analysis, so a fixed placeholder is fine).
    fn cov(files: &[FileSpec]) -> CoverageIndex {
        use crate::select::CoverageFile;
        let mut idx = CoverageIndex {
            schema: 1,
            files: HashMap::new(),
        };
        for (path, lines) in files {
            let mut lm = HashMap::new();
            for (ln, ids) in *lines {
                lm.insert(*ln, ids.iter().map(|s| s.to_string()).collect());
            }
            idx.files.insert(
                (*path).to_string(),
                CoverageFile {
                    hash: "H".into(),
                    lines: lm,
                },
            );
        }
        idx
    }

    fn durs<'a>(pairs: &[(&'a str, f64)]) -> HashMap<&'a str, f64> {
        pairs.iter().map(|(id, d)| (*id, *d)).collect()
    }

    #[test]
    fn zero_unique_slow_test_is_flagged_as_waste() {
        // test_dup covers only lines test_keep also covers (mod.py:1-2), so it
        // adds no unique coverage. test_keep owns a unique line (mod.py:3).
        let idx = cov(&[(
            "mod.py",
            &[
                (1, &["t.py::test_dup", "t.py::test_keep"]),
                (2, &["t.py::test_dup", "t.py::test_keep"]),
                (3, &["t.py::test_keep"]),
            ],
        )]);
        let d = durs(&[("t.py::test_dup", 5.0), ("t.py::test_keep", 5.0)]);
        let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS).expect("a waste candidate");
        assert_eq!(cw.redundant_tests, 1);
        assert_eq!(cw.tests.len(), 1);
        assert_eq!(cw.tests[0].nodeid, "t.py::test_dup");
        assert_eq!(cw.tests[0].covered_lines, 2);
        // Its two lines are shared with exactly one other test (test_keep).
        assert_eq!(cw.tests[0].also_covered_by, 1);
        assert!((cw.wasted_seconds - 5.0).abs() < 1e-6);
    }

    #[test]
    fn a_fast_redundant_test_is_below_the_floor() {
        // Fully redundant, but too fast to be worth deleting for time.
        let idx = cov(&[("mod.py", &[(1, &["t.py::test_dup", "t.py::test_keep"])])]);
        let d = durs(&[("t.py::test_dup", 0.1), ("t.py::test_keep", 0.1)]);
        assert!(coverage_waste(&d, &idx, WASTE_MIN_SECONDS).is_none());
    }

    #[test]
    fn a_test_with_any_unique_line_is_not_waste() {
        // test_a shares line 1 but owns line 2 - not redundant despite being slow.
        let idx = cov(&[(
            "mod.py",
            &[
                (1, &["t.py::test_a", "t.py::test_b"]),
                (2, &["t.py::test_a"]),
            ],
        )]);
        let d = durs(&[("t.py::test_a", 9.0), ("t.py::test_b", 9.0)]);
        let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS).expect("test_b is redundant");
        let ids: Vec<&str> = cw.tests.iter().map(|t| t.nodeid.as_str()).collect();
        assert_eq!(ids, vec!["t.py::test_b"], "only the fully-shared test");
    }

    #[test]
    fn a_tests_own_file_lines_dont_count_as_unique_coverage() {
        // Under `--cov=.` the index also records each test's own body (a line
        // only that test runs). If those counted, no test would ever look
        // redundant. Here both tests cover the same product line (mod.py:1) and
        // each also "covers" its own test-file body line - which must be ignored,
        // so test_dup still reads as pure product-redundant.
        let idx = cov(&[
            ("mod.py", &[(1, &["t.py::test_dup", "t.py::test_keep"])]),
            // The test file itself, each test's own line (self-only coverage):
            (
                "t.py",
                &[(1, &["t.py::test_dup"]), (2, &["t.py::test_keep"])],
            ),
        ]);
        let d = durs(&[("t.py::test_dup", 5.0), ("t.py::test_keep", 5.0)]);
        let cw =
            coverage_waste(&d, &idx, WASTE_MIN_SECONDS).expect("still redundant on product code");
        let ids: Vec<&str> = cw.tests.iter().map(|t| t.nodeid.as_str()).collect();
        // Both are product-redundant (mod.py:1 shared, no unique product line),
        // but deleting both loses mod.py:1 - only one (tie -> first nodeid) is.
        assert_eq!(ids, vec!["t.py::test_dup"], "{ids:?}");
        assert_eq!(cw.redundant_tests, 1);
        assert!((cw.wasted_seconds - 5.0).abs() < 1e-6);
        // covered_lines counts PRODUCT lines only (1), not the test-file body.
        let dup = cw
            .tests
            .iter()
            .find(|t| t.nodeid == "t.py::test_dup")
            .unwrap();
        assert_eq!(dup.covered_lines, 1);
    }

    #[test]
    fn an_identical_pair_flags_only_one_and_counts_its_time_once() {
        // test_a and test_b cover exactly the same lines: each has zero unique
        // lines, but deleting both loses them. Only the slower one is waste.
        let idx = cov(&[(
            "mod.py",
            &[
                (1, &["t.py::test_a", "t.py::test_b"]),
                (2, &["t.py::test_a", "t.py::test_b"]),
            ],
        )]);
        let d = durs(&[("t.py::test_a", 4.0), ("t.py::test_b", 6.0)]);
        let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS).expect("one is redundant");
        let ids: Vec<&str> = cw.tests.iter().map(|t| t.nodeid.as_str()).collect();
        assert_eq!(ids, vec!["t.py::test_b"], "slowest of the pair");
        assert_eq!(cw.redundant_tests, 1);
        assert!((cw.wasted_seconds - 6.0).abs() < 1e-6);
        // Its co-coverer is the kept test only.
        assert_eq!(cw.tests[0].also_covered_by, 1);
    }

    #[test]
    fn an_identical_triple_keeps_one_coverer() {
        let ids3: &[&str] = &["t.py::a", "t.py::b", "t.py::c"];
        let idx = cov(&[("mod.py", &[(1, ids3)])]);
        let d = durs(&[("t.py::a", 3.0), ("t.py::b", 3.0), ("t.py::c", 3.0)]);
        let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS).expect("two are redundant");
        assert_eq!(cw.redundant_tests, 2);
        assert!((cw.wasted_seconds - 6.0).abs() < 1e-6);
    }

    #[test]
    fn no_coverage_index_yields_no_section() {
        // analyze without an index never produces a waste section.
        let mut run = Run::default();
        record_test(&mut run, "t.py::a", 0, 5.0);
        assert!(analyze(&run, &[], 5.0, 1, None).coverage_waste.is_none());
    }
}
