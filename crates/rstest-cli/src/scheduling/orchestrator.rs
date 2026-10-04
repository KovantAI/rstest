//! Concerns shared by the eager (`pool::run_pool`) and lazy
//! (`lazy::run_lazy_pool`) orchestrator loops. Both loops keep their own
//! (divergent) dispatch model, but the crash/rerun/watchdog/wind-down/exit
//! paths are identical modulo the item-id type, so they live here as pure,
//! unit-testable helpers instead of two hand-kept copies.

use std::time::{Duration, Instant};

use crate::reporting::progress::Progress;
use crate::reporting::report::Run;
use crate::reporting::sink::Sink;
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
    /// The hang limit for the in-flight item, set at its `item_start`.
    fn running_watchdog(&self) -> Option<Watchdog>;
    /// Hard-kill the worker process (hang watchdog).
    fn kill_worker(&mut self);
    /// Tell the worker its queue is closed (`NoMoreItems`); it drains and ends.
    fn send_no_more_items(&mut self);
    /// Kill (if running) and reap the worker process, marking the slot dead.
    fn reap_dead(&mut self);
}

/// The hang limit for one in-flight item: how long its worker may sit on it
/// before the watchdog kills the worker.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Watchdog {
    pub limit: Duration,
    /// Set by `--worker-timeout` (vs derived from the test's own timeout).
    pub explicit: bool,
}

impl Watchdog {
    /// What the limit is, for the kill warning and the fabricated failure.
    fn describe(&self) -> String {
        let secs = self.limit.as_secs();
        if self.explicit {
            format!("--worker-timeout ({secs}s)")
        } else {
            format!("the hang watchdog ({secs}s: 3x the test's timeout + 10s)")
        }
    }
}

/// The watchdog for one item. An explicit `--worker-timeout` wins for every
/// test. Otherwise it is derived from the item's own effective timeout (its
/// `@pytest.mark.timeout`, else `--timeout`), which the worker reports at
/// `item_start`, at a generous multiple: the worker's in-process interrupt
/// fires first and the watchdog only catches a hang the signal can't reach (a
/// C-extension deadlock, or any test on Windows, which has no SIGALRM). Sizing
/// it per item keeps a marker longer than the global `--timeout` from being
/// killed at the global limit. Only a positive, finite timeout arms it, and the
/// duration is clamped so a huge value can't panic `from_secs_f64`.
pub(crate) fn watchdog_for(
    explicit: Option<Duration>,
    test_timeout: Option<f64>,
) -> Option<Watchdog> {
    if let Some(limit) = explicit {
        return Some(Watchdog {
            limit,
            explicit: true,
        });
    }
    test_timeout
        .filter(|t| t.is_finite() && *t > 0.0)
        .map(|t| Watchdog {
            limit: Duration::try_from_secs_f64(t * 3.0 + 10.0).unwrap_or(Duration::MAX),
            explicit: false,
        })
}

/// Hang watchdog: kill any worker stuck on its in-flight item past that item's
/// limit. The crash machinery (reader thread sees EOF) then reports the item
/// failed. Called on every idle tick; items without a limit are never killed.
pub(crate) fn watchdog_tick(sink: &mut Sink, states: &mut [impl Slot]) {
    for (widx, s) in states.iter_mut().enumerate() {
        if s.dead() || s.timeout_killed() {
            continue;
        }
        if let (Some(since), Some(wd)) = (s.running_since(), s.running_watchdog()) {
            if since.elapsed() > wd.limit {
                sink.warn(&format!(
                    "rstest: worker gw{widx} exceeded {} on one test; killing it",
                    wd.describe()
                ));
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

/// Stop the run on SIGINT/SIGTERM (see [`crate::scheduling::interrupt`]):
/// name each in-flight test, record it failed (a CI job killed by its timeout
/// then shows the hung test in every report), and kill and reap every worker,
/// so none outlives the orchestrator. `running` gives slot i's in-flight nodeid.
/// The caller pushes exit status 2 (pytest's INTERRUPTED) and leaves its loop
/// for the normal wind-down, which writes the journal.
pub(crate) fn interrupt_all<S: Slot>(
    sink: &mut Sink,
    run: &mut Run,
    prog: &mut Progress,
    states: &mut [S],
    running: impl Fn(usize, &S) -> Option<String>,
    sig: i32,
) {
    let sig = crate::scheduling::interrupt::name(sig);
    let in_flight: Vec<(usize, String, f64)> = states
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.dead())
        .filter_map(|(i, s)| {
            let secs = s.running_since().map_or(0.0, |t| t.elapsed().as_secs_f64());
            running(i, s).map(|id| (i, id, secs))
        })
        .collect();
    if in_flight.is_empty() {
        sink.warn(&format!(
            "rstest: interrupted by {sig}; stopping the workers"
        ));
    } else {
        sink.warn(&format!(
            "rstest: interrupted by {sig}; stopping the workers. Running at the time:"
        ));
        for (i, id, secs) in &in_flight {
            sink.warn(&format!("  gw{i}  {id}  ({secs:.1}s)"));
        }
    }
    for (i, id, secs) in in_flight {
        let r = proto::Report {
            longrepr: Some(format!(
                "interrupted by {sig} after {secs:.1}s: worker gw{i} was stopped \
                 while running this test (reported failed)"
            )),
            ..fabricate_crash_report(id, None, i, &anyhow::anyhow!("interrupted"))
        };
        let nodeid = r.nodeid.clone();
        prog.on_report(sink, Some(i), &r);
        sink.emit_report(Some(i), &r);
        run.record(Some(i), r);
        run.mark_crashed(&nodeid);
    }
    for s in states.iter_mut().filter(|s| !s.dead()) {
        s.reap_dead();
    }
}

/// Build the synthetic "worker crashed while running this test" report. The
/// dead worker announced item_start before the crash, so its in-flight item is
/// reported failed and NOT retried (segfault-loop guard). `nodeid` is assembled
/// by the caller (the full pool appends a ` [gwN]` suffix under `--dist each`;
/// the lazy pool passes the bare nodeid).
pub(crate) fn fabricate_crash_report(
    nodeid: String,
    killed_by: Option<Watchdog>,
    worker_idx: usize,
    error: &anyhow::Error,
) -> proto::Report {
    proto::Report {
        nodeid,
        when: "call".into(),
        outcome: "failed".into(),
        duration: 0.0,
        longrepr: Some(if let Some(wd) = killed_by {
            format!(
                "test exceeded {}; its worker was killed (reported failed)",
                wd.describe()
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
        subtest: false,
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
    sink: &mut Sink,
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
        prog.on_report(sink, Some(worker_idx), &r);
        sink.emit_report(Some(worker_idx), &r);
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
/// passed. `retried` says retries were in play (a global `--reruns` budget,
/// or any test marked flaky, which covers `@pytest.mark.flaky` retrying on
/// its own). `collect_aborted` (lazy only) forces at least "interrupted" (2).
pub(crate) fn finalize_exit(
    statuses: &[i32],
    all_passed: bool,
    retried: bool,
    collect_aborted: bool,
) -> i32 {
    let mut exitstatus = crate::scheduling::pool::merge_statuses(statuses);
    if collect_aborted {
        exitstatus = exitstatus.max(2);
    }
    if exitstatus == 0 && !all_passed {
        exitstatus = 1;
    }
    if retried && exitstatus == 1 && all_passed {
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
        watchdog: Option<Watchdog>,
        killed: u32,
        no_more_sent: u32,
        reaped: u32,
        // Only read by the unix-only interrupt_all test.
        #[cfg(unix)]
        running: Option<String>,
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
        fn running_watchdog(&self) -> Option<Watchdog> {
            self.watchdog
        }
        fn kill_worker(&mut self) {
            self.killed += 1;
        }
        fn send_no_more_items(&mut self) {
            self.no_more_sent += 1;
        }
        fn reap_dead(&mut self) {
            self.reaped += 1;
            self.dead = true;
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
            subtest: false,
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

    fn wd(secs: u64) -> Option<Watchdog> {
        Some(Watchdog {
            limit: Duration::from_secs(secs),
            explicit: true,
        })
    }

    #[test]
    fn watchdog_kills_only_over_limit_running_workers() {
        let stuck = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .expect("subtract an hour");
        let mut states = vec![
            MockSlot {
                running_since: Some(stuck),
                watchdog: wd(1),
                ..Default::default()
            }, // over limit -> killed
            MockSlot {
                running_since: None,
                watchdog: wd(1),
                ..Default::default()
            }, // idle -> untouched
            // Over limit but already killed once: not re-killed.
            MockSlot {
                running_since: Some(stuck),
                watchdog: wd(1),
                timeout_killed: true,
                ..Default::default()
            },
            // Over limit but dead: skipped.
            MockSlot {
                running_since: Some(stuck),
                watchdog: wd(1),
                dead: true,
                ..Default::default()
            },
            MockSlot {
                running_since: Some(Instant::now()),
                watchdog: wd(1),
                ..Default::default()
            }, // fresh -> under limit
            // Running for an hour with no limit (no timeout anywhere): kept.
            MockSlot {
                running_since: Some(stuck),
                watchdog: None,
                ..Default::default()
            },
        ];
        let (mut sink, _cap) = Sink::captured();
        watchdog_tick(&mut sink, &mut states);
        assert!(states[0].killed == 1 && states[0].timeout_killed);
        assert_eq!(states[1].killed, 0);
        assert_eq!(states[2].killed, 0);
        assert_eq!(states[3].killed, 0);
        assert_eq!(states[4].killed, 0);
        assert_eq!(states[5].killed, 0);
    }

    #[test]
    fn watchdog_is_per_item_not_global() {
        // Same elapsed time, two workers: one on a --timeout 1 item (limit 13s),
        // one on a @pytest.mark.timeout(300) item (limit 910s). Only the first
        // is over its own limit.
        let started = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("subtract a minute");
        let mut states = vec![
            MockSlot {
                running_since: Some(started),
                watchdog: watchdog_for(None, Some(1.0)),
                ..Default::default()
            },
            MockSlot {
                running_since: Some(started),
                watchdog: watchdog_for(None, Some(300.0)),
                ..Default::default()
            },
        ];
        let (mut sink, cap) = Sink::captured();
        watchdog_tick(&mut sink, &mut states);
        assert_eq!(states[0].killed, 1);
        assert_eq!(states[1].killed, 0);
        assert!(
            cap.err().contains("the hang watchdog (13s"),
            "{}",
            cap.err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn interrupt_all_names_and_fails_running_tests_and_reaps_every_worker() {
        let mut states = vec![
            MockSlot {
                running: Some("t.py::hang".into()),
                running_since: Some(Instant::now()),
                ..Default::default()
            },
            MockSlot::default(), // alive, idle
            MockSlot {
                dead: true, // already gone: left alone
                running: Some("t.py::ghost".into()),
                ..Default::default()
            },
        ];
        let (mut sink, cap) = Sink::captured();
        let mut run = Run::default();
        let mut prog = Progress::default();
        interrupt_all(
            &mut sink,
            &mut run,
            &mut prog,
            &mut states,
            |_, s| s.running.clone(),
            libc::SIGTERM,
        );
        assert_eq!(
            states.iter().map(|s| s.reaped).collect::<Vec<_>>(),
            [1, 1, 0]
        );
        assert!(
            cap.err().contains("interrupted by SIGTERM"),
            "{}",
            cap.err()
        );
        assert!(cap.err().contains("gw0  t.py::hang"), "{}", cap.err());
        assert!(!cap.err().contains("ghost"), "{}", cap.err());
        assert_eq!(run.counts()["failed"], 1);
        assert!(run.failure_text("t.py::hang").unwrap().contains("SIGTERM"));
    }

    #[test]
    fn watchdog_for_explicit_worker_timeout_wins() {
        let w = watchdog_for(Some(Duration::from_secs(30)), Some(300.0)).unwrap();
        assert_eq!(w.limit, Duration::from_secs(30));
        assert!(w.explicit);
        assert_eq!(
            watchdog_for(Some(Duration::from_secs(30)), None)
                .unwrap()
                .limit
                .as_secs(),
            30
        );
    }

    #[test]
    fn watchdog_for_derives_from_the_items_timeout() {
        let w = watchdog_for(None, Some(2.0)).unwrap();
        assert_eq!(w.limit, Duration::from_secs(16));
        assert!(!w.explicit);
    }

    #[test]
    fn watchdog_for_disabled_for_non_positive_or_non_finite_timeout() {
        assert_eq!(watchdog_for(None, None), None);
        assert_eq!(watchdog_for(None, Some(0.0)), None);
        assert_eq!(watchdog_for(None, Some(-4.0)), None);
        assert_eq!(watchdog_for(None, Some(f64::NAN)), None);
        assert_eq!(watchdog_for(None, Some(f64::INFINITY)), None);
    }

    #[test]
    fn watchdog_for_clamps_overflowing_timeout_instead_of_panicking() {
        assert_eq!(
            watchdog_for(None, Some(f64::MAX)).unwrap().limit,
            Duration::MAX
        );
    }

    #[test]
    fn crash_report_is_failed_call_with_reason() {
        let e = anyhow::anyhow!("boom");
        let r = fabricate_crash_report("t.py::x [gw2]".into(), None, 2, &e);
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
        let r = fabricate_crash_report("t.py::x".into(), wd(30), 0, &e);
        let msg = r.longrepr.unwrap();
        assert!(msg.contains("--worker-timeout (30s)"), "{msg}");
        // Timeout variant must NOT leak the raw decode error.
        assert!(!msg.contains("eof"), "{msg}");
        let derived = watchdog_for(None, Some(5.0));
        let msg = fabricate_crash_report("t.py::x".into(), derived, 0, &e)
            .longrepr
            .unwrap();
        assert!(msg.contains("the hang watchdog (25s"), "{msg}");
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
        assert_eq!(finalize_exit(&[0, 0], false, false, false), 1);
        // Session says failed (1) but everything ultimately passed after a
        // retry (--reruns or a lone @mark.flaky) -> flaky pass, downgrade to 0.
        assert_eq!(finalize_exit(&[1], true, true, false), 0);
        // Same with no retry in play stays failed.
        assert_eq!(finalize_exit(&[1], true, false, false), 1);
        // All green -> 0.
        assert_eq!(finalize_exit(&[0, 0], true, false, false), 0);
    }

    #[test]
    fn finalize_exit_collect_abort_forces_interrupted() {
        // collect_aborted floors at 2 even when sessions were clean...
        assert_eq!(finalize_exit(&[0], true, false, true), 2);
        // ...and never downgrades a more severe code.
        assert_eq!(finalize_exit(&[3], false, false, true), 3);
    }

    #[test]
    fn finalize_attempt_records_and_marks_flaky() {
        let mut run = Run::default();
        let mut prog = Progress::default();
        let mut fail_count = 0u64;
        let (mut sink, _cap) = Sink::captured();
        // Two failed attempts already counted elsewhere; the FINAL buffered
        // attempt passed after 2 retries -> recorded, no new failures, flaky.
        finalize_attempt(
            &mut sink,
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
        let (mut sink, _cap) = Sink::captured();
        finalize_attempt(
            &mut sink,
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
