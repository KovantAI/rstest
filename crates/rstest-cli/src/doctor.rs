//! `rstest --doctor`: why is this suite slow?
//!
//! Research finding behind this feature: in every suite profiled, the
//! biggest speedup was suite CONTENT visible in runner-owned timing data —
//! sleep-bound tests (rich: 74% of call time), wait-bound IO/timeouts
//! (aiohttp: 78%), and repeated expensive fixtures (allauth: one key
//! parsed 206×). No runner surfaces this; the data is already streaming.
//!
//! One analysis pass feeds two outputs: the terminal report and a
//! versioned JSON document (`--doctor-json`) for CI trending.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::proto::FixtureStat;
use crate::report::Run;

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

/// Realized parallel speedup measured from an actual run: how much of the
/// worker budget the run actually converted into wall-clock savings, plus
/// the per-worker load balance that caps it. Unlike `ParallelFloor` (a
/// static pre-run estimate), this is the after-the-fact answer to "why
/// isn't `-n auto` faster?". Only emitted for multi-worker pool runs.
#[derive(Serialize)]
struct ParallelEfficiency {
    /// test_time / wall. May exceed `ideal_speedup` for wait-bound suites,
    /// where overlapping sleeps/IO run more tests at once than there are
    /// cores.
    realized_speedup: f64,
    /// Worker count (`-n`) — the ceiling for a purely CPU-bound suite.
    ideal_speedup: usize,
    /// 100 * realized / ideal. >100% signals wait-bound overlap.
    efficiency_pct: f64,
    /// Busy time summed per worker, descending — the load-balance picture.
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
    // Only meaningful for multi-worker pool runs. Groups the per-test
    // durations already collected by their recorded worker to expose load
    // imbalance without needing any new timeline data from the workers.
    let parallel_efficiency = (workers > 1 && test_time > 0.0).then(|| {
        let mut by_worker: BTreeMap<&str, (f64, usize)> = BTreeMap::new();
        for (_, e) in tests.iter() {
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
        // Workers that ran no test are absent from `by_worker` but still part
        // of the pool: their busy time is 0. Treating min as the smallest
        // *observed* load hides the worst imbalance (all work on one worker
        // reads as 0% instead of ~100%). Seed idle workers at 0.
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
    }
}

pub fn write_json(path: &std::path::Path, report: &DoctorReport) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
}

/// The doctor analysis as GitHub-flavored markdown, shaped for a job
/// summary: same signals as the terminal report, tables instead of
/// aligned columns.
pub fn render_markdown(r: &DoctorReport) -> String {
    use std::fmt::Write;

    let mut md = String::from("## rstest doctor\n\n");
    if r.tests == 0 {
        md.push_str("No timing data collected.\n");
        return md;
    }
    let _ = writeln!(
        md,
        "**{} tests** — test time {:.1}s (wall {:.1}s, {} workers)\n",
        r.tests, r.test_time_seconds, r.wall_seconds, r.workers
    );

    if let Some(w) = &r.wait_bound {
        let _ = writeln!(
            md,
            "**Wait-bound:** {:.0}% of test time ({:.1}s) is waiting, not \
             computing (sleeps / IO / timeouts).\n",
            w.wait_pct, w.wait_seconds
        );
        if !w.tests.is_empty() {
            md.push_str("| Waiting | Duration | Test |\n|---:|---:|---|\n");
            for t in w.tests.iter().take(8) {
                let _ = writeln!(
                    md,
                    "| {:.2}s | {:.2}s | `{}` |",
                    t.wait, t.duration, t.nodeid
                );
            }
            if w.tests.len() > 8 {
                let _ = writeln!(md, "\n... and {} more", w.tests.len() - 8);
            }
            md.push('\n');
        }
    }

    if let Some(p) = &r.parallel_floor {
        let _ = writeln!(
            md,
            "**Parallel floor:** the longest test ({:.1}s) exceeds the ideal \
             per-worker share ({:.1}s at `-n {}`); no worker count can finish \
             faster than its longest test.\n",
            p.longest_seconds, p.ideal_share_seconds, r.workers
        );
        if !p.gate_tests.is_empty() {
            md.push_str("| Duration | Gate test |\n|---:|---|\n");
            for t in p.gate_tests.iter().take(5) {
                let _ = writeln!(md, "| {:.2}s | `{}` |", t.duration, t.nodeid);
            }
            md.push('\n');
        }
    }

    if let Some(pe) = &r.parallel_efficiency {
        let _ = writeln!(
            md,
            "**Parallel efficiency:** {:.1}× realized of {}× possible ({:.0}%). \
             Long pole {:.1}s; {:.0}% load imbalance between busiest and idlest \
             worker.\n",
            pe.realized_speedup,
            pe.ideal_speedup,
            pe.efficiency_pct,
            pe.long_pole_seconds,
            pe.imbalance_pct
        );
        if pe.efficiency_pct > 105.0 {
            md.push_str("> Over 100% means tests overlap beyond core count (wait-bound).\n\n");
        }
        if !pe.workers_busy.is_empty() {
            md.push_str("| Worker | Busy | Tests |\n|---|---:|---:|\n");
            for w in pe.workers_busy.iter().take(8) {
                let _ = writeln!(
                    md,
                    "| `{}` | {:.2}s | {} |",
                    w.worker, w.busy_seconds, w.tests
                );
            }
            md.push('\n');
        }
    }

    let interesting: Vec<&FixtureEntry> = r
        .fixtures
        .iter()
        .filter(|f| f.total_seconds >= 0.5)
        .take(8)
        .collect();
    if !interesting.is_empty() {
        md.push_str("### Fixture hotspots (setup time across all workers)\n\n");
        md.push_str("| Fixture | Scope | Runs | Total | |\n|---|---|---:|---:|---|\n");
        for f in interesting {
            let advice = if f.scope == "function" && f.count >= 20 && f.total_seconds >= 1.0 {
                "ran many times; widen scope if value is reusable"
            } else if f.scope == "session" && f.count > 1 {
                "session fixture ran once per worker; must be safe to duplicate"
            } else {
                ""
            };
            let _ = writeln!(
                md,
                "| `{}` | {} | {} | {:.1}s | {advice} |",
                f.name, f.scope, f.count, f.total_seconds
            );
        }
        md.push('\n');
    }

    if !r.slowest_files.is_empty() {
        md.push_str("### Slowest files\n\n| File | Time | Share |\n|---|---:|---:|\n");
        for f in r.slowest_files.iter().take(5) {
            let _ = writeln!(
                md,
                "| `{}` | {:.2}s | {:.0}% |",
                f.file, f.total_seconds, f.pct
            );
        }
    }
    md
}

pub fn write_markdown(path: &std::path::Path, report: &DoctorReport) -> anyhow::Result<()> {
    std::fs::write(path, render_markdown(report))?;
    Ok(())
}

/// Publish the markdown report to the CI's job-summary surface, if any —
/// zero-config: any doctor run on a supported runner shows up on the run
/// page. GitHub Actions appends to `$GITHUB_STEP_SUMMARY`; Buildkite pipes
/// it to `buildkite-agent annotate`. (GitLab and TeamCity have no native
/// markdown job-summary surface — use `--doctor-md` and publish the file
/// as an artifact there.)
pub fn append_ci_summary(report: &DoctorReport) -> anyhow::Result<()> {
    // GitHub Actions: append to the step-summary file (hard error on write
    // failure — the path came from the runner, so a failure is real).
    if let Some(path) = std::env::var("GITHUB_STEP_SUMMARY")
        .ok()
        .filter(|p| !p.is_empty())
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;
        f.write_all(render_markdown(report).as_bytes())?;
        return Ok(());
    }
    // Buildkite: pipe the markdown to the agent as an info annotation.
    // Best-effort — a missing/failing agent must not fail the test run
    // (the annotation is cosmetic, unlike GitHub's guaranteed file path).
    if std::env::var("BUILDKITE")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some()
    {
        buildkite_annotate(&render_markdown(report));
    }
    Ok(())
}

/// Feed markdown to `buildkite-agent annotate` over stdin. Swallows all
/// errors (logging to stderr) — see `append_ci_summary`.
fn buildkite_annotate(md: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let child = Command::new("buildkite-agent")
        .args(["annotate", "--style", "info", "--context", "rstest-doctor"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rstest: skipping Buildkite annotation (buildkite-agent: {e})");
            return;
        }
    };
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        let _ = stdin.write_all(md.as_bytes());
    }
    if let Err(e) = child.wait() {
        eprintln!("rstest: buildkite-agent annotate failed: {e}");
    }
}

pub fn render(r: &DoctorReport) {
    if r.tests == 0 {
        println!("\n== rstest doctor: no timing data collected ==");
        return;
    }
    println!("\n================== rstest doctor ==================");
    println!(
        "{} tests, {:.1}s test time (wall {:.1}s, {} workers)",
        r.tests, r.test_time_seconds, r.wall_seconds, r.workers
    );

    if let Some(w) = &r.wait_bound {
        println!(
            "\nWAIT-BOUND: {:.0}% of test time ({:.1}s) is waiting, \
             not computing (sleeps / IO / timeouts).",
            w.wait_pct, w.wait_seconds
        );
        for t in w.tests.iter().take(8) {
            println!(
                "  {:7.2}s waiting of {:7.2}s  {}",
                t.wait, t.duration, t.nodeid
            );
        }
        if w.tests.len() > 8 {
            println!("  ... and {} more", w.tests.len() - 8);
        }
    }

    if let Some(p) = &r.parallel_floor {
        println!(
            "\nPARALLEL FLOOR: the longest test ({:.1}s) exceeds the ideal \
             per-worker share ({:.1}s at -n {});\nno worker count can finish \
             faster than its longest test. Gate tests:",
            p.longest_seconds, p.ideal_share_seconds, r.workers
        );
        for t in p.gate_tests.iter().take(5) {
            println!("  {:7.2}s  {}", t.duration, t.nodeid);
        }
    }

    if let Some(pe) = &r.parallel_efficiency {
        println!(
            "\nPARALLEL EFFICIENCY: {:.1}x realized of {}x possible ({:.0}%).",
            pe.realized_speedup, pe.ideal_speedup, pe.efficiency_pct
        );
        if pe.efficiency_pct > 105.0 {
            println!(
                "  over 100%: tests overlap beyond core count \
                 (wait-bound; see WAIT-BOUND above)."
            );
        }
        println!(
            "  long pole: {:.1}s (no worker count finishes faster)",
            pe.long_pole_seconds
        );
        println!("  worker load (busy time):");
        for w in pe.workers_busy.iter().take(8) {
            println!(
                "    {:<8} {:7.2}s ({} tests)",
                w.worker, w.busy_seconds, w.tests
            );
        }
        if pe.workers_busy.len() > 8 {
            println!("    ... and {} more", pe.workers_busy.len() - 8);
        }
        println!(
            "  imbalance: {:.0}% between busiest and idlest worker",
            pe.imbalance_pct
        );
    }

    let interesting: Vec<&FixtureEntry> = r
        .fixtures
        .iter()
        .filter(|f| f.total_seconds >= 0.5)
        .take(8)
        .collect();
    if !interesting.is_empty() {
        println!("\nFIXTURE HOTSPOTS (setup time across all workers):");
        for f in interesting {
            let advice = if f.scope == "function" && f.count >= 20 && f.total_seconds >= 1.0 {
                "  <- ran many times; widen scope if value is reusable"
            } else if f.scope == "session" && f.count > 1 {
                "  <- session fixture ran once PER WORKER; must be safe to duplicate (DBs, servers, ports)"
            } else {
                ""
            };
            println!(
                "  {:7.2}s {:6}x  scope={:<8} {}{advice}",
                f.total_seconds, f.count, f.scope, f.name
            );
        }
    }

    println!("\nSLOWEST FILES:");
    for f in r.slowest_files.iter().take(5) {
        println!("  {:7.2}s ({:4.1}%)  {}", f.total_seconds, f.pct, f.file);
    }
    println!("===================================================");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(tests: usize) -> DoctorReport {
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
        }
    }

    #[test]
    fn markdown_renders_all_sections() {
        let md = render_markdown(&report(12));
        assert!(md.starts_with("## rstest doctor\n"));
        assert!(md.contains("**12 tests** — test time 30.0s (wall 9.0s, 4 workers)"));
        assert!(md.contains("**Wait-bound:** 80% of test time (24.0s)"));
        assert!(md.contains("| 5.00s | 5.10s | `tests/test_a.py::test_sleepy` |"));
        assert!(md.contains("**Parallel floor:**"));
        assert!(md.contains("| 8.40s | `tests/test_a.py::test_long` |"));
        assert!(md.contains("**Parallel efficiency:** 3.3× realized of 4× possible (82%)"));
        assert!(md.contains("| `gw0` | 16.00s | 6 |"));
        assert!(md.contains("### Fixture hotspots"));
        assert!(md.contains("| `db` | session | 4 | 6.1s | session fixture ran once per worker"));
        assert!(md.contains("### Slowest files"));
        assert!(md.contains("| `tests/test_a.py` | 20.00s | 67% |"));
    }

    #[test]
    fn markdown_empty_run() {
        let md = render_markdown(&report(0));
        assert!(md.contains("No timing data collected."));
        assert!(!md.contains("Wait-bound"));
    }

    /// Record one completed test (setup/call/teardown) on `worker` with the
    /// given call duration, mirroring what the pool feeds `Run::record`.
    fn record_test(run: &mut Run, nodeid: &str, worker: usize, dur: f64) {
        let r = |when: &str, duration: f64| crate::proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: "passed".into(),
            duration,
            longrepr: None,
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            sections: Vec::new(),
            lineno: None,
        };
        run.record(Some(worker), r("setup", 0.0));
        run.record(Some(worker), r("call", dur));
        run.record(Some(worker), r("teardown", 0.0));
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
