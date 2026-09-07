//! Concerns shared by the eager (`pool::run_pool`) and lazy
//! (`lazy::run_lazy_pool`) orchestrator loops. Both loops keep their own
//! (divergent) dispatch model, but the crash/rerun/watchdog/wind-down/exit
//! paths are identical modulo the item-id type, so they live here as pure,
//! unit-testable helpers instead of two hand-kept copies.

use std::time::{Duration, Instant};

use crate::reporting::progress::Progress;
use crate::reporting::report::Run;
use crate::scheduling::proto;

/// The per-worker behavior the shared loop mechanics touch. Each loop's own
/// `WorkerState` implements it, so `watchdog_tick` / `stop_all` operate on
/// either without knowing the item-id type or the dispatch bookkeeping. The two
/// worker-process pokes (`kill_worker`, `send_no_more_items`) are trait methods
/// rather than a raw `&mut Worker` so a mock can stand in — the loop mechanics
/// are testable without spawning a child.
pub(crate) trait Slot {
    fn dead(&self) -> bool;
    fn finishing(&self) -> bool;
    fn set_finishing(&mut self, v: bool);
    fn timeout_killed(&self) -> bool;
    fn set_timeout_killed(&mut self);
    fn running_since(&self) -> Option<Instant>;
    /// Hard-kill the worker process (hang watchdog).
    fn kill_worker(&mut self);
    /// Tell the worker its queue is closed (`NoMoreItems`); it drains and ends.
    fn send_no_more_items(&mut self);
}

/// Hang watchdog: kill any worker stuck on a single item past `limit`. The
/// crash machinery (reader thread sees EOF) then reports the in-flight item
/// failed. Call sites gate on `worker_timeout.is_some()` and pass the limit.
pub(crate) fn watchdog_tick(states: &mut [impl Slot], limit: Duration) {
    for (widx, s) in states.iter_mut().enumerate() {
        if s.dead() || s.timeout_killed() {
            continue;
        }
        if let Some(since) = s.running_since() {
            if since.elapsed() > limit {
                eprintln!(
                    "rstest: worker gw{widx} exceeded --worker-timeout ({}s) on one test; killing it",
                    limit.as_secs()
                );
                s.set_timeout_killed();
                s.kill_worker();
            }
        }
    }
}

/// Tell every still-listening worker the queue is closed (maxfail trip or a
/// lazy collection abort): each finishes its in-flight work and ends. Bounded
/// overshoot, the trade xdist makes.
pub(crate) fn stop_all(states: &mut [impl Slot]) {
    for s in states.iter_mut().filter(|s| !s.dead() && !s.finishing()) {
        s.set_finishing(true);
        s.send_no_more_items();
    }
}

/// Build the synthetic "worker crashed while running this test" report. The
/// dead worker announced item_start before the crash, so its in-flight item is
/// reported failed and NOT retried (segfault-loop guard). `nodeid` is assembled
/// by the caller (the full pool appends a ` [gwN]` suffix under `--dist each`;
/// the lazy pool passes the bare nodeid).
pub(crate) fn fabricate_crash_report(
    nodeid: String,
    was_timeout: bool,
    worker_timeout: Option<Duration>,
    worker_idx: usize,
    error: &anyhow::Error,
) -> proto::Report {
    proto::Report {
        nodeid,
        when: "call".into(),
        outcome: "failed".into(),
        duration: 0.0,
        longrepr: Some(if was_timeout {
            format!(
                "test exceeded --worker-timeout ({}s); its worker was killed (reported failed)",
                worker_timeout.map(|d| d.as_secs()).unwrap_or(0)
            )
        } else {
            format!(
                "worker gw{worker_idx} crashed while running this test \
                 (reported failed, not retried): {error:#}"
            )
        }),
        wasxfail: false,
        skip_reason: None,
        cpu: None,
        thread_delta: None,
        fd_delta: None,
        sections: Vec::new(),
        lineno: None,
    }
}

/// Whether a finished item's failed attempt is retry-eligible under
/// `--only-rerun`: an empty pattern list always allows; otherwise at least one
/// buffered report must be a failure whose `longrepr` matches a pattern. (The
/// rerun-budget and known-flaky gates are applied separately at the call site.)
pub(crate) fn rerun_allowed(only_rerun: &[regex::Regex], attempt: &[proto::Report]) -> bool {
    only_rerun.is_empty()
        || attempt.iter().any(|r| {
            r.outcome == "failed"
                && r.longrepr
                    .as_deref()
                    .is_some_and(|t| only_rerun.iter().any(|re| re.is_match(t)))
        })
}

/// A finished item's buffered rerun attempt, ready to be committed as final.
pub(crate) struct FinishedAttempt {
    /// Retries already spent on this item (0 = ran once, no rerun).
    pub attempts_used: u32,
    /// The buffered reports of the final attempt, recorded as-is.
    pub reports: Vec<proto::Report>,
    /// Whether the final attempt failed (drives the flaky mark).
    pub failed: bool,
    /// Nodeid to mark flaky under, when the item passed after >0 retries.
    pub flaky_key: Option<String>,
}

/// Commit a finished item's buffered attempt reports as final: record each
/// (counting failures toward `fail_count`) and, if the item ultimately passed
/// after >0 retries, mark it flaky under `flaky_key`. Called on the terminal
/// (non-requeued) branch of the ItemDone rerun logic in both loops.
pub(crate) fn finalize_attempt(
    run: &mut Run,
    prog: &mut Progress,
    fail_count: &mut u64,
    worker_idx: usize,
    attempt: FinishedAttempt,
) {
    for r in attempt.reports {
        if r.outcome == "failed" {
            *fail_count += 1;
        }
        prog.on_report(Some(worker_idx), &r);
        run.record(Some(worker_idx), r);
    }
    if !attempt.failed && attempt.attempts_used > 0 {
        if let Some(k) = attempt.flaky_key {
            run.mark_flaky(k, attempt.attempts_used);
        }
    }
}

/// Reconcile the process exit status from per-worker session codes and the
/// recorded outcomes. Recorded outcomes win over session codes both ways: a
/// fabricated crash failure never hits a session (codes read 0), and a flaky
/// test's first attempt fails inside a session (code 1) though it finally
/// passed. `collect_aborted` (lazy only) forces at least "interrupted" (2).
pub(crate) fn finalize_exit(
    statuses: &[i32],
    all_passed: bool,
    reruns: u32,
    collect_aborted: bool,
) -> i32 {
    let mut exitstatus = crate::scheduling::pool::merge_statuses(statuses);
    if collect_aborted {
        exitstatus = exitstatus.max(2);
    }
    if exitstatus == 0 && !all_passed {
        exitstatus = 1;
    }
    if reruns > 0 && exitstatus == 1 && all_passed {
        exitstatus = 0;
    }
    exitstatus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockSlot {
        dead: bool,
        finishing: bool,
        timeout_killed: bool,
        running_since: Option<Instant>,
        killed: u32,
        no_more_sent: u32,
    }

    impl Slot for MockSlot {
        fn dead(&self) -> bool {
            self.dead
        }
        fn finishing(&self) -> bool {
            self.finishing
        }
        fn set_finishing(&mut self, v: bool) {
            self.finishing = v;
        }
        fn timeout_killed(&self) -> bool {
            self.timeout_killed
        }
        fn set_timeout_killed(&mut self) {
            self.timeout_killed = true;
        }
        fn running_since(&self) -> Option<Instant> {
            self.running_since
        }
        fn kill_worker(&mut self) {
            self.killed += 1;
        }
        fn send_no_more_items(&mut self) {
            self.no_more_sent += 1;
        }
    }

    fn rep(outcome: &str, longrepr: Option<&str>) -> proto::Report {
        proto::Report {
            nodeid: "t.py::x".into(),
            when: "call".into(),
            outcome: outcome.into(),
            duration: 0.0,
            longrepr: longrepr.map(str::to_string),
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            thread_delta: None,
            fd_delta: None,
            sections: Vec::new(),
            lineno: None,
        }
    }

    #[test]
    fn stop_all_finishes_only_live_unfinishing_workers() {
        let mut states = vec![
            MockSlot::default(), // live -> stopped
            MockSlot {
                dead: true,
                ..Default::default()
            }, // dead -> skipped
            MockSlot {
                finishing: true,
                ..Default::default()
            }, // finishing -> skipped
            MockSlot::default(), // live -> stopped
        ];
        stop_all(&mut states);
        assert!(states[0].finishing && states[0].no_more_sent == 1);
        // A dead worker is never told and never marked finishing.
        assert!(!states[1].finishing && states[1].no_more_sent == 0);
        // An already-finishing worker is not re-sent (bounded overshoot).
        assert_eq!(states[2].no_more_sent, 0);
        assert!(states[3].finishing && states[3].no_more_sent == 1);
    }

    #[test]
    fn watchdog_kills_only_over_limit_running_workers() {
        let stuck = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .expect("subtract an hour");
        let mut states = vec![
            MockSlot {
                running_since: Some(stuck),
                ..Default::default()
            }, // over limit -> killed
            MockSlot {
                running_since: None,
                ..Default::default()
            }, // idle -> untouched
            // Over limit but already killed once: not re-killed.
            MockSlot {
                running_since: Some(stuck),
                timeout_killed: true,
                ..Default::default()
            },
            // Over limit but dead: skipped.
            MockSlot {
                running_since: Some(stuck),
                dead: true,
                ..Default::default()
            },
            MockSlot {
                running_since: Some(Instant::now()),
                ..Default::default()
            }, // fresh -> under limit
        ];
        watchdog_tick(&mut states, Duration::from_secs(1));
        assert!(states[0].killed == 1 && states[0].timeout_killed);
        assert_eq!(states[1].killed, 0);
        assert_eq!(states[2].killed, 0);
        assert_eq!(states[3].killed, 0);
        assert_eq!(states[4].killed, 0);
    }

    #[test]
    fn crash_report_is_failed_call_with_reason() {
        let e = anyhow::anyhow!("boom");
        let r = fabricate_crash_report("t.py::x [gw2]".into(), false, None, 2, &e);
        assert_eq!(r.nodeid, "t.py::x [gw2]");
        assert_eq!(r.when, "call");
        assert_eq!(r.outcome, "failed");
        let msg = r.longrepr.unwrap();
        assert!(msg.contains("worker gw2 crashed"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[test]
    fn crash_report_timeout_variant_names_the_limit() {
        let e = anyhow::anyhow!("eof");
        let r =
            fabricate_crash_report("t.py::x".into(), true, Some(Duration::from_secs(30)), 0, &e);
        let msg = r.longrepr.unwrap();
        assert!(msg.contains("--worker-timeout (30s)"), "{msg}");
        // Timeout variant must NOT leak the raw decode error.
        assert!(!msg.contains("eof"), "{msg}");
    }

    #[test]
    fn rerun_allowed_empty_pattern_always_true() {
        assert!(rerun_allowed(&[], &[rep("passed", None)]));
    }

    #[test]
    fn rerun_allowed_matches_failed_longrepr_only() {
        let pat = vec![regex::Regex::new("Connection reset").unwrap()];
        // A failure whose text matches -> eligible.
        assert!(rerun_allowed(
            &pat,
            &[rep("failed", Some("Connection reset by peer"))]
        ));
        // Matching text but a passing report -> not eligible.
        assert!(!rerun_allowed(
            &pat,
            &[rep("passed", Some("Connection reset by peer"))]
        ));
        // A failure whose text doesn't match -> not eligible.
        assert!(!rerun_allowed(
            &pat,
            &[rep("failed", Some("assert 1 == 2"))]
        ));
    }

    #[test]
    fn finalize_exit_recorded_outcomes_win() {
        // Clean sessions but a recorded failure (fabricated crash) -> 1.
        assert_eq!(finalize_exit(&[0, 0], false, 0, false), 1);
        // Session says failed (1) but everything ultimately passed and reruns
        // are on -> flaky pass, downgrade to 0.
        assert_eq!(finalize_exit(&[1], true, 3, false), 0);
        // Same without reruns stays failed.
        assert_eq!(finalize_exit(&[1], true, 0, false), 1);
        // All green -> 0.
        assert_eq!(finalize_exit(&[0, 0], true, 0, false), 0);
    }

    #[test]
    fn finalize_exit_collect_abort_forces_interrupted() {
        // collect_aborted floors at 2 even when sessions were clean...
        assert_eq!(finalize_exit(&[0], true, 0, true), 2);
        // ...and never downgrades a more severe code.
        assert_eq!(finalize_exit(&[3], false, 0, true), 3);
    }

    #[test]
    fn finalize_attempt_records_and_marks_flaky() {
        let mut run = Run::default();
        let mut prog = Progress::default();
        let mut fail_count = 0u64;
        // Two failed attempts already counted elsewhere; the FINAL buffered
        // attempt passed after 2 retries -> recorded, no new failures, flaky.
        finalize_attempt(
            &mut run,
            &mut prog,
            &mut fail_count,
            0,
            FinishedAttempt {
                attempts_used: 2,
                reports: vec![rep("passed", None)],
                failed: false,
                flaky_key: Some("t.py::x".into()),
            },
        );
        assert_eq!(fail_count, 0);
        assert!(run.all_passed());
    }

    #[test]
    fn finalize_attempt_counts_failures() {
        let mut run = Run::default();
        let mut prog = Progress::default();
        let mut fail_count = 0u64;
        finalize_attempt(
            &mut run,
            &mut prog,
            &mut fail_count,
            0,
            FinishedAttempt {
                attempts_used: 0,
                reports: vec![rep("failed", Some("assert"))],
                failed: true,
                flaky_key: None,
            },
        );
        assert_eq!(fail_count, 1);
        assert!(!run.all_passed());
    }
}
