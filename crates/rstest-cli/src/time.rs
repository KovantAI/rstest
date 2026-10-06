//! Wall-clock epoch helpers. `SystemTime::now().duration_since(UNIX_EPOCH)`
//! only errors if the clock is before 1970; every caller treats that as 0, so
//! centralize the pattern rather than repeat the `.map(..).unwrap_or(0)` dance.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch (0 if the clock predates it).
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Nanoseconds since the Unix epoch (0 if the clock predates it). Used where a
/// high-resolution, monotonic-enough token is wanted (run uids, tmp names,
/// shuffle seeds).
pub fn now_epoch_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
