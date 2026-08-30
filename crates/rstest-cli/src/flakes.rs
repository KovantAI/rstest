//! Cross-run flake history: `.rstest_cache/flakes.json` in the cwd.
//!
//! `--reruns` detects a flake and forgets it when the run ends; this log
//! is the memory. Only tests that ever flaked or failed get an entry
//! (sparse — a green suite writes nothing), so the file stays small at
//! any suite size. The data feeds the flaky-section history annotation
//! and gives teams a ranked candidate list for `--quarantine`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::report::Run;

#[derive(Default, Clone, Copy, Serialize, Deserialize)]
pub struct FlakeStats {
    /// Runs where the test passed only after rerun(s).
    #[serde(default)]
    pub flaky: u32,
    /// Runs where the test hard-failed (quarantined failures included).
    #[serde(default)]
    pub failed: u32,
    /// Unix epoch of the last recorded event.
    #[serde(default)]
    pub last_epoch: u64,
}

fn path() -> PathBuf {
    PathBuf::from(".rstest_cache/flakes.json")
}

pub fn load() -> HashMap<String, FlakeStats> {
    std::fs::read(path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Node ids with a recorded *flaky* history (`flaky > 0`) — tests that have
/// passed-after-rerun on some past run. This is the candidate set for
/// `--reruns-only-known-flaky`: a hard-failure-only history (`failed > 0`,
/// `flaky == 0`) is deliberately excluded, so a deterministic mass-failure
/// (one root cause failing many tests identically, recorded as `failed`) is
/// never treated as known-flaky and does not consume the rerun budget.
pub fn known_flaky() -> HashSet<String> {
    filter_known_flaky(load())
}

fn filter_known_flaky(log: HashMap<String, FlakeStats>) -> HashSet<String> {
    log.into_iter()
        .filter(|(_, s)| s.flaky > 0)
        .map(|(id, _)| id)
        .collect()
}

/// Merge this run's flake/failure events over the stored history.
/// Best-effort like the duration cache: IO errors are ignored.
pub fn record(run: &Run) {
    let mut events: Vec<(&String, bool)> = Vec::new();
    for (nodeid, _) in &run.flaky {
        events.push((nodeid, true));
    }
    for nodeid in run.failed_nodeids() {
        events.push((nodeid, false));
    }
    if events.is_empty() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut log = load();
    for (nodeid, was_flaky) in events {
        let e = log.entry(nodeid.clone()).or_default();
        if was_flaky {
            e.flaky += 1;
        } else {
            e.failed += 1;
        }
        e.last_epoch = now;
    }
    let _ = std::fs::create_dir_all(".rstest_cache");
    if let Ok(bytes) = serde_json::to_vec(&log) {
        let _ = std::fs::write(path(), bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_flaky_keys_on_flaky_not_failed() {
        let mut log = HashMap::new();
        log.insert(
            "a::flaked".into(),
            FlakeStats {
                flaky: 2,
                failed: 0,
                last_epoch: 1,
            },
        );
        // Hard-failure-only history must NOT count as known-flaky — this is
        // what keeps a deterministic mass-failure from burning the budget.
        log.insert(
            "b::failed_only".into(),
            FlakeStats {
                flaky: 0,
                failed: 9,
                last_epoch: 1,
            },
        );
        log.insert(
            "c::both".into(),
            FlakeStats {
                flaky: 1,
                failed: 3,
                last_epoch: 1,
            },
        );
        let set = filter_known_flaky(log);
        assert!(set.contains("a::flaked"));
        assert!(set.contains("c::both"));
        assert!(!set.contains("b::failed_only"));
        assert_eq!(set.len(), 2);
    }
}
