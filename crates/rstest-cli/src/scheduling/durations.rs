//! Per-test duration cache: `.rstest_cache/durations.json` in the cwd.
//! Drives long-pole-first scheduling; absent or stale entries are harmless,
//! unknown tests just keep collection order.

use std::collections::HashMap;
use std::path::Path;

use crate::cache;
use crate::reporting::report::Run;

pub const FILE: &str = "durations.json";

/// Per-project wall-clock cache: `.rstest_cache/wall.json`, a single float of
/// seconds. Unlike `durations.json` (call phase only), this captures the whole
/// suite's elapsed time — fixture setup/teardown included — so the monorepo
/// planner can weight a fixture-bound project by its real cost rather than its
/// near-zero call time. See `mono::project_cost`.
pub const WALL_FILE: &str = "wall.json";

/// Record this run's total wall time for the cwd project.
pub fn save_wall(secs: f64) {
    if let Ok(bytes) = serde_json::to_vec(&secs) {
        let _ = cache::write_atomic(&cache::file(WALL_FILE), &bytes);
    }
}

/// Last run's wall seconds for `project`, if recorded.
pub fn load_wall_in(project: &Path) -> Option<f64> {
    let bytes = std::fs::read(cache::file_in(project, WALL_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn load() -> HashMap<String, f64> {
    std::fs::read(cache::file(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(run: &Run) {
    // Merge over previous cache: tests not in this run keep old timings
    // (-k/-m filtered runs must not wipe the rest of the suite's data).
    let mut cache = load();
    for (id, d) in run.durations() {
        cache.insert(id.clone(), d);
    }
    if cache.is_empty() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = cache::write_atomic(&cache::file(FILE), &bytes);
    }
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

/// Fail-fast dispatch order: surface a red as early as possible. Sort by
/// hard-failure count (desc), then flake count (desc) — the tests most likely
/// to fail go first — then ascending duration so, among equally-suspect (and
/// among all-clean) tests, the fastest run first and slow-stable tests land
/// last. Duration stays a secondary key, so workers still fill. Stable: equal
/// keys keep collection order. Pairs with `--maxfail`/`-x` for true early exit.
pub fn failfast_order(
    ids: &[String],
    cache: &HashMap<String, f64>,
    flakes: &HashMap<String, crate::reporting::flakes::FlakeStats>,
) -> Vec<u64> {
    let mut order: Vec<u64> = (0..ids.len() as u64).collect();
    order.sort_by(|&a, &b| {
        let (ia, ib) = (&ids[a as usize], &ids[b as usize]);
        let fa = flakes.get(ia).copied().unwrap_or_default();
        let fb = flakes.get(ib).copied().unwrap_or_default();
        let (da, db) = (
            cache.get(ia).copied().unwrap_or(0.0),
            cache.get(ib).copied().unwrap_or(0.0),
        );
        fb.failed
            .cmp(&fa.failed)
            .then(fb.flaky.cmp(&fa.flaky))
            .then(da.total_cmp(&db))
    });
    order
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
    fn failfast_orders_failed_then_flaky_then_fast_then_slow() {
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
        // hard-failed first, then flaky, then the two clean tests fast-before-slow.
        assert_eq!(failfast_order(&names, &cache, &flakes), vec![0, 1, 2, 3]);
    }

    #[test]
    fn failfast_no_signal_is_ascending_duration_stable() {
        use crate::reporting::flakes::FlakeStats;
        let names: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        // b slow, a fast, c unknown (0.0). Ascending duration: c(0) , a(0.5), b(3).
        let cache = HashMap::from([("a".to_string(), 0.5), ("b".to_string(), 3.0)]);
        let flakes: HashMap<String, FlakeStats> = HashMap::new();
        assert_eq!(failfast_order(&names, &cache, &flakes), vec![2, 0, 1]);
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
}
