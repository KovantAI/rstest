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
use crate::scheduling::pool::FixtureGroup;
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
    /// `--fork-pool=app`: what the warm template imported. Text report only.
    #[serde(skip)]
    warm: Option<crate::scheduling::warm::Stats>,
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
    /// Modules and classes `--dist load` kept on one worker for their fixture
    /// setup cost. `null` when the run grouped none (lazy collection,
    /// `--shuffle`, `--order fail-fast`, one worker, or no costly scope).
    fixture_grouping: Option<FixtureGrouping>,
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
    /// Tests that leave a wait behind for whichever test runs next on their
    /// worker (the follower's call time carries the lump), from the run
    /// history paired with the replay journals. Empty unless one recurs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    absorbed_waits: Vec<crate::reporting::absorbed::AbsorbedWait>,
    /// Tests that opened outbound network connections (outside loopback and
    /// `[tool.rstest] network-allow`). `null` when none did.
    network: Option<NetworkAccess>,
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
    /// Tests that would need one but already carry a `@pytest.mark.timeout`
    /// at least as high; not in `override_count`.
    marker_covered: usize,
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
    /// Its current `@pytest.mark.timeout`, below the suggestion; absent when
    /// it has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<f64>,
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

/// What fixture-cost-aware dispatch saved: each module or class it kept on
/// one worker, its setups this run, and how many plain chunked dispatch would
/// have caused (an estimate from the chunk size and worker count).
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct FixtureGrouping {
    /// Setup seconds saved over every group, against plain chunked dispatch.
    saved_seconds: f64,
    /// Every group, most saved first.
    groups: Vec<GroupEntry>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct GroupEntry {
    /// The module's or class's nodeid.
    scope: String,
    /// Tests in it this run dispatched.
    tests: usize,
    /// Times its module/class fixtures were set up this run.
    setups: usize,
    /// About how many times plain chunked dispatch would have set them up.
    chunked_setups: usize,
    /// One setup's recorded seconds.
    setup_seconds: f64,
    /// `(chunked_setups - setups) * setup_seconds`, never negative.
    saved_seconds: f64,
}

fn fixture_grouping(run: &Run, groups: &[FixtureGroup]) -> Option<FixtureGrouping> {
    if groups.is_empty() {
        return None;
    }
    let mut entries: Vec<GroupEntry> = groups
        .iter()
        .map(|g| {
            // Measured setups; a scope whose fixtures reported nothing this run
            // (all its tests deselected or skipped) counts its planned pieces.
            let setups = run
                .scope_costs
                .get(&g.scope)
                .map_or(g.pieces, |&(_, n)| n as usize);
            GroupEntry {
                scope: g.scope.clone(),
                tests: g.tests,
                setups,
                chunked_setups: g.chunked_setups,
                setup_seconds: g.setup_seconds,
                saved_seconds: g.chunked_setups.saturating_sub(setups) as f64 * g.setup_seconds,
            }
        })
        .collect();
    entries.sort_by(|a, b| b.saved_seconds.total_cmp(&a.saved_seconds));
    Some(FixtureGrouping {
        saved_seconds: entries.iter().map(|e| e.saved_seconds).sum(),
        groups: entries,
    })
}

/// Tests that reached the network, from the worker's network guard.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct NetworkAccess {
    /// Tests that connected outside loopback and the allowlist.
    tests_with_network: usize,
    /// Those tests, most destinations first (top 20).
    tests: Vec<NetworkTest>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct NetworkTest {
    /// The test.
    nodeid: String,
    /// Destinations it connected to, as `host:port` (with the IP when it was
    /// resolved from a name).
    destinations: Vec<String>,
}

fn network_access(run: &Run) -> Option<NetworkAccess> {
    if run.network.is_empty() {
        return None;
    }
    let mut tests: Vec<NetworkTest> = run
        .network
        .iter()
        .map(|(id, d)| NetworkTest {
            nodeid: id.clone(),
            destinations: d.clone(),
        })
        .collect();
    tests.sort_by(|a, b| {
        b.destinations
            .len()
            .cmp(&a.destinations.len())
            .then_with(|| a.nodeid.cmp(&b.nodeid))
    });
    let tests_with_network = tests.len();
    tests.truncate(20);
    Some(NetworkAccess {
        tests_with_network,
        tests,
    })
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
                .filter(|(_, e)| crate::reporting::history::is_pass(e.outcome_name()))
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
    let mut judged: Vec<(String, f64, usize, u64, Option<f64>)> = run
        .tests()
        .iter()
        .filter_map(|(id, e)| {
            let calls = crate::scheduling::durations::passing_calls(history, id)?;
            let slowest = calls.iter().copied().fold(0.0, f64::max);
            let limit = timeout_for(slowest);
            Some((id.clone(), slowest, calls.len(), limit, e.marker_timeout))
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
    let needing = judged.len();
    // A marker at or above the suggestion already does the job.
    judged.retain(|t| t.4.is_none_or(|m| m < t.3 as f64));
    let marker_covered = needing - judged.len();
    judged.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    let override_count = judged.len();
    judged.truncate(TIMEOUT_OVERRIDES_SHOWN);
    Some(TimeoutAdvice {
        suite_timeout,
        tests_judged,
        override_count,
        marker_covered,
        overrides: judged
            .into_iter()
            .map(
                |(nodeid, slowest, runs, timeout, current)| TimeoutOverride {
                    nodeid,
                    slowest_seconds: (slowest * 1e4).round() / 1e4,
                    runs,
                    timeout,
                    current,
                },
            )
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

    /// Add the `culprits` in this run: tests whose follower absorbs a wait they
    /// leave ([`crate::reporting::absorbed`]).
    pub fn with_absorbed(
        mut self,
        run: &Run,
        culprits: &[crate::reporting::absorbed::AbsorbedWait],
    ) -> Self {
        self.absorbed_waits = culprits
            .iter()
            .filter(|w| run.tests().contains_key(&w.culprit))
            .cloned()
            .collect();
        self
    }

    /// Add what the `--fork-pool=app` template imported.
    pub fn with_warm(mut self, warm: Option<crate::scheduling::warm::Stats>) -> Self {
        self.warm = warm;
        self
    }

    /// Add what fixture-cost-aware dispatch grouped ([`FixtureGrouping`]).
    pub fn with_fixture_groups(mut self, run: &Run, groups: &[FixtureGroup]) -> Self {
        self.fixture_grouping = fixture_grouping(run, groups);
        self
    }
}

/// A test that ended with more threads / open fds than it started: a resource
/// it opened and never released (its own teardown included). Or a class,
/// module or package fixture whose resources outlived its scope end in at
/// least two of its setups (`scope` set).
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Leak {
    /// The test that leaked, or the fixture (`baseid::name`) when `scope` is
    /// set.
    pub nodeid: String,
    /// The fixture's scope (`class`, `module`, `package`); absent for a test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Threads the test created that outlived its teardown (0 if only fds
    /// leaked).
    pub threads: i64,
    /// Fds the test opened that are still open after its teardown (0 if only
    /// threads leaked).
    pub fds: i64,
}

impl Leak {
    /// The nodeid, or `[module fixture] tests::server` for a fixture leak.
    pub fn label(&self) -> String {
        match &self.scope {
            Some(scope) => format!("[{scope} fixture] {}", self.nodeid),
            None => self.nodeid.clone(),
        }
    }
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

    // Wait: test time not spent on cpu, as seconds and a share of test time.
    let measured_wait = (n_cpu > 0).then(|| {
        let wait = (test_time - cpu_time).max(0.0);
        (wait, 100.0 * wait / test_time.max(f64::EPSILON))
    });
    let wait_bound = wait_bound(&durations, measured_wait);

    // Slowest first, for the long pole and the parallel floor.
    durations.sort_by(|a, b| b.1.total_cmp(&a.1));
    let long_pole = durations.first().map(|(_, d, _)| *d);
    let parallel_floor = parallel_floor(&durations, test_time, workers);

    let parallel_efficiency =
        parallel_efficiency(run, workers, test_time, wall, long_pole.unwrap_or(0.0));

    let fx = fixture_entries(fixtures);
    let files = slowest_files(&durations, test_time);
    let coverage_waste = coverage_waste_of(run, &durations, coverage);

    let leaks = detect_leaks(run);

    DoctorReport {
        schema: SCHEMA_VERSION,
        rstest_version: env!("CARGO_PKG_VERSION"),
        workers,
        wall_seconds: wall,
        startup_seconds: startup,
        fork_prewarm,
        warm: None,
        tests: durations.len(),
        test_time_seconds: test_time,
        cpu_time_seconds: cpu_time,
        wait_bound,
        measured_wait,
        long_pole_seconds: long_pole,
        parallel_floor,
        parallel_efficiency,
        fixtures: fx,
        fixture_grouping: None,
        slowest_files: files,
        coverage_waste,
        timeouts: None,
        drift: None,
        absorbed_waits: Vec::new(),
        network: network_access(run),
        leaks,
    }
}

/// Wait-bound: a run whose test time is mostly waiting (wall vs cpu), with
/// the tests that wait the most.
fn wait_bound(
    durations: &[(&String, f64, Option<f64>)],
    measured_wait: Option<(f64, f64)>,
) -> Option<WaitBound> {
    if let Some((wait, pct)) = measured_wait {
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
    }
}

/// Parallel floor: the longest tests when one outlasts a worker's even share.
/// `durations` is sorted slowest first.
fn parallel_floor(
    durations: &[(&String, f64, Option<f64>)],
    test_time: f64,
    workers: usize,
) -> Option<ParallelFloor> {
    durations.first().and_then(|&(_, longest, _)| {
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
    })
}

/// Parallel efficiency: realized speedup and worker load balance.
/// Multi-worker only. Groups already-collected per-test durations by
/// recorded worker to expose load imbalance without new timeline data.
fn parallel_efficiency(
    run: &Run,
    workers: usize,
    test_time: f64,
    wall: f64,
    long_pole: f64,
) -> Option<ParallelEfficiency> {
    let tests = run.tests();
    (workers > 1 && test_time > 0.0).then(|| {
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
            long_pole_seconds: long_pole,
        }
    })
}

/// The fixture rows: the 50 costliest fixtures, with their promotion saving.
fn fixture_entries(fixtures: &[FixtureStat]) -> Vec<FixtureEntry> {
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
    fx
}

/// The 20 slowest test files by summed protocol time.
fn slowest_files(durations: &[(&String, f64, Option<f64>)], test_time: f64) -> Vec<FileEntry> {
    let mut by_file: BTreeMap<&str, f64> = BTreeMap::new();
    for (id, d, _) in durations {
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
    files
}

/// Coverage waste: slow tests adding no unique coverage.
/// `coverage` is this run's own per-test index (see run_post_gates), so its
/// nodeids and line numbers describe the code as it is now. Only PASSED tests
/// count: a skipped or failed test's call time says nothing about what it
/// covers, and it is no stand-in for a test that did run.
fn coverage_waste_of(
    run: &Run,
    durations: &[(&String, f64, Option<f64>)],
    coverage: Option<(&CoverageIndex, &ProjectConfig)>,
) -> Option<CoverageWaste> {
    coverage.and_then(|(index, project)| {
        let passed = run.green_nodeids();
        let duration_of: HashMap<&str, f64> = durations
            .iter()
            .filter(|(id, _, _)| passed.contains(id.as_str()))
            .map(|(id, d, _)| (id.as_str(), *d))
            .collect();
        coverage_waste(&duration_of, index, WASTE_MIN_SECONDS, |file| {
            crate::collect::is_test_file(std::path::Path::new(file), project)
        })
    })
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
/// teardown) and wide fixtures that leaked past their scope end, worst first.
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
                scope: None,
                threads,
                fds,
            })
        })
        .collect();
    leaks.extend(
        run.fixture_leaks
            .iter()
            .map(|((fixture, scope), &(threads, fds))| Leak {
                nodeid: fixture.clone(),
                scope: Some(scope.clone()),
                threads,
                fds,
            }),
    );
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
mod tests;
