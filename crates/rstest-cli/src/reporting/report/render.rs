//! Terminal rendering for a [`Run`]: the `print_*` blocks that write the
//! flaky / quarantined / durations / failures sections to stdout. Split from the
//! data model in [`super`] so recording/accounting stays free of presentation.
//! A child module, so it still reaches `Run`'s private fields.

use super::{FailureWrap, Run, TestEntry};
use crate::reporting::sink::Sink;

impl Run {
    pub fn print_flaky(
        &self,
        sink: &mut Sink,
        history: &std::collections::HashMap<String, crate::reporting::flakes::FlakeStats>,
        wrap: FailureWrap,
    ) {
        if self.flaky.is_empty() {
            return;
        }
        let header = sink
            .palette()
            .yellow("=========== flaky tests (passed after rerun) ===========");
        // GitLab has no per-line warning command; fold the whole flaky block
        // into one collapsed section so it's tucked away but greppable - the
        // CI-native analogue of GitHub's `::warning` flaky annotations.
        let gitlab = wrap == FailureWrap::GitlabSection;
        let id = format!("rstest_flaky_{}", std::process::id());
        if gitlab {
            sink.out_line(&format!(
                "\n\x1b[0Ksection_start:{}:{id}[collapsed=true]\r\x1b[0K{header}",
                crate::time::now_epoch_secs()
            ));
        } else {
            sink.out_line(&format!("\n{header}"));
        }
        for (nodeid, attempts) in &self.flaky {
            let past = history
                .get(nodeid)
                .filter(|h| h.flaky + h.failed > 0)
                .map(|h| format!("; flaked {}x before, failed {}x", h.flaky, h.failed))
                .unwrap_or_default();
            sink.out_line(&format!(
                "  {nodeid}  ({attempts} rerun{}{past})",
                if *attempts > 1 { "s" } else { "" }
            ));
        }
        if gitlab {
            sink.out_line(&format!(
                "\x1b[0Ksection_end:{}:{id}\r\x1b[0K",
                crate::time::now_epoch_secs()
            ));
        }
    }

    /// The quarantined-failures section: visible (with tracebacks - a
    /// quarantined test still needs fixing), never fatal.
    pub fn print_quarantined(
        &self,
        sink: &mut Sink,
        history: &std::collections::HashMap<String, crate::reporting::flakes::FlakeStats>,
    ) {
        let quarantined: Vec<(&String, &TestEntry)> =
            self.tests.iter().filter(|(_, e)| e.quarantined).collect();
        if quarantined.is_empty() {
            return;
        }
        let palette = sink.palette();
        sink.out_line(&format!(
            "\n{}",
            palette.yellow("=========== quarantined failures (known-flaky, non-fatal) ===========")
        ));
        for (nodeid, entry) in quarantined {
            let past = history
                .get(nodeid)
                .filter(|h| h.flaky + h.failed > 0)
                .map(|h| format!("  (flaked {}x, failed {}x before)", h.flaky, h.failed))
                .unwrap_or_default();
            sink.out_line(&format!(
                "\n{}{past}",
                palette.yellow(&format!("--- QUARANTINED {nodeid} ---"))
            ));
            if let Some(repr) = &entry.longrepr {
                sink.out_line(repr);
            }
        }
    }

    /// pytest's "slowest N durations" block (terminal summary_durations):
    /// every phase, sorted slowest first; below `min` hidden unless `vv`,
    /// with pytest's hidden-count note. `n == 0` means all.
    pub fn print_durations(&self, n: usize, min: f64, vv: bool, sink: &mut Sink) {
        if !self.track_phase_durations {
            return;
        }
        let mut rows: Vec<&(f64, String, String)> = self.phase_durations.iter().collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0));
        if n > 0 {
            rows.truncate(n);
        }
        let header = if n > 0 {
            format!("=========== slowest {n} durations ===========")
        } else {
            "=========== slowest durations ===========".to_string()
        };
        sink.out_line(&format!("\n{}", sink.palette().yellow(&header)));
        let mut hidden = 0usize;
        for (duration, when, nodeid) in rows {
            if !vv && *duration < min {
                hidden += 1;
                continue;
            }
            sink.out_line(&format!("{duration:.2}s {when:<8} {nodeid}"));
        }
        if hidden > 0 {
            // pytest's exact wording - tooling greps for it.
            sink.out_line(&format!(
                "\n({hidden} durations < {min}s hidden.  Use -vv to show these durations.)"
            ));
        }
    }

    /// The failures block, with each failure optionally wrapped in a CI
    /// log-folding construct so the job UI collapses tracebacks per test.
    pub fn print_failures(&self, sink: &mut Sink, wrap: FailureWrap) {
        let palette = sink.palette();
        let open = |header: &str, idx: usize| -> String {
            match wrap {
                FailureWrap::Plain => {
                    format!(
                        "\n{}",
                        palette.bold_red(&format!("--- FAILED {header} ---"))
                    )
                }
                FailureWrap::GitlabSection => {
                    // Section ids must be unique within the job log; the
                    // pid keeps concurrent monorepo children (whose output
                    // the parent reprints) from colliding.
                    let id = format!("rstest_fail_{}_{idx}", std::process::id());
                    format!(
                        "\n\x1b[0Ksection_start:{}:{id}[collapsed=true]\r\x1b[0K{}",
                        crate::time::now_epoch_secs(),
                        palette.bold_red(&format!("--- FAILED {header} ---"))
                    )
                }
                // The `+++` group header IS the headline in the Buildkite log
                // UI - no extra dashes.
                FailureWrap::BuildkiteGroup => {
                    format!("\n+++ {}", palette.bold_red(&format!("FAILED {header}")))
                }
            }
        };
        let close = |idx: usize| -> Option<String> {
            (wrap == FailureWrap::GitlabSection).then(|| {
                let id = format!("rstest_fail_{}_{idx}", std::process::id());
                format!(
                    "\x1b[0Ksection_end:{}:{id}\r\x1b[0K",
                    crate::time::now_epoch_secs()
                )
            })
        };
        let mut idx = 0usize;
        for (worker, nodeid, longrepr, sections) in &self.failures {
            // Quarantined failures print in their own section instead.
            if self.tests.get(nodeid).is_some_and(|e| e.quarantined) {
                continue;
            }
            let attribution = worker.map(|w| format!("[gw{w}] ")).unwrap_or_default();
            sink.out_line(&open(&format!("{attribution}{nodeid}"), idx));
            sink.out_line(longrepr);
            for (name, content) in sections {
                sink.out_line(&format!(
                    "{}\n{}",
                    palette.yellow(&format!("--------- {name} ---------")),
                    content.trim_end()
                ));
            }
            if let Some(c) = close(idx) {
                sink.out_line(&c);
            }
            idx += 1;
        }
        for (nodeid, longrepr) in &self.collect_errors {
            sink.out_line(&open(nodeid, idx));
            sink.out_line(longrepr);
            if let Some(c) = close(idx) {
                sink.out_line(&c);
            }
            idx += 1;
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::reporting::flakes::FlakeStats;
    use crate::scheduling::proto;

    fn report(nodeid: &str, when: &str, outcome: &str) -> proto::Report {
        proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: outcome.into(),
            duration: 0.5,
            longrepr: (outcome == "failed").then(|| "boom traceback".into()),
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            thread_delta: None,
            fd_delta: None,
            sections: Vec::new(),
            lineno: None,
        }
    }

    fn full(run: &mut Run, nodeid: &str, outcome: &str) {
        run.record(None, report(nodeid, "setup", "passed"));
        run.record(None, report(nodeid, "call", outcome));
        run.record(None, report(nodeid, "teardown", "passed"));
    }

    // ---- print_flaky ----

    #[test]
    fn print_flaky_empty_writes_nothing() {
        let run = Run::default();
        let (mut sink, cap) = Sink::captured();
        run.print_flaky(&mut sink, &HashMap::new(), FailureWrap::Plain);
        assert!(cap.out().is_empty(), "no flaky tests -> no section");
    }

    #[test]
    fn print_flaky_plain_lists_each_with_rerun_count() {
        let mut run = Run::default();
        run.flaky = vec![("a.py::one".into(), 1), ("a.py::two".into(), 3)];
        let (mut sink, cap) = Sink::captured();
        run.print_flaky(&mut sink, &HashMap::new(), FailureWrap::Plain);
        let out = cap.out();
        assert!(out.contains("flaky tests (passed after rerun)"));
        // Singular vs plural "rerun(s)".
        assert!(out.contains("a.py::one  (1 rerun)"), "singular:\n{out}");
        assert!(out.contains("a.py::two  (3 reruns)"), "plural:\n{out}");
        // Plain wrap emits no GitLab section markers.
        assert!(!out.contains("section_start"), "plain has no folds:\n{out}");
    }

    #[test]
    fn print_flaky_appends_history_when_present() {
        let mut run = Run::default();
        run.flaky = vec![("a.py::one".into(), 1)];
        let mut history = HashMap::new();
        history.insert(
            "a.py::one".to_string(),
            FlakeStats {
                flaky: 4,
                failed: 2,
                last_epoch: 0,
            },
        );
        let (mut sink, cap) = Sink::captured();
        run.print_flaky(&mut sink, &history, FailureWrap::Plain);
        let out = cap.out();
        assert!(
            out.contains("flaked 4x before, failed 2x"),
            "history summary should ride the line:\n{out}"
        );
    }

    #[test]
    fn print_flaky_gitlab_wraps_in_a_collapsed_section() {
        let mut run = Run::default();
        run.flaky = vec![("a.py::one".into(), 1)];
        let (mut sink, cap) = Sink::captured();
        run.print_flaky(&mut sink, &HashMap::new(), FailureWrap::GitlabSection);
        let out = cap.out();
        assert!(
            out.contains("section_start"),
            "gitlab opens a section:\n{out}"
        );
        assert!(out.contains("section_end"), "gitlab closes it:\n{out}");
    }

    // ---- print_quarantined ----

    #[test]
    fn print_quarantined_empty_writes_nothing() {
        let mut run = Run::default();
        full(&mut run, "a.py::ok", "passed");
        let (mut sink, cap) = Sink::captured();
        run.print_quarantined(&mut sink, &HashMap::new());
        assert!(cap.out().is_empty(), "no quarantined -> no section");
    }

    #[test]
    fn print_quarantined_shows_traceback_and_history() {
        let mut run = Run::default();
        full(&mut run, "a.py::flake", "failed");
        run.quarantine(|id| id == "a.py::flake");
        let mut history = HashMap::new();
        history.insert(
            "a.py::flake".to_string(),
            FlakeStats {
                flaky: 1,
                failed: 5,
                last_epoch: 0,
            },
        );
        let (mut sink, cap) = Sink::captured();
        run.print_quarantined(&mut sink, &history);
        let out = cap.out();
        assert!(out.contains("quarantined failures"), "header:\n{out}");
        assert!(
            out.contains("QUARANTINED a.py::flake"),
            "per-test title:\n{out}"
        );
        assert!(
            out.contains("flaked 1x, failed 5x before"),
            "history:\n{out}"
        );
        assert!(
            out.contains("boom traceback"),
            "still prints the repr:\n{out}"
        );
    }

    // ---- print_durations ----

    #[test]
    fn print_durations_untracked_writes_nothing() {
        let mut run = Run::default();
        full(&mut run, "a.py::ok", "passed"); // durations not tracked
        let (mut sink, cap) = Sink::captured();
        run.print_durations(0, 0.0, false, &mut sink);
        assert!(cap.out().is_empty(), "no tracking -> no durations block");
    }

    #[test]
    fn print_durations_sorts_truncates_and_counts_hidden() {
        let mut run = Run::default();
        run.track_phase_durations = true;
        // Two tests; each records setup/call/teardown at 0.5s.
        full(&mut run, "a.py::one", "passed");
        full(&mut run, "a.py::two", "passed");
        // Slowest-2, with a min above 0.5 so everything shown is hidden instead.
        let (mut sink, cap) = Sink::captured();
        run.print_durations(2, 1.0, false, &mut sink);
        let out = cap.out();
        assert!(out.contains("slowest 2 durations"), "n>0 header:\n{out}");
        assert!(
            out.contains("durations < 1s hidden"),
            "sub-min rows report a hidden count:\n{out}"
        );
    }

    #[test]
    fn print_durations_vv_shows_all_and_n_zero_header() {
        let mut run = Run::default();
        run.track_phase_durations = true;
        full(&mut run, "a.py::one", "passed");
        let (mut sink, cap) = Sink::captured();
        // n==0 -> "slowest durations"; vv -> nothing hidden even below min.
        run.print_durations(0, 100.0, true, &mut sink);
        let out = cap.out();
        assert!(out.contains("slowest durations ="), "n==0 header:\n{out}");
        assert!(!out.contains("hidden"), "vv shows everything:\n{out}");
        assert!(out.contains("0.50s"), "prints the row:\n{out}");
    }

    // ---- print_failures ----

    #[test]
    fn print_failures_plain_prints_repr_and_sections() {
        let mut run = Run::default();
        let mut r = report("a.py::bad", "call", "failed");
        r.sections = vec![("Captured stdout".into(), "hello\n".into())];
        run.record(None, report("a.py::bad", "setup", "passed"));
        run.record(None, r);
        let (mut sink, cap) = Sink::captured();
        run.print_failures(&mut sink, FailureWrap::Plain);
        let out = cap.out();
        assert!(out.contains("--- FAILED a.py::bad ---"), "header:\n{out}");
        assert!(out.contains("boom traceback"), "repr body:\n{out}");
        assert!(out.contains("Captured stdout"), "section header:\n{out}");
        assert!(out.contains("hello"), "section body:\n{out}");
    }

    #[test]
    fn print_failures_gitlab_folds_each_failure() {
        let mut run = Run::default();
        full(&mut run, "a.py::bad", "failed");
        let (mut sink, cap) = Sink::captured();
        run.print_failures(&mut sink, FailureWrap::GitlabSection);
        let out = cap.out();
        assert!(out.contains("section_start"), "opens a fold:\n{out}");
        assert!(out.contains("section_end"), "closes it:\n{out}");
    }

    #[test]
    fn print_failures_buildkite_uses_group_header() {
        let mut run = Run::default();
        full(&mut run, "a.py::bad", "failed");
        let (mut sink, cap) = Sink::captured();
        run.print_failures(&mut sink, FailureWrap::BuildkiteGroup);
        let out = cap.out();
        assert!(out.contains("+++ "), "buildkite groups with +++:\n{out}");
    }

    #[test]
    fn print_failures_skips_quarantined_and_shows_collect_errors() {
        let mut run = Run::default();
        full(&mut run, "a.py::flake", "failed");
        full(&mut run, "a.py::real", "failed");
        run.quarantine(|id| id == "a.py::flake");
        run.collect_error("c.py".into(), "ImportError: boom".into());
        let (mut sink, cap) = Sink::captured();
        run.print_failures(&mut sink, FailureWrap::Plain);
        let out = cap.out();
        assert!(out.contains("a.py::real"), "real bug shown:\n{out}");
        assert!(
            !out.contains("FAILED a.py::flake"),
            "quarantined prints elsewhere, not here:\n{out}"
        );
        assert!(out.contains("c.py"), "collect error shown:\n{out}");
        assert!(out.contains("ImportError"), "collect repr shown:\n{out}");
    }
}
