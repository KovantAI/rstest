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

/// Seconds a flake/failure record stays relevant. A test with no event inside
/// this window reads as fixed and its entry is dropped, so long-green tests
/// stop carrying "flaked Nx before" annotations. Override with
/// `RSTEST_FLAKE_RETENTION_DAYS`; `0` disables aging (keep forever).
fn retention_secs() -> u64 {
    parse_retention(std::env::var("RSTEST_FLAKE_RETENTION_DAYS").ok())
}

/// Pure retention parse: days string -> seconds, defaulting to 90 days when
/// unset or unparseable. Split out so the policy is testable without env.
fn parse_retention(raw: Option<String>) -> u64 {
    const DEFAULT_DAYS: u64 = 90;
    let days = raw
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DAYS);
    days.saturating_mul(24 * 60 * 60)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Drop entries whose last event predates the retention window. Pure so the
/// aging policy is testable without a clock. `max_age == 0` disables aging.
/// `saturating_sub` means a zeroed/future `last_epoch` (bad clock, skew) is
/// retained, never wiped.
fn retain_recent(log: &mut HashMap<String, FlakeStats>, now: u64, max_age: u64) {
    if max_age == 0 {
        return;
    }
    log.retain(|_, e| now.saturating_sub(e.last_epoch) <= max_age);
}

pub fn load() -> HashMap<String, FlakeStats> {
    let mut log: HashMap<String, FlakeStats> = std::fs::read(path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    retain_recent(&mut log, now(), retention_secs());
    log
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
    let now = now();
    // load() has already dropped entries past the retention window, so writing
    // the merged map back garbage-collects the file on any event-bearing run.
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

    fn entry(last_epoch: u64) -> FlakeStats {
        FlakeStats {
            flaky: 1,
            failed: 0,
            last_epoch,
        }
    }

    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn drops_entries_past_window_keeps_recent() {
        let now = 100 * DAY;
        let mut log = HashMap::from([
            ("stale".to_string(), entry(now - 91 * DAY)), // older than 90d
            ("edge".to_string(), entry(now - 90 * DAY)),  // exactly at window
            ("fresh".to_string(), entry(now - DAY)),
        ]);
        retain_recent(&mut log, now, 90 * DAY);
        assert!(!log.contains_key("stale"), "past-window entry must drop");
        assert!(log.contains_key("edge"), "at-window entry must stay");
        assert!(log.contains_key("fresh"));
    }

    #[test]
    fn max_age_zero_disables_aging() {
        let now = 1_000 * DAY;
        let mut log = HashMap::from([("ancient".to_string(), entry(0))]);
        retain_recent(&mut log, now, 0);
        assert!(log.contains_key("ancient"), "0 window keeps everything");
    }

    #[test]
    fn bad_clock_or_future_epoch_is_retained_not_wiped() {
        // now behind the recorded epoch (skew) -> saturating_sub == 0 -> keep.
        let mut log = HashMap::from([("future".to_string(), entry(500 * DAY))]);
        retain_recent(&mut log, 100 * DAY, 90 * DAY);
        assert!(log.contains_key("future"), "future epoch must not be wiped");
    }

    #[test]
    fn retention_parse_override_and_defaults() {
        assert_eq!(parse_retention(Some("7".into())), 7 * DAY);
        assert_eq!(parse_retention(Some("0".into())), 0, "0 -> disabled");
        assert_eq!(parse_retention(None), 90 * DAY, "unset -> 90d default");
        assert_eq!(
            parse_retention(Some("garbage".into())),
            90 * DAY,
            "unparseable -> default"
        );
    }
}
