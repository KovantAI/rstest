//! Per-test duration cache: `.rstest_cache/durations.json` in the cwd.
//! Drives long-pole-first scheduling; absent or stale entries are harmless,
//! unknown tests just keep collection order.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::reporting::report::Run;

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
    fn long_poles_first_longest_first() {
        let ids: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        let mut cache = HashMap::new();
        cache.insert("t1".to_string(), 2.0);
        cache.insert("t3".to_string(), 9.0);
        cache.insert("t0".to_string(), 0.2); // under threshold: stays put
        assert_eq!(dispatch_order(&ids, &cache), vec![3, 1, 0, 2, 4]);
    }
}
