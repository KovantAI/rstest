//! Per-test duration cache: `.rstest_cache/durations.json` in the cwd.
//!
//! Drives long-pole-first scheduling (research: aiohttp's 55s test must
//! start first or it floors the whole run). Written after every run from
//! the reports we already stream; absent or stale entries are harmless —
//! unknown tests just keep collection order.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::report::Run;

fn cache_path() -> PathBuf {
    PathBuf::from(".rstest_cache/durations.json")
}

pub fn load() -> HashMap<String, f64> {
    std::fs::read(cache_path())
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
    let _ = std::fs::create_dir_all(".rstest_cache");
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = std::fs::write(cache_path(), bytes);
    }
}

/// Items with a cached duration above this run first, longest first.
pub const SLOW_THRESHOLD_SECS: f64 = 1.0;

/// --durations-regress rows: (nodeid, baseline seconds, current seconds),
/// worst absolute growth first. A test regresses when its wall time grew
/// past `ratio` × baseline AND the growth clears a jitter floor: the
/// baseline itself is at least 50ms (ratio on micro-tests is noise) and
/// the absolute growth is at least half a second. Tests absent from the
/// baseline (new/renamed) never flag.
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
                crate::proto::Report {
                    nodeid: id.into(),
                    when: "call".into(),
                    outcome: "passed".into(),
                    duration: d,
                    longrepr: None,
                    wasxfail: false,
                    skip_reason: None,
                    cpu: None,
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

    // Build a Run whose call durations are exactly `timings`.
    fn run_with(timings: &[(&str, f64)]) -> Run {
        let mut run = Run::default();
        for &(id, d) in timings {
            run.record(
                None,
                crate::proto::Report {
                    nodeid: id.into(),
                    when: "call".into(),
                    outcome: "passed".into(),
                    duration: d,
                    longrepr: None,
                    wasxfail: false,
                    skip_reason: None,
                    cpu: None,
                    sections: Vec::new(),
                    lineno: None,
                },
            );
        }
        run
    }

    #[test]
    fn regression_threshold_multiplies_baseline_not_divides() {
        // old=1.0, ratio=3.0 -> the bar is old*ratio = 3.0s. A 2.0s current is
        // UNDER the bar, so nothing flags. If the operator were `/` the bar
        // collapses to 0.33s and this would wrongly flag.
        let run = run_with(&[("t::a", 2.0)]);
        let base = HashMap::from([("t::a".to_string(), 1.0)]);
        assert!(
            regressions(&run, &base, 3.0).is_empty(),
            "current below old*ratio must not flag"
        );
    }

    #[test]
    fn regressions_sorted_by_absolute_growth_desc() {
        // A grew 1.0s (0.1->1.1), B grew 0.8s (5.0->5.8). Worst ABSOLUTE growth
        // first => A before B. Sorting by sum (b.2+b.1) would put B first (10.8
        // vs 1.2), so this pins the subtraction against the `+` mutant.
        let run = run_with(&[("t::a", 1.1), ("t::b", 5.8)]);
        let base = HashMap::from([("t::a".to_string(), 0.1), ("t::b".to_string(), 5.0)]);
        let rows = regressions(&run, &base, 1.0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "t::a", "larger absolute growth ranks first");
        assert_eq!(rows[1].0, "t::b");
    }

    #[test]
    fn regressions_ranked_by_growth_not_ratio() {
        // A grew 1.0s at 1.1x (10.0->11.0); B grew 0.8s at 9x (0.1->0.9). By
        // absolute growth A wins; a `/`-mutated comparator would rank by ratio
        // and put B first. Distinguishes subtraction from division.
        let run = run_with(&[("t::a", 11.0), ("t::b", 0.9)]);
        let base = HashMap::from([("t::a".to_string(), 10.0), ("t::b".to_string(), 0.1)]);
        let rows = regressions(&run, &base, 1.0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "t::a", "growth, not ratio, decides the order");
    }

    #[test]
    fn under_threshold_cached_test_is_not_a_long_pole() {
        // t1 is cached but 0.3s < SLOW_THRESHOLD (1.0s), so it is NOT promoted
        // ahead of the uncached t0 — collection order holds. A mutated guard of
        // `true` would treat every cached test as slow and yield [1, 0].
        let ids = vec!["t0".to_string(), "t1".to_string()];
        let cache = HashMap::from([("t1".to_string(), 0.3)]);
        assert_eq!(dispatch_order(&ids, &cache), vec![0, 1]);
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
