//! `--quarantine` and `--auto-quarantine`: the quarantine set (file patterns
//! plus history flippers), the post-run demotion, and the exit status after it.

use std::io::Write;

use anyhow::Result;

use crate::cli::Cli;
use crate::reporting::report;
use crate::reporting::sink::Sink;
use crate::scheduling::pool;

/// Apply `--quarantine` BEFORE any output or exit-code consumer: classification,
/// counts, junit, report-json, and the sessionfinish envelope must all see the
/// demoted outcomes consistently. Inert under passthrough (no aggregate Run).
pub(super) fn apply_quarantine(
    cli: &Cli,
    quarantine: Option<&Quarantine>,
    outcome: &mut pool::PoolOutcome,
    passthrough: bool,
    sink: &mut Sink,
) -> Result<()> {
    if passthrough {
        // Only the explicit flag warns: a [tool.rstest] key would nag every -s run.
        if cli.quarantine.is_some() {
            warn_quarantine_passthrough(sink.err());
        }
        return Ok(());
    }
    let Some(q) = quarantine else {
        return Ok(());
    };
    let (matcher, auto_applied) = q.demotion_matcher(&outcome.run, sink);
    let demoted = outcome.run.quarantine(|id| matcher.is_match(id));
    if auto_applied {
        // The run's auto-quarantined tests (passing ones included), for the
        // HTML report.
        let ran: std::collections::BTreeSet<String> = q
            .auto()
            .iter()
            .filter(|id| outcome.run.tests().contains_key(*id))
            .cloned()
            .collect();
        outcome.run.auto_quarantined = ran;
    }
    outcome.exitstatus = quarantined_exit(
        outcome.exitstatus,
        !demoted.is_empty(),
        outcome.run.all_passed(),
        outcome.run.stopped_after.is_some(),
        !auto_applied,
    );
    Ok(())
}

/// The exit status once quarantined failures are demoted.
fn quarantined_exit(
    status: i32,
    demoted: bool,
    all_passed: bool,
    stopped: bool,
    over_cap: bool,
) -> i32 {
    // A session's own count skips quarantined failures, so the single-session
    // path can report 0 over the auto set's failures the demotion then keeps
    // (over its cap): those still fail the run. Any other 0 is the session's
    // own call (`pytest.exit(returncode=0)`, a plugin), kept as under pytest.
    if status == 0 && over_cap && !all_passed {
        return 1;
    }
    // pytest exit 1 = tests failed; if every failure was quarantined the run is
    // green by policy. Exit codes 2+ (usage/internal errors) are never touched.
    // A run -x / --maxfail cut short stays failed: demoted failures never
    // count toward the limit (auto ones count only past the cap, when none is
    // demoted), so a real failure tripped it.
    if demoted && status == 1 && all_passed && !stopped {
        return 0;
    }
    status
}

/// Note (to `w`) that `--quarantine` is inert under passthrough IO (-s/--pdb/--co):
/// there is no aggregate Run to demote outcomes in.
fn warn_quarantine_passthrough(w: &mut dyn Write) {
    let _ = writeln!(
        w,
        "rstest: --quarantine has no effect in passthrough mode; ignoring"
    );
}

pub(super) use crate::scheduling::orchestrator::Quarantine;

impl Quarantine {
    /// Matcher for the post-run demotion, and whether it includes the auto
    /// set. When more auto-set tests failed
    /// this run than `max_pct` percent of the run's tests (at least one),
    /// the environment is the likelier culprit: warn and demote only the
    /// file's tests, so those failures count as usual.
    pub(super) fn demotion_matcher(
        &self,
        run: &report::Run,
        sink: &mut Sink,
    ) -> (&regex::RegexSet, bool) {
        let failed = run
            .tests()
            .iter()
            .filter(|(id, e)| {
                e.any_phase_failed() && self.auto.contains(*id) && !self.file.is_match(id)
            })
            .count();
        let total = run.cap_total.unwrap_or_else(|| run.tests().len());
        let cap = crate::scheduling::orchestrator::auto_quarantine_cap(total, self.max_pct);
        if failed <= cap {
            return (&self.all, true);
        }
        sink.warn(&format!(
            "rstest: --auto-quarantine: {failed} auto-quarantined test(s) failed, over the cap \
             of {cap} ({}% of {total} tests in this run); quarantining none of them, the \
             environment looks broken (raise the cap with --auto-quarantine-max=PCT)",
            self.max_pct
        ));
        (&self.file, false)
    }

    /// The auto-quarantined nodeids (from the history; empty when off).
    pub(super) fn auto(&self) -> &std::collections::HashSet<String> {
        &self.auto
    }
}

/// Build the run's [`Quarantine`], announcing the auto set on stderr (the run
/// log is the audit trail, there is no committed list). `None` when nothing
/// can be quarantined.
pub(super) fn quarantine_set(
    file: Option<&std::path::Path>,
    auto: u32,
    max_pct: u32,
    history: &mut crate::reporting::history::RunHistory,
    sink: &mut Sink,
) -> Result<Option<Quarantine>> {
    let file_patterns = match file {
        Some(p) => quarantine_patterns(p, sink)?,
        None => Vec::new(),
    };
    let ids = if auto == 0 {
        Vec::new()
    } else {
        history.get().flippers(auto)
    };
    if file.is_none() && ids.is_empty() {
        return Ok(None);
    }
    if !ids.is_empty() {
        sink.warn(&format!(
            "rstest: --auto-quarantine: {} test(s) flipped pass/fail {auto}+ times in recent history:\n  {}",
            ids.len(),
            ids.join("\n  ")
        ));
    }
    let all = file_patterns
        .iter()
        .cloned()
        .chain(ids.iter().map(|id| format!("^{}$", regex::escape(id))));
    Ok(Some(Quarantine {
        all: regex::RegexSet::new(all)?,
        file: regex::RegexSet::new(file_patterns)?,
        auto: ids.into_iter().collect(),
        max_pct,
    }))
}

/// The --quarantine file as anchored regexes: exact nodeids or `*` globs,
/// one per line, `#` comments and blanks skipped. A trailing comment
/// (whitespace then `#`, e.g. `test_a  # JIRA-1`) is stripped too.
fn quarantine_patterns(path: &std::path::Path, sink: &mut Sink) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("--quarantine: cannot read {}: {e}", path.display()))?;
    let patterns: Vec<String> = text
        .lines()
        .map(strip_quarantine_comment)
        .filter(|l| !l.is_empty())
        .map(|l| {
            format!(
                "^{}$",
                l.split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(".*")
            )
        })
        .collect();
    if patterns.is_empty() {
        sink.warn(&format!(
            "rstest: --quarantine: {} lists no patterns",
            path.display()
        ));
    }
    Ok(patterns)
}

/// One quarantine line without its comment: a leading `#` blanks the line,
/// and a `#` preceded by whitespace ends the pattern. A `#` glued to the
/// nodeid (a param id like `test_a[#1]`) stays part of the pattern.
fn strip_quarantine_comment(line: &str) -> &str {
    let line = line.trim();
    if line.starts_with('#') {
        return "";
    }
    let end = line
        .char_indices()
        .find(|&(i, c)| c == '#' && line[..i].ends_with(char::is_whitespace))
        .map_or(line.len(), |(i, _)| i);
    line[..end].trim_end()
}

#[cfg(test)]
mod tests {
    use super::{quarantine_set, quarantined_exit, warn_quarantine_passthrough};
    use crate::reporting::sink::Sink;

    fn write_quarantine(suffix: &str, body: &str) -> std::path::PathBuf {
        // Unique per-test name (pid + suffix) so parallel tests never collide.
        let path = std::env::temp_dir().join(format!(
            "rstest-quarantine-test-{}-{suffix}.txt",
            std::process::id()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn quarantine_matcher(
        path: &std::path::Path,
        sink: &mut Sink,
    ) -> anyhow::Result<regex::RegexSet> {
        let none = crate::reporting::history::History::default();
        quarantine_set(Some(path), 0, 5, &mut none.into(), sink).map(|s| s.unwrap().all)
    }

    /// History where `id` has outcomes `outcomes`, one run each.
    fn history_of(tests: &[(&str, &[&str])]) -> crate::reporting::history::History {
        use crate::reporting::history::{outcome_code, Entry, History, RunMeta};
        let mut h = History::default();
        let runs = tests.iter().map(|(_, o)| o.len()).max().unwrap_or(0);
        for seq in 0..runs as u64 {
            h.runs.push(RunMeta {
                seq,
                id: format!("r{seq}"),
                epoch: 0,
                // One clean commit: pass->fail flips only count within one.
                commit: Some("c".into()),
                env: String::new(),
            });
        }
        for (id, outcomes) in tests {
            h.tests.insert(
                id.to_string(),
                outcomes
                    .iter()
                    .enumerate()
                    .map(|(i, o)| Entry {
                        run: i as u64,
                        outcome: outcome_code(o).unwrap(),
                        call: None,
                        setup: 0.0,
                    })
                    .collect(),
            );
        }
        h
    }

    #[test]
    fn quarantine_set_unions_file_with_auto_flippers() {
        let flap: &[&str] = &["passed", "failed", "passed", "failed", "passed", "failed"];
        let broke: &[&str] = &["passed", "failed", "failed", "failed"];
        // `[1]` would be a regex class if the nodeid were not escaped.
        let h = history_of(&[("a.py::flap[1]", flap), ("a.py::broke", broke)]);
        let path = write_quarantine("auto", "b.py::*\n");

        let (mut sink, cap) = Sink::captured();
        let set = quarantine_set(Some(&path), 3, 100, &mut h.clone().into(), &mut sink)
            .unwrap()
            .unwrap()
            .all;
        assert!(set.is_match("a.py::flap[1]"));
        assert!(!set.is_match("a.py::flap1"));
        assert!(!set.is_match("a.py::broke")); // steady regression still fails
        assert!(set.is_match("b.py::test_x")); // file patterns kept
        let err = cap.err();
        assert!(err.contains("--auto-quarantine: 1 test(s)"), "{err}");
        assert!(err.contains("a.py::flap[1]"), "{err}");

        // Auto alone, below threshold or off: nothing to match, no banner.
        for auto in [4, 0] {
            let (mut sink, cap) = Sink::captured();
            assert!(
                quarantine_set(None, auto, 100, &mut h.clone().into(), &mut sink)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(cap.err(), "");
        }
        // Off: the history file is never read (a large suite's costs a few
        // hundred milliseconds per run).
        let mut unread = crate::reporting::history::RunHistory::default();
        assert!(
            quarantine_set(None, 0, 100, &mut unread, &mut Sink::captured().0)
                .unwrap()
                .is_none()
        );
        assert!(!unread.is_loaded());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn auto_quarantine_cap_counts_this_runs_failures() {
        use crate::reporting::report::Run;
        use crate::scheduling::proto::Report;
        let rep = |nodeid: &str, when: &str, outcome: &str| Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: outcome.into(),
            duration: 0.1,
            longrepr: None,
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            sections: Vec::new(),
            lineno: None,
            thread_delta: None,
            fd_delta: None,
            subtest: false,
            crash: None,
            cause: None,
            xfail_reason: None,
            skip_location: None,
            network: Vec::new(),
            marker_timeout: None,
            no_rerun: false,
            subtest_desc: None,
            subtest_shown: false,
        };
        let flap: &[&str] = &["passed", "failed", "passed", "failed", "passed", "failed"];
        let h = history_of(&[("t.py::f1", flap), ("t.py::f2", flap), ("t.py::f3", flap)]);
        let path = write_quarantine("capfile", "t.py::f1\n");
        let q = quarantine_set(
            Some(&path),
            3,
            5,
            &mut h.clone().into(),
            &mut Sink::captured().0,
        )
        .unwrap()
        .unwrap();
        let run_of = |failing: &[&str], passing: usize| {
            let mut run = Run::default();
            for id in failing {
                run.record(None, rep(id, "call", "failed"));
            }
            for i in 0..passing {
                run.record(None, rep(&format!("t.py::ok{i}"), "call", "passed"));
            }
            run
        };

        // 40 tests, 5% cap = 2: f2 + f3 failing is at the cap -> all demoted.
        // f1 is in the file and never counts toward the cap.
        let run = run_of(&["t.py::f1", "t.py::f2", "t.py::f3"], 37);
        let (mut sink, cap) = Sink::captured();
        let (m, auto) = q.demotion_matcher(&run, &mut sink);
        assert!(auto && m.is_match("t.py::f2") && m.is_match("t.py::f3"));
        assert_eq!(cap.err(), "");

        // Same failures in a 20-test run: cap = 1, over it -> none of the auto
        // set is demoted, the file's test still is.
        let run = run_of(&["t.py::f1", "t.py::f2", "t.py::f3"], 17);
        let (mut sink, cap) = Sink::captured();
        let (m, auto) = q.demotion_matcher(&run, &mut sink);
        assert!(!auto && m.is_match("t.py::f1"));
        assert!(!m.is_match("t.py::f2") && !m.is_match("t.py::f3"));
        let err = cap.err();
        assert!(
            err.contains("2 auto-quarantined test(s) failed, over the cap of 1 (5% of 20"),
            "{err}"
        );

        // Passing auto-set tests don't count: a tiny run with one failure is
        // under the floor of one test.
        let run = run_of(&["t.py::f2"], 1);
        assert!(q
            .demotion_matcher(&run, &mut Sink::captured().0)
            .0
            .is_match("t.py::f2"));
        std::fs::remove_file(&path).ok();
    }

    /// `-x` cut the run short: the in-run count held the auto failures against
    /// a cap from the collected total, so the post-run demotion must use that
    /// same total, not the handful of tests that got to report.
    #[test]
    fn auto_quarantine_cap_uses_the_in_run_total_after_an_early_stop() {
        use crate::reporting::report::Run;
        use crate::scheduling::proto::Report;
        let rep = |nodeid: String, outcome: &str| Report {
            nodeid,
            when: "call".into(),
            outcome: outcome.into(),
            duration: 0.1,
            longrepr: None,
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            sections: Vec::new(),
            lineno: None,
            thread_delta: None,
            fd_delta: None,
            subtest: false,
            crash: None,
            xfail_reason: None,
            skip_location: None,
            network: Vec::new(),
            marker_timeout: None,
            no_rerun: false,
            subtest_desc: None,
            subtest_shown: false,
            cause: None,
        };
        let flap: &[&str] = &["passed", "failed", "passed", "failed", "passed", "failed"];
        let ids = ["t.py::f1", "t.py::f2", "t.py::f3", "t.py::f4"];
        let h = history_of(&ids.map(|id| (id, flap)));
        let q = quarantine_set(None, 3, 5, &mut h.clone().into(), &mut Sink::captured().0)
            .unwrap()
            .unwrap();
        // 1000 collected, stopped after 41 reported: in-run cap 50, not 3.
        let mut run = Run::default();
        for id in ids {
            run.record(None, rep(id.into(), "failed"));
        }
        for i in 0..37 {
            run.record(None, rep(format!("t.py::ok{i}"), "passed"));
        }
        run.cap_total = Some(1000);
        let (mut sink, cap) = Sink::captured();
        let (m, _) = q.demotion_matcher(&run, &mut sink);
        assert!(ids.iter().all(|id| m.is_match(id)));
        assert_eq!(cap.err(), "");
    }

    #[test]
    fn quarantine_matcher_handles_exact_globs_comments_blanks() {
        let path = write_quarantine(
            "mixed",
            "# a comment\n\ntest_foo.py::test_a\ntest_bar.py::*\n",
        );
        let set = quarantine_matcher(&path, &mut Sink::captured().0).unwrap();
        assert_eq!(set.len(), 2); // comment + blank line skipped
        assert!(set.is_match("test_foo.py::test_a")); // exact
        assert!(!set.is_match("test_foo.py::test_ab")); // anchored: no substring match
        assert!(set.is_match("test_bar.py::test_z")); // glob
        assert!(!set.is_match("other.py::test_a"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn quarantine_matcher_strips_trailing_comments() {
        let path = write_quarantine(
            "trailing",
            "test_foo.py::test_a  # JIRA-1\ntest_bar.py::*\t# flaky on CI\ntest_p.py::test_b[#1]\n",
        );
        let set = quarantine_matcher(&path, &mut Sink::captured().0).unwrap();
        assert_eq!(set.len(), 3);
        assert!(set.is_match("test_foo.py::test_a"));
        assert!(set.is_match("test_bar.py::test_z"));
        assert!(set.is_match("test_p.py::test_b[#1]")); // glued `#` is not a comment
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn quarantine_matcher_empty_when_only_comments() {
        let path = write_quarantine("empty", "# nothing here\n\n");
        let set = quarantine_matcher(&path, &mut Sink::captured().0).unwrap();
        assert_eq!(set.len(), 0);
        assert!(!set.is_match("test_foo.py::test_a"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn quarantine_matcher_errors_on_missing_file() {
        let path = std::env::temp_dir().join("rstest-quarantine-does-not-exist-xyz.txt");
        assert!(quarantine_matcher(&path, &mut Sink::captured().0).is_err());
    }

    #[test]
    fn quarantined_exit_never_passes_kept_failures() {
        // Session said 0 but the auto set went over its cap, so its failures
        // survived the demotion: red.
        assert_eq!(quarantined_exit(0, false, false, false, true), 1);
        assert_eq!(quarantined_exit(0, true, false, false, true), 1);
        // Under the cap, a 0 over a kept failure is the session's own call
        // (pytest.exit(returncode=0), a plugin): kept, as with no quarantine.
        assert_eq!(quarantined_exit(0, false, false, false, false), 0);
        assert_eq!(quarantined_exit(0, true, false, false, false), 0);
        // Every failure demoted: green, unless -x / --maxfail stopped it.
        assert_eq!(quarantined_exit(1, true, true, false, false), 0);
        assert_eq!(quarantined_exit(1, true, true, true, false), 1);
        // Nothing demoted, or a severe code: untouched.
        assert_eq!(quarantined_exit(1, false, false, false, false), 1);
        assert_eq!(quarantined_exit(2, true, true, false, false), 2);
        assert_eq!(quarantined_exit(0, false, true, false, false), 0);
    }

    #[test]
    fn warn_quarantine_passthrough_writes_the_note() {
        let mut buf = Vec::new();
        warn_quarantine_passthrough(&mut buf);
        assert!(String::from_utf8(buf)
            .unwrap()
            .contains("--quarantine has no effect in passthrough mode"));
    }
}
