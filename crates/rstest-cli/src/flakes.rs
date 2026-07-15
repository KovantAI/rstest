//! Cross-run flake history: `.rstest_cache/flakes.json` in the cwd.
//!
//! `--reruns` detects a flake and forgets it when the run ends; this log
//! is the memory. Only tests that ever flaked or failed get an entry
//! (sparse — a green suite writes nothing), so the file stays small at
//! any suite size. The data feeds the flaky-section history annotation
//! and gives teams a ranked candidate list for `--quarantine`.

use std::collections::HashMap;
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
