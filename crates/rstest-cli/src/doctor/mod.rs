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

use crate::config::ProjectConfig;
use crate::reporting::report::Run;
use crate::scheduling::proto::FixtureStat;
use crate::select::{CoverageFile, CoverageIndex};

/// Bump when the JSON shape changes incompatibly.
const SCHEMA_VERSION: u32 = 3;

/// Only tests at least this slow are worth flagging as coverage waste: deleting
/// a fast redundant test frees no meaningful time.
const WASTE_MIN_SECONDS: f64 = 0.5;

/// A test is a parallel floor only when it outlasts a worker's ideal share by
/// more than this factor (10%), so a balanced pool is not flagged on jitter.
const FLOOR_SLACK: f64 = 1.1;

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DoctorReport {
    /// Document schema version, bumped when the shape changes incompatibly.
    schema: u32,
    /// The rstest version that wrote the report.
    rstest_version: &'static str,
    /// Workers that ran tests: 1 for a single-worker (`-n 0` / `-n 1`) run.
    workers: usize,
    /// Wall-clock time of the whole run, in seconds.
    wall_seconds: f64,
    /// Wall from pool spawn to every worker's first event (imported core +
    /// started collecting), part of `wall_seconds`. A fixed per-run tax that
    /// `--fork-pool` (Unix) cuts at high `-n`; 0.0 on single-worker runs.
    /// Surfaced so a startup-bound suite is legible.
    startup_seconds: f64,
    /// Whether this run already used `--fork-pool` (Unix fork-prewarm). Gates
    /// the "try --fork-pool" hint so it isn't suggested when already on.
    fork_prewarm: bool,
    /// Tests with a recorded call duration (tests skipped before their call
    /// phase drop out): the population every time figure is computed over.
    tests: usize,
    /// Sum of each test's whole protocol (setup + call + teardown), so time
    /// spent in function fixtures counts.
    test_time_seconds: f64,
    /// Sum of whole-protocol CPU time (the worker process plus child processes
    /// it waited for), over tests where it was measured.
    cpu_time_seconds: f64,
    /// Present only when waiting is a notable share (>= 20% and >= 1s);
    /// `--doctor-fail-on` gates `wait_*` on the measured values regardless.
    wait_bound: Option<WaitBound>,
    /// `test_time - cpu_time` and its share of test time, whenever CPU was
    /// measured for at least one test. Gate-only (not serialized): the JSON
    /// `wait_bound` section keeps its display threshold.
    #[serde(skip)]
    measured_wait: Option<(f64, f64)>,
    /// Slowest single test (whole protocol), at any worker count. Gate-only;
    /// the JSON carries it under `parallel_efficiency` on pool runs.
    #[serde(skip)]
    long_pole_seconds: Option<f64>,
    /// Present only when the longest test outlasts both a worker's even share
    /// of the test time and 1 second, plus 10%: no worker count can finish
    /// faster than that test.
    parallel_floor: Option<ParallelFloor>,
    /// Realized speedup and worker load balance; present only on multi-worker
    /// pool runs.
    parallel_efficiency: Option<ParallelEfficiency>,
    /// Fixture setup cost summed across all workers, costliest first (top 50).
    fixtures: Vec<FixtureEntry>,
    /// Test time per file, slowest first (top 20).
    slowest_files: Vec<FileEntry>,
    /// Slow tests whose every covered line is also covered by another test -
    /// delete/merge candidates. `None` unless a per-test coverage index was
    /// warm (`--cov --cov-context=test`) and at least one test qualified.
    coverage_waste: Option<CoverageWaste>,
    /// Suggested `--timeout` and per-test `@pytest.mark.timeout` overrides,
    /// from each test's slowest recorded passing run (call phase). `null`
    /// until some test in this run has 5 passing runs in the run history.
    timeouts: Option<TimeoutAdvice>,
    /// Tests whose typical call time crept up across their recorded runs, in
    /// steps too small for `--durations-regress`. `null` unless one did.
    drift: Option<Drift>,
    /// Tests that leaked threads / fds (created by the test, still open after
    /// its teardown). Empty
    /// unless leak-check instrumentation ran (`--doctor` / `--fail-on-leak`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leaks: Vec<Leak>,
}

/// Slow tests that add no unique coverage: every line each one executes is also
/// executed by some other test, so it can be deleted or merged without dropping
/// any covered line. Pure suite bloat on the time axis.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct CoverageWaste {
    /// Sum of the durations of every redundant slow test (not just the shown
    /// ones) - the time reclaimable by pruning them.
    wasted_seconds: f64,
    /// Count of redundant slow tests found (`tests` shows the slowest of them).
    redundant_tests: usize,
    /// The slowest redundant tests, worst first (capped).
    tests: Vec<WasteTest>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct WasteTest {
    /// The redundant test.
    nodeid: String,
    /// Its whole-protocol time (setup + call + teardown), in seconds.
    duration: f64,
    /// Lines this test covered, all shared with at least one other test.
    covered_lines: u64,
    /// Distinct OTHER tests that between them also cover those lines.
    also_covered_by: u64,
}

/// Timeouts that catch a hang without killing a slow-but-healthy test. Each
/// test's own limit is the larger of 3x its slowest recorded passing run and
/// that run plus 1s, rounded up to whole seconds.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct TimeoutAdvice {
    /// Suggested suite-wide `--timeout`, in seconds: the limit that covers 95%
    /// of the judged tests.
    suite_timeout: u64,
    /// Tests in this run with at least 5 passing runs in the history.
    tests_judged: usize,
    /// Judged tests whose own limit is above `suite_timeout`: they need a
    /// per-test `@pytest.mark.timeout`.
    override_count: usize,
    /// The highest of those, worst first (top 20).
    overrides: Vec<TimeoutOverride>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct TimeoutOverride {
    /// The test.
    nodeid: String,
    /// Its slowest recorded passing call-phase time, in seconds (the phase
    /// `--timeout` limits; fixture setup/teardown not included).
    slowest_seconds: f64,
    /// Passing runs that slowest time comes from.
    runs: usize,
    /// Its suggested `@pytest.mark.timeout`, in seconds.
    timeout: u64,
}

/// Slow creep: a test whose median call time rose from the older third of
/// its recorded passing runs to the middle third to the newest, by at least
/// 1.5x and 0.2s overall.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct Drift {
    /// Tests in this run that drift.
    drifting_tests: usize,
    /// The largest rises, worst first (top 10).
    tests: Vec<DriftTest>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct DriftTest {
    /// The test.
    nodeid: String,
    /// Median call-phase seconds over the oldest third of its passing runs.
    older_median: f64,
    /// Median over the middle third.
    middle_median: f64,
    /// Median over the newest third.
    newer_median: f64,
    /// Passing runs the medians come from.
    runs: usize,
}

/// Passing runs a test needs before its trend is judged (three per window).
const DRIFT_MIN_RUNS: usize = 9;
const DRIFT_SHOWN: usize = 10;

/// Older/middle/newer medians of `calls` (oldest first) when they rise step
/// by step, at least 1.5x and 0.2s overall.
pub(crate) fn drift_of(calls: &[f64]) -> Option<(f64, f64, f64)> {
    if calls.len() < DRIFT_MIN_RUNS {
        return None;
    }
    let third = calls.len() / 3;
    let med = |w: &[f64]| {
        let mut w = w.to_vec();
        w.sort_by(f64::total_cmp);
        crate::explain::median(&w)
    };
    let (a, b, c) = (
        med(&calls[..third])?,
        med(&calls[third..2 * third])?,
        med(&calls[2 * third..])?,
    );
    (a < b && b < c && c >= a * 1.5 && c - a >= 0.2).then_some((a, b, c))
}

fn drift(run: &Run, history: &crate::reporting::history::History) -> Option<Drift> {
    let round = |x: f64| (x * 1e4).round() / 1e4;
    let mut tests: Vec<DriftTest> = run
        .tests()
        .keys()
        .filter_map(|id| {
            let calls: Vec<f64> = history
                .series(id)
                .iter()
                .filter(|(_, e)| matches!(e.outcome_name(), "passed" | "flaky"))
                .filter_map(|(_, e)| e.call.map(f64::from))
                .collect();
            let (a, b, c) = drift_of(&calls)?;
            Some(DriftTest {
                nodeid: id.clone(),
                older_median: round(a),
                middle_median: round(b),
                newer_median: round(c),
                runs: calls.len(),
            })
        })
        .collect();
    if tests.is_empty() {
        return None;
    }
    tests.sort_by(|x, y| {
        (y.newer_median - y.older_median)
            .total_cmp(&(x.newer_median - x.older_median))
            .then_with(|| x.nodeid.cmp(&y.nodeid))
    });
    let drifting_tests = tests.len();
    tests.truncate(DRIFT_SHOWN);
    Some(Drift {
        drifting_tests,
        tests,
    })
}

/// Overrides listed in a report (`override_count` has the total).
const TIMEOUT_OVERRIDES_SHOWN: usize = 20;

/// A test's own limit from its slowest passing run: 3x, at least +1s.
fn timeout_for(slowest: f64) -> u64 {
    ((slowest * 3.0).max(slowest + 1.0).ceil() as u64).max(1)
}

/// The timeout advice for the tests in `run`, from their passing call times
/// in `history`; `None` when no test has enough runs.
fn timeout_advice(
    run: &Run,
    history: &crate::reporting::history::History,
) -> Option<TimeoutAdvice> {
    let mut judged: Vec<(String, f64, usize, u64)> = run
        .tests()
        .keys()
        .filter_map(|id| {
            let calls = crate::scheduling::durations::passing_calls(history, id)?;
            let slowest = calls.iter().copied().fold(0.0, f64::max);
            Some((id.clone(), slowest, calls.len(), timeout_for(slowest)))
        })
        .collect();
    if judged.is_empty() {
        return None;
    }
    let mut limits: Vec<u64> = judged.iter().map(|t| t.3).collect();
    limits.sort_unstable();
    // Nearest-rank 95th percentile.
    let suite_timeout = limits[(limits.len() * 95).div_ceil(100) - 1];
    let tests_judged = judged.len();
    judged.retain(|t| t.3 > suite_timeout);
    judged.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    let override_count = judged.len();
    judged.truncate(TIMEOUT_OVERRIDES_SHOWN);
    Some(TimeoutAdvice {
        suite_timeout,
        tests_judged,
        override_count,
        overrides: judged
            .into_iter()
            .map(|(nodeid, slowest, runs, timeout)| TimeoutOverride {
                nodeid,
                slowest_seconds: (slowest * 1e4).round() / 1e4,
                runs,
                timeout,
            })
            .collect(),
    })
}

impl DoctorReport {
    /// Add the history-based sections ([`TimeoutAdvice`], [`Drift`]).
    pub fn with_history(mut self, run: &Run, history: &crate::reporting::history::History) -> Self {
        self.timeouts = timeout_advice(run, history);
        self.drift = drift(run, history);
        self
    }
}

/// A test that ended with more threads / open fds than it started: a resource
/// it opened and never released (its own teardown included).
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Leak {
    /// The test that leaked.
    pub nodeid: String,
    /// Threads the test created that outlived its teardown (0 if only fds
    /// leaked).
    pub threads: i64,
    /// Fds the test opened that are still open after its teardown (0 if only
    /// threads leaked).
    pub fds: i64,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct WaitBound {
    /// Test time spent waiting rather than on CPU (`test_time_seconds -
    /// cpu_time_seconds`).
    wait_seconds: f64,
    /// `wait_seconds` as a percentage of `test_time_seconds`.
    wait_pct: f64,
    /// The biggest waiters, most waiting first (top 50): tests of at least
    /// 0.2s that spent 60% or more of their time waiting.
    tests: Vec<WaitTest>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct WaitTest {
    /// The waiting test.
    nodeid: String,
    /// Its whole-protocol time (setup + call + teardown), in seconds.
    duration: f64,
    /// The part of `duration` not spent on CPU, in seconds.
    wait: f64,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct ParallelFloor {
    /// The longest single test (setup + call + teardown), in seconds.
    longest_seconds: f64,
    /// A worker's even share of the test time (`test_time_seconds /
    /// workers`), in seconds.
    ideal_share_seconds: f64,
    /// The tests that exceed the floor, longest first (at most 10).
    gate_tests: Vec<GateTest>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct GateTest {
    /// The test that gates the wall time.
    nodeid: String,
    /// Its whole-protocol time (setup + call + teardown), in seconds.
    duration: f64,
}

/// Realized parallel speedup measured from an actual run. Unlike
/// `ParallelFloor` (a static pre-run estimate), this is the after-the-fact
/// "why isn't `-n auto` faster?". Only for multi-worker pool runs.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct ParallelEfficiency {
    /// test_time / wall. At most `ideal_speedup`, since each worker runs one
    /// test at a time; a wait-bound suite run with `-n` above the core count
    /// can realize more than the core count.
    realized_speedup: f64,
    /// Worker count (`-n`) - the ceiling for a purely CPU-bound suite.
    ideal_speedup: usize,
    /// 100 * realized / ideal: how busy the workers were, up to 100%.
    efficiency_pct: f64,
    /// Busy time summed per worker, descending - the load-balance picture.
    workers_busy: Vec<WorkerLoad>,
    /// 100 * (busiest - idlest) / busiest. High = uneven distribution.
    imbalance_pct: f64,
    /// Slowest single test (setup + call + teardown): the hard floor no
    /// worker count beats.
    long_pole_seconds: f64,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct WorkerLoad {
    /// Worker id (`gw0`, `gw1`, ...), or `serial` for tests with no
    /// recorded worker.
    worker: String,
    /// Test time this worker spent running tests, in seconds.
    busy_seconds: f64,
    /// Tests this worker ran.
    tests: usize,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct FixtureEntry {
    /// Fixture name.
    name: String,
    /// pytest scope: `function`, `class`, `module`, `package` or `session`.
    scope: String,
    /// Setups across all workers.
    count: u64,
    /// Setup time summed across all workers, in seconds.
    total_seconds: f64,
    /// Scope-promotion advisor: a function-scoped fixture that produced the
    /// same immutable builtin value on every call in every worker, with no
    /// per-test teardown or narrower-scoped inputs (checked worker-side), a
    /// candidate for `@pytest.fixture(scope="session")`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    constant: bool,
    /// Projected wall-time saved by promoting this candidate to session scope:
    /// the largest per-worker-session `(calls - 1) * mean_setup`, i.e. the
    /// redundant re-setups removed on the worker that benefits most.
    /// 0 unless `constant`.
    #[serde(default, skip_serializing_if = "is_zero")]
    projected_saving_seconds: f64,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct FileEntry {
    /// Test file, the path part of its tests' nodeids.
    file: String,
    /// Summed whole-protocol time of the file's tests, in seconds.
    total_seconds: f64,
    /// `total_seconds` as a percentage of `test_time_seconds`.
    pct: f64,
}

pub fn analyze(
    run: &Run,
    fixtures: &[FixtureStat],
    wall: f64,
    startup: f64,
    fork_prewarm: bool,
    workers: usize,
    coverage: Option<(&CoverageIndex, &ProjectConfig)>,
) -> DoctorReport {
    // A single-worker run (`-n 0` runs in one worker process) still has one
    // worker doing the work.
    let workers = workers.max(1);
    let tests = run.tests();
    // Each test's time is its whole protocol (setup + call + teardown): the
    // span it held its worker. Call-phase-only time would hide a suite whose
    // cost lives in function fixtures and misread its parallel speedup.
    let mut durations: Vec<(&String, f64, Option<f64>)> = tests
        .iter()
        .filter_map(|(id, e)| e.protocol_seconds().map(|d| (id, d, e.protocol_cpu())))
        .collect();
    let test_time: f64 = durations.iter().map(|(_, d, _)| d).sum();
    let cpu_time: f64 = durations.iter().filter_map(|(_, _, c)| *c).sum();
    let n_cpu = durations.iter().filter(|(_, _, c)| c.is_some()).count();

    // -- Wait-bound: wall vs cpu ---------------------------------------
    let measured_wait = (n_cpu > 0).then(|| {
        let wait = (test_time - cpu_time).max(0.0);
        (wait, 100.0 * wait / test_time.max(f64::EPSILON))
    });
    let wait_bound = if let Some((wait, pct)) = measured_wait {
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
    let long_pole = durations.first().map(|(_, d, _)| *d);
    let parallel_floor = durations.first().and_then(|&(_, longest, _)| {
        let ideal = test_time / workers.max(1) as f64;
        // A test only gates the wall when it clearly outlasts a worker's even
        // share: N equal tests on N workers sit exactly at the share, and
        // timing jitter alone must not flag that perfectly balanced pool.
        let floor = ideal.max(1.0) * FLOOR_SLACK;
        (longest > floor).then(|| ParallelFloor {
            longest_seconds: longest,
            ideal_share_seconds: ideal,
            gate_tests: durations
                .iter()
                .take(10)
                .filter(|(_, d, _)| *d > floor)
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
            if let Some(d) = e.protocol_seconds() {
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
            long_pole_seconds: long_pole.unwrap_or(0.0),
        }
    });

    // -- Fixtures ----------------------------------------------------------
    let mut fx: Vec<FixtureEntry> = fixtures
        .iter()
        .map(|f| {
            // Workers report `constant` as "never varied here", including a
            // session that ran it only once; `repeated` says some session
            // actually compared two values. Promotion runs the fixture once
            // per worker session, so each session's redundant setup is every
            // call after its first; `redundant` is the largest of those, the
            // wall time saved on the worker that benefits most.
            let constant = f.constant && f.repeated;
            let saving = if constant { f.redundant } else { 0.0 };
            FixtureEntry {
                name: f.name.clone(),
                scope: f.scope.clone(),
                count: f.count,
                total_seconds: f.total,
                constant,
                projected_saving_seconds: saving,
            }
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
    // `coverage` is this run's own per-test index (see run_post_gates), so its
    // nodeids and line numbers describe the code as it is now. Only PASSED tests
    // count: a skipped or failed test's call time says nothing about what it
    // covers, and it is no stand-in for a test that did run.
    let coverage_waste = coverage.and_then(|(index, project)| {
        let passed = run.green_nodeids();
        let duration_of: HashMap<&str, f64> = durations
            .iter()
            .filter(|(id, _, _)| passed.contains(id.as_str()))
            .map(|(id, d, _)| (id.as_str(), *d))
            .collect();
        coverage_waste(&duration_of, index, WASTE_MIN_SECONDS, |file| {
            crate::collect::is_test_file(std::path::Path::new(file), project)
        })
    });

    let leaks = detect_leaks(run);

    DoctorReport {
        schema: SCHEMA_VERSION,
        rstest_version: env!("CARGO_PKG_VERSION"),
        workers,
        wall_seconds: wall,
        startup_seconds: startup,
        fork_prewarm,
        tests: durations.len(),
        test_time_seconds: test_time,
        cpu_time_seconds: cpu_time,
        wait_bound,
        measured_wait,
        long_pole_seconds: long_pole,
        parallel_floor,
        parallel_efficiency,
        fixtures: fx,
        slowest_files: files,
        coverage_waste,
        timeouts: None,
        drift: None,
        leaks,
    }
}

/// Slow tests that can be deleted TOGETHER without losing any covered line -
/// pure redundant suite cost. `duration_of` maps nodeid -> protocol time of the
/// tests eligible to count (passed this run); only tests at least `min_seconds`
/// slow qualify, and a nodeid absent from it is not counted as a coverer.
/// `is_test_file` tells test code (excluded) from product code. `None` when the
/// index is empty or nothing qualifies.
///
/// Candidates are picked greedily, slowest first: one is accepted only while
/// every line it covers is still covered by some other test that is kept. Two
/// slow tests with the same coverage give one candidate, not two.
fn coverage_waste(
    duration_of: &HashMap<&str, f64>,
    index: &CoverageIndex,
    min_seconds: f64,
    is_test_file: impl Fn(&str) -> bool,
) -> Option<CoverageWaste> {
    // Measure redundancy over PRODUCT code only. Under `--cov=.` the index also
    // records each test's OWN file, whose body lines only that test executes -
    // counting them would make every test trivially "unique" and hide real
    // waste. Test files are decided by the project's `python_files` patterns,
    // not by "some nodeid lives here": a doctest nodeid (`pkg/mod.py::pkg.mod.f`
    // under `--doctest-modules`) lives in a product module.
    let source: Vec<(&str, &CoverageFile)> = index
        .files
        .iter()
        .filter(|(file, _)| !is_test_file(file))
        .map(|(file, cov)| (file.as_str(), cov))
        .collect();
    let live = |id: &str| duration_of.contains_key(id);

    // Pass 1, O(total coverage entries): per live test, covered PRODUCT lines
    // and whether it ALONE covers any (a line's live coverers are exactly the
    // current-run tests that hit it).
    let mut total: HashMap<&str, u64> = HashMap::new();
    let mut has_unique: HashSet<&str> = HashSet::new();
    for (_, cov) in &source {
        for ids in cov.lines.values() {
            let coverers: Vec<&str> = ids.iter().map(String::as_str).filter(|i| live(i)).collect();
            for id in &coverers {
                *total.entry(id).or_default() += 1;
            }
            if let [solo] = coverers[..] {
                has_unique.insert(solo);
            }
        }
    }
    let dur = |id: &str| duration_of.get(id).copied().unwrap_or(0.0);
    // Individually redundant: covers >= 1 line, none alone, slow enough.
    let mut pool: Vec<&str> = total
        .keys()
        .copied()
        .filter(|id| !has_unique.contains(id))
        .filter(|id| dur(id) >= min_seconds)
        .collect();
    if pool.is_empty() {
        return None;
    }
    // Slowest first (then nodeid for stability): greedy maximizes reclaimed time.
    pool.sort_by(|a, b| dur(b).total_cmp(&dur(a)).then(a.cmp(b)));
    let in_pool: HashSet<&str> = pool.iter().copied().collect();

    // Pass 2: each pool test's lines, the live coverer count per line, and the
    // distinct OTHER tests sharing its lines ("shared with N other tests").
    let mut lines_of: HashMap<&str, Vec<(&str, u32)>> = HashMap::new();
    let mut remaining: HashMap<(&str, u32), usize> = HashMap::new();
    let mut co: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (file, cov) in &source {
        for (line, ids) in &cov.lines {
            let coverers: Vec<&str> = ids.iter().map(String::as_str).filter(|i| live(i)).collect();
            let mut pooled = coverers.iter().filter(|i| in_pool.contains(**i)).peekable();
            if pooled.peek().is_none() {
                continue;
            }
            remaining.insert((file, *line), coverers.len());
            for id in pooled {
                lines_of.entry(id).or_default().push((file, *line));
                co.entry(id)
                    .or_default()
                    .extend(coverers.iter().filter(|o| *o != id));
            }
        }
    }

    // Greedy: accept a test only if every one of its lines keeps another coverer.
    let mut picked: Vec<&str> = Vec::new();
    for id in pool {
        let lines = &lines_of[id];
        if lines.iter().all(|l| remaining[l] >= 2) {
            for l in lines {
                *remaining.get_mut(l).expect("counted in pass 2") -= 1;
            }
            picked.push(id);
        }
    }
    let redundant_tests = picked.len();
    // Headline reclaimable time spans ALL picked tests, not just the shown ones.
    let wasted_seconds: f64 = picked.iter().map(|id| dur(id)).sum();
    // Already slowest first; keep the slowest for the detail table.
    picked.truncate(20);
    let tests = picked
        .iter()
        .map(|id| WasteTest {
            nodeid: (*id).to_string(),
            duration: dur(id),
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

/// Tests that leaked threads/fds (created by the test, still open after its
/// teardown), worst first.
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
    crate::reporting::write_output(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
}

#[cfg(test)]
mod timeout_tests {
    use super::testutil::record_test;
    use super::*;
    use crate::reporting::history::{outcome_code, Entry, History, RunMeta};

    /// History giving each test `runs` passing runs at `secs`, plus one
    /// failed run whose time must not count.
    fn history(tests: &[(&str, f32, usize)]) -> History {
        let mut h = History::default();
        let n = tests.iter().map(|t| t.2).max().unwrap_or(0) + 1;
        for seq in 0..n as u64 {
            h.runs.push(RunMeta {
                seq,
                id: format!("r{seq}"),
                epoch: seq,
                commit: None,
                env: String::new(),
            });
        }
        for (id, secs, runs) in tests {
            let mut series: std::collections::VecDeque<Entry> = (0..*runs)
                .map(|i| Entry {
                    run: i as u64,
                    outcome: outcome_code("passed").unwrap(),
                    call: Some(*secs),
                    setup: 0.0,
                })
                .collect();
            series.push_back(Entry {
                run: *runs as u64,
                outcome: outcome_code("failed").unwrap(),
                call: Some(99.0),
                setup: 0.0,
            });
            h.tests.insert(id.to_string(), series);
        }
        h
    }

    #[test]
    fn drift_needs_a_steady_rise_of_1_5x_and_0_2s() {
        let creep: Vec<f64> = (0..12).map(|i| 0.4 + 0.03 * f64::from(i)).collect();
        let (a, b, c) = drift_of(&creep).unwrap();
        assert!(a < b && b < c);
        // Under 9 runs: not judged.
        assert!(drift_of(&creep[..8]).is_none());
        // Noisy but flat: medians don't rise step by step.
        let flat = [1.0, 2.0, 1.5, 1.0, 2.0, 1.5, 1.0, 2.0, 1.5];
        assert!(drift_of(&flat).is_none());
        // Rising, but only 10%: noise, not drift.
        let small: Vec<f64> = (0..9).map(|i| 1.0 + 0.012 * f64::from(i)).collect();
        assert!(drift_of(&small).is_none());
        // 2x, but only 0.1s overall: too small to matter.
        let tiny: Vec<f64> = (0..9).map(|i| 0.1 + 0.0125 * f64::from(i)).collect();
        assert!(drift_of(&tiny).is_none());
        // A jump then flat: medians 1, 3, 3 don't rise twice.
        let jump = [1.0, 1.0, 1.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0];
        assert!(drift_of(&jump).is_none());
    }

    #[test]
    fn drift_section_lists_creeping_tests_in_this_run() {
        let mut h = History::default();
        for seq in 0..12u64 {
            h.runs.push(RunMeta {
                seq,
                id: format!("r{seq}"),
                epoch: seq,
                commit: None,
                env: String::new(),
            });
            for (id, secs) in [
                ("t.py::creep", 0.4 + 0.05 * seq as f32),
                ("t.py::steady", 0.5),
                ("t.py::gone", 0.1 + 0.2 * seq as f32),
            ] {
                h.tests.entry(id.into()).or_default().push_back(Entry {
                    run: seq,
                    outcome: crate::reporting::history::outcome_code("passed").unwrap(),
                    call: Some(secs),
                    setup: 0.0,
                });
            }
        }
        let mut run = Run::default();
        record_test(&mut run, "t.py::creep", 0, 1.0);
        record_test(&mut run, "t.py::steady", 0, 0.5);
        let d = drift(&run, &h).unwrap();
        assert_eq!(d.drifting_tests, 1);
        assert_eq!(d.tests[0].nodeid, "t.py::creep");
        assert_eq!(d.tests[0].runs, 12);
        assert!(drift(&run, &History::default()).is_none());
    }

    #[test]
    fn timeout_for_is_3x_with_a_one_second_floor() {
        assert_eq!(timeout_for(0.1), 2); // 0.1 + 1 = 1.1 -> 2
        assert_eq!(timeout_for(0.5), 2); // 1.5 -> 2
        assert_eq!(timeout_for(5.0), 15);
        assert_eq!(timeout_for(8.4), 26); // 25.2 -> 26
        assert_eq!(timeout_for(0.0), 1);
    }

    #[test]
    fn suite_timeout_covers_95_pct_and_slow_tests_get_overrides() {
        let mut tests: Vec<(String, f32, usize)> = (0..38)
            .map(|i| (format!("t.py::fast{i}"), 0.1, 5))
            .collect();
        tests.push(("t.py::slow".into(), 5.0, 6));
        tests.push(("t.py::slowest".into(), 8.4, 12));
        tests.push(("t.py::young".into(), 30.0, 4)); // under 5 runs: not judged
        tests.push(("t.py::not_run".into(), 50.0, 9)); // not in this run
        let refs: Vec<(&str, f32, usize)> = tests
            .iter()
            .map(|(id, s, n)| (id.as_str(), *s, *n))
            .collect();
        let h = history(&refs);
        let mut run = Run::default();
        for (id, ..) in &tests[..41] {
            record_test(&mut run, id, 0, 0.1);
        }
        let t = timeout_advice(&run, &h).unwrap();
        assert_eq!(t.tests_judged, 40);
        assert_eq!(t.suite_timeout, 2);
        assert_eq!(t.override_count, 2);
        let o: Vec<(&str, u64, usize)> = t
            .overrides
            .iter()
            .map(|o| (o.nodeid.as_str(), o.timeout, o.runs))
            .collect();
        assert_eq!(o, [("t.py::slowest", 26, 12), ("t.py::slow", 15, 6)]);
        // The failed 99s run never counts as "slowest".
        assert!((t.overrides[0].slowest_seconds - 8.4).abs() < 1e-3);

        // No history: no section.
        assert!(timeout_advice(&run, &History::default()).is_none());
        // A small suite: the 95th percentile is its slowest, so no overrides.
        let h = history(&[("a", 0.1, 5), ("b", 5.0, 5)]);
        let mut run = Run::default();
        record_test(&mut run, "a", 0, 0.1);
        record_test(&mut run, "b", 0, 0.1);
        let t = timeout_advice(&run, &h).unwrap();
        assert_eq!((t.suite_timeout, t.override_count), (15, 0));
    }
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
            startup_seconds: 0.3,
            fork_prewarm: false,
            tests,
            test_time_seconds: 30.0,
            cpu_time_seconds: 6.0,
            measured_wait: Some((24.0, 80.0)),
            long_pole_seconds: Some(8.4),
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
            fixtures: vec![
                FixtureEntry {
                    name: "db".into(),
                    scope: "session".into(),
                    count: 4,
                    total_seconds: 6.1,
                    constant: false,
                    projected_saving_seconds: 0.0,
                },
                FixtureEntry {
                    name: "settings".into(),
                    scope: "function".into(),
                    count: 40,
                    total_seconds: 4.0,
                    constant: true,
                    projected_saving_seconds: 0.9,
                },
            ],
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
            timeouts: Some(TimeoutAdvice {
                suite_timeout: 3,
                tests_judged: 40,
                override_count: 1,
                overrides: vec![TimeoutOverride {
                    nodeid: "tests/test_a.py::test_long".into(),
                    slowest_seconds: 8.4,
                    runs: 12,
                    timeout: 26,
                }],
            }),
            drift: Some(Drift {
                drifting_tests: 1,
                tests: vec![DriftTest {
                    nodeid: "tests/test_a.py::test_creep".into(),
                    older_median: 0.4,
                    middle_median: 0.55,
                    newer_median: 0.7,
                    runs: 30,
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
            subtest: false,
            crash: None,
            xfail_reason: None,
            skip_location: None,
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
            subtest: false,
            crash: None,
            xfail_reason: None,
            skip_location: None,
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

    /// One test whose time is mostly fixture setup/teardown, with per-phase
    /// wall and CPU as the worker reports them.
    fn record_phases(run: &mut Run, nodeid: &str, worker: usize, phases: [(f64, f64); 3]) {
        for (when, (duration, cpu)) in ["setup", "call", "teardown"].into_iter().zip(phases) {
            run.record(
                Some(worker),
                crate::scheduling::proto::Report {
                    nodeid: nodeid.into(),
                    when: when.into(),
                    outcome: "passed".into(),
                    duration,
                    longrepr: None,
                    wasxfail: false,
                    skip_reason: None,
                    cpu: Some(cpu),
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
    }

    #[test]
    fn fixture_time_counts_toward_every_number() {
        // 8 tests on 4 workers, each 0.25s setup + 0.02s call + 0.1s teardown,
        // all sleeping: the work is 2.96s, not the 0.16s of call phases.
        let mut run = Run::default();
        for i in 0..8 {
            record_phases(
                &mut run,
                &format!("t.py::t{i}"),
                i % 4,
                [(0.25, 0.0), (0.02, 0.0), (0.1, 0.0)],
            );
        }
        let r = analyze(&run, &[], 0.8, 0.0, false, 4, None);
        assert!(
            (r.test_time_seconds - 2.96).abs() < 1e-6,
            "{}",
            r.test_time_seconds
        );
        assert!((r.slowest_files[0].total_seconds - 2.96).abs() < 1e-6);
        let pe = r.parallel_efficiency.as_ref().unwrap();
        assert!((pe.long_pole_seconds - 0.37).abs() < 1e-6);
        assert!((pe.workers_busy[0].busy_seconds - 0.74).abs() < 1e-6);
        assert!((pe.realized_speedup - 3.7).abs() < 1e-6);
        // Fixture sleeps are waiting: shown and gateable.
        let wb = r.wait_bound.as_ref().expect("wait-bound section");
        assert!((wb.wait_pct - 100.0).abs() < 1e-6);
        assert_eq!(wb.tests.len(), 8);
        assert_eq!(r.long_pole_seconds, Some(0.37));
    }

    #[test]
    fn fixture_cpu_is_computing_not_waiting() {
        // A CPU-heavy fixture: setup/teardown CPU equals their wall, so the
        // test is not waiting.
        let mut run = Run::default();
        record_phases(
            &mut run,
            "t.py::hot",
            0,
            [(0.6, 0.6), (0.01, 0.01), (0.4, 0.4)],
        );
        let r = analyze(&run, &[], 1.1, 0.0, false, 1, None);
        assert!(r.wait_bound.is_none());
        let (wait, pct) = r.measured_wait.expect("cpu measured");
        assert!(wait.abs() < 1e-6 && pct.abs() < 1e-6, "{wait} {pct}");
    }

    #[test]
    fn single_worker_run_reports_one_worker_and_gateable_numbers() {
        // `-n 0` passes 0 workers: the report still says 1, keeps the long
        // pole, and measures wait below the WAIT-BOUND display threshold.
        let mut run = Run::default();
        record_phases(
            &mut run,
            "t.py::sleep",
            0,
            [(0.0, 0.0), (0.8, 0.0), (0.0, 0.0)],
        );
        record_phases(
            &mut run,
            "t.py::cpu",
            0,
            [(0.0, 0.0), (1.0, 1.0), (0.0, 0.0)],
        );
        let r = analyze(&run, &[], 1.9, 0.0, false, 0, None);
        assert_eq!(r.workers, 1);
        assert!(r.parallel_efficiency.is_none());
        assert!(
            r.wait_bound.is_none(),
            "0.8s wait is below the 1s display floor"
        );
        let (wait, pct) = r.measured_wait.unwrap();
        assert!((wait - 0.8).abs() < 1e-6 && (pct - 44.444).abs() < 0.01);
        assert_eq!(r.long_pole_seconds, Some(1.0));
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
        let pe = analyze(&run, &[], 8.0, 0.0, false, 8, None)
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
    fn balanced_pool_is_not_a_parallel_floor() {
        // Four ~1s tests on four workers: the longest sits at the ideal share
        // plus jitter, which is not a floor.
        let mut run = Run::default();
        for (i, d) in [1.0047, 1.0012, 1.0031, 1.0008].into_iter().enumerate() {
            record_test(&mut run, &format!("t.py::t{i}"), i, d);
        }
        assert!(analyze(&run, &[], 1.1, 0.0, false, 4, None)
            .parallel_floor
            .is_none());
        // One test clearly longer than the share still is.
        let mut run = Run::default();
        record_test(&mut run, "t.py::long", 0, 4.0);
        record_test(&mut run, "t.py::short", 1, 0.5);
        let floor = analyze(&run, &[], 4.0, 0.0, false, 2, None)
            .parallel_floor
            .expect("4s test over a 2.25s share is a floor");
        assert_eq!(floor.gate_tests.len(), 1);
    }

    #[test]
    fn balanced_workers_report_low_imbalance() {
        let mut run = Run::default();
        record_test(&mut run, "t.py::a", 0, 10.0);
        record_test(&mut run, "t.py::b", 1, 10.0);
        let pe = analyze(&run, &[], 10.0, 0.0, false, 2, None)
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
        let pe = analyze(&run, &[], 9.0, 0.0, false, 4, None)
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

    /// The fixtures' only test file is `t.py`; everything else is product code.
    fn test_py(file: &str) -> bool {
        file == "t.py"
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
        let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).expect("a waste candidate");
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
        assert!(coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).is_none());
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
        let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).expect("test_b is redundant");
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
        let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py)
            .expect("still redundant on product code");
        let ids: Vec<&str> = cw.tests.iter().map(|t| t.nodeid.as_str()).collect();
        // mod.py:1 is shared, so one of the pair is product-redundant (the tie
        // breaks on nodeid); the other must stay to keep the line covered.
        assert_eq!(ids, vec!["t.py::test_dup"]);
        // covered_lines counts PRODUCT lines only (1), not the test-file body.
        let dup = cw
            .tests
            .iter()
            .find(|t| t.nodeid == "t.py::test_dup")
            .unwrap();
        assert_eq!(dup.covered_lines, 1);
    }

    #[test]
    fn identical_coverers_yield_one_candidate_not_both() {
        // test_a and test_b cover exactly the same lines. Each is individually
        // redundant, but deleting BOTH drops mod.py:1-2, so only the slower one
        // (test_a) is a candidate and only its time is reclaimable.
        let idx = cov(&[(
            "mod.py",
            &[
                (1, &["t.py::test_a", "t.py::test_b"]),
                (2, &["t.py::test_a", "t.py::test_b"]),
            ],
        )]);
        let d = durs(&[("t.py::test_a", 7.0), ("t.py::test_b", 3.0)]);
        let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).expect("one is redundant");
        let ids: Vec<&str> = cw.tests.iter().map(|t| t.nodeid.as_str()).collect();
        assert_eq!(ids, vec!["t.py::test_a"]);
        assert_eq!(cw.redundant_tests, 1);
        assert!((cw.wasted_seconds - 7.0).abs() < 1e-6);
    }

    #[test]
    fn a_nodeid_absent_from_the_run_is_not_a_coverer() {
        // The index says test_gone also covers mod.py:1, but it was deleted
        // since the coverage run (no duration). test_slow is now the sole
        // coverer of that line, so it is not waste.
        let idx = cov(&[("mod.py", &[(1, &["t.py::test_slow", "t.py::test_gone"])])]);
        let d = durs(&[("t.py::test_slow", 9.0)]);
        assert!(coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).is_none());
    }

    #[test]
    fn a_doctest_module_is_product_code_not_a_test_file() {
        // Under --doctest-modules, pkg/mod.py hosts a doctest nodeid. It is
        // still product code: test_slow's only unique line lives there, so it
        // is not waste (a "some nodeid lives here" rule would skip the file and
        // flag test_slow as fully shared).
        let idx = cov(&[
            (
                "pkg/mod.py",
                &[
                    (1, &["t.py::test_slow", "pkg/mod.py::pkg.mod.f"]),
                    (7, &["t.py::test_slow"]),
                ],
            ),
            (
                "pkg/other.py",
                &[(1, &["t.py::test_slow", "t.py::test_keep"])],
            ),
        ]);
        let d = durs(&[
            ("t.py::test_slow", 9.0),
            ("t.py::test_keep", 5.0),
            ("pkg/mod.py::pkg.mod.f", 0.1),
        ]);
        let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py);
        let ids: Vec<String> = cw
            .map(|c| c.tests.into_iter().map(|t| t.nodeid).collect())
            .unwrap_or_default();
        assert!(!ids.contains(&"t.py::test_slow".to_string()), "{ids:?}");
    }

    #[test]
    fn a_skipped_test_is_not_a_coverer() {
        // The index says test_skip also covers mod.py:1, but this run it skipped
        // (a call-phase pytest.skip still has a duration). test_slow is the only
        // test that really covered the line, so it is not waste.
        let mut run = Run::default();
        record_test(&mut run, "tests/test_a.py::test_slow", 0, 9.0);
        for when in ["setup", "call", "teardown"] {
            run.record(
                Some(0),
                crate::scheduling::proto::Report {
                    nodeid: "tests/test_a.py::test_skip".into(),
                    when: when.into(),
                    outcome: if when == "call" { "skipped" } else { "passed" }.into(),
                    duration: if when == "call" { 9.0 } else { 0.0 },
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
        let idx = cov(&[(
            "mod.py",
            &[(
                1,
                &["tests/test_a.py::test_slow", "tests/test_a.py::test_skip"],
            )],
        )]);
        let project = ProjectConfig::default();
        let report = analyze(&run, &[], 9.0, 0.0, false, 1, Some((&idx, &project)));
        assert!(report.coverage_waste.is_none());
        // Control: with both passing, one of the pair IS waste.
        let mut both = Run::default();
        record_test(&mut both, "tests/test_a.py::test_slow", 0, 9.0);
        record_test(&mut both, "tests/test_a.py::test_skip", 0, 9.0);
        assert!(
            analyze(&both, &[], 9.0, 0.0, false, 1, Some((&idx, &project)))
                .coverage_waste
                .is_some()
        );
    }

    #[test]
    fn no_coverage_index_yields_no_section() {
        // analyze without an index never produces a waste section.
        let mut run = Run::default();
        record_test(&mut run, "t.py::a", 0, 5.0);
        assert!(analyze(&run, &[], 5.0, 0.0, false, 1, None)
            .coverage_waste
            .is_none());
    }

    fn fstat(name: &str, count: u64, total: f64, constant: bool, redundant: f64) -> FixtureStat {
        FixtureStat {
            name: name.into(),
            scope: "function".into(),
            count,
            total,
            constant,
            repeated: redundant > 0.0,
            redundant,
            fingerprint: None,
        }
    }

    #[test]
    fn scope_promotion_projects_largest_per_worker_saving() {
        let run = Run::default();
        // Merged stat: the busiest session skipped 1.1s of repeat setup.
        let fixtures = vec![fstat("cfg", 40, 4.0, true, 1.1)];
        let r = analyze(&run, &fixtures, 10.0, 0.0, false, 4, None);
        let e = r.fixtures.iter().find(|f| f.name == "cfg").unwrap();
        assert!(e.constant);
        assert!((e.projected_saving_seconds - 1.1).abs() < 1e-9);
    }

    #[test]
    fn non_constant_and_unrepeated_fixtures_project_no_saving() {
        let run = Run::default();
        let fixtures = vec![
            // Value varied in some session => never a candidate.
            fstat("varies", 40, 4.0, false, 0.0),
            // Never varied, but no session ran it twice (e.g. 5 sessions
            // after a respawn, one call each): no evidence, nothing to save.
            fstat("once_each", 5, 5.0, true, 0.0),
        ];
        let r = analyze(&run, &fixtures, 10.0, 0.0, false, 4, None);
        for f in &r.fixtures {
            assert_eq!(f.projected_saving_seconds, 0.0, "{}", f.name);
            assert!(!f.constant, "{}", f.name);
        }
    }

    #[test]
    fn json_omits_zero_saving_and_false_constant() {
        let r = testutil::report(12);
        let v = serde_json::to_value(&r).unwrap();
        let fixtures = v["fixtures"].as_array().unwrap();
        // `db`: not constant, no saving => both fields skipped.
        let db = &fixtures[0];
        assert_eq!(db["name"], "db");
        assert!(db.get("constant").is_none());
        assert!(db.get("projected_saving_seconds").is_none());
        // `settings`: a candidate => both fields serialized.
        let settings = &fixtures[1];
        assert_eq!(settings["name"], "settings");
        assert_eq!(settings["constant"], true);
        assert_eq!(settings["projected_saving_seconds"], 0.9);
    }
}
