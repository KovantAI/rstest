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
const SCHEMA_VERSION: u32 = 1;

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
        fixtures: fx,
        slowest_files: files,
    }
}

pub fn write_json(path: &std::path::Path, report: &DoctorReport) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
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
