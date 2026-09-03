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

pub use gate::{evaluate, parse_conditions};
pub use render::{append_ci_summary, render, write_markdown};

use std::collections::BTreeMap;

use serde::Serialize;

use crate::reporting::report::Run;
use crate::scheduling::proto::FixtureStat;

/// Bump when the JSON shape changes incompatibly.
const SCHEMA_VERSION: u32 = 2;

#[derive(Serialize)]
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
    /// Tests that leaked threads / fds (net positive after teardown). Empty
    /// unless leak-check instrumentation ran (`--doctor` / `--fail-on-leak`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leaks: Vec<Leak>,
}

/// A test that ended with more threads / open fds than it started — a resource
/// it opened and never released (its own teardown included).
#[derive(Serialize)]
pub struct Leak {
    pub nodeid: String,
    /// Net threads leaked (0 if only fds leaked).
    pub threads: i64,
    /// Net open fds leaked (0 if only threads leaked).
    pub fds: i64,
}

#[derive(Serialize)]
struct WaitBound {
    wait_seconds: f64,
    wait_pct: f64,
    tests: Vec<WaitTest>,
}

#[derive(Serialize)]
struct WaitTest {
    nodeid: String,
    duration: f64,
    wait: f64,
}

#[derive(Serialize)]
struct ParallelFloor {
    longest_seconds: f64,
    ideal_share_seconds: f64,
    gate_tests: Vec<GateTest>,
}

#[derive(Serialize)]
struct GateTest {
    nodeid: String,
    duration: f64,
}

/// Realized parallel speedup measured from an actual run. Unlike
/// `ParallelFloor` (a static pre-run estimate), this is the after-the-fact
/// "why isn't `-n auto` faster?". Only for multi-worker pool runs.
#[derive(Serialize)]
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
struct WorkerLoad {
    worker: String,
    busy_seconds: f64,
    tests: usize,
}

#[derive(Serialize)]
struct FixtureEntry {
    name: String,
    scope: String,
    count: u64,
    total_seconds: f64,
}

#[derive(Serialize)]
struct FileEntry {
    file: String,
    total_seconds: f64,
    pct: f64,
}

pub fn analyze(run: &Run, fixtures: &[FixtureStat], wall: f64, workers: usize) -> DoctorReport {
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
        let file = id.split("::").next().unwrap_or(id);
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
        leaks,
    }
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
        let pe = analyze(&run, &[], 8.0, 8)
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
        let pe = analyze(&run, &[], 10.0, 2)
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
        let pe = analyze(&run, &[], 9.0, 4)
            .parallel_efficiency
            .expect("multi-worker run has efficiency");
        assert_eq!(pe.workers_busy.len(), 3);
        // max 8.0, min 0.0 (idle gw3) => 100%.
        assert!((pe.imbalance_pct - 100.0).abs() < 1e-6);
    }
}
