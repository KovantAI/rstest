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
    fn long_poles_first_longest_first() {
        let ids: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        let mut cache = HashMap::new();
        cache.insert("t1".to_string(), 2.0);
        cache.insert("t3".to_string(), 9.0);
        cache.insert("t0".to_string(), 0.2); // under threshold: stays put
        assert_eq!(dispatch_order(&ids, &cache), vec![3, 1, 0, 2, 4]);
    }
}
