//! Terminal rendering for a [`Run`]: the `print_*` blocks that write the
//! flaky / quarantined / durations / failures sections to stdout. Split from the
//! data model in [`super`] so recording/accounting stays free of presentation.
//! A child module, so it still reaches `Run`'s private fields.

use super::{FailureWrap, Run, TbStyle, TestEntry};
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
            if let Some(repr) = entry
                .longrepr
                .as_ref()
                .filter(|_| sink.tb_style() != TbStyle::No)
            {
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

    /// The "failures by cause" block: failures sharing an exception, message
    /// and project frame, one entry per group (see [`Run::failure_groups`]),
    /// so a broken dependency failing dozens of tests reads as one cause.
    /// Nothing when no two failures share a cause.
    pub fn print_failure_groups(&self, sink: &mut Sink) {
        const SHOWN: usize = 5;
        let groups = self.failure_groups();
        if groups.is_empty() {
            return;
        }
        let palette = sink.palette();
        sink.out_line(&format!(
            "\n{}",
            palette.bold_red("=========== failures by cause ===========")
        ));
        for g in &groups {
            let cause = if g.message.is_empty() {
                g.exception.clone()
            } else {
                format!("{}: {}", g.exception, g.message)
            };
            sink.out_line(&format!("{} tests: {cause}", g.count));
            if let Some(at) = &g.at {
                sink.out_line(&format!("  at {at}"));
            }
            for id in g.nodeids.iter().take(SHOWN) {
                sink.out_line(&format!("    {id}"));
            }
            if g.count > SHOWN {
                sink.out_line(&palette.dim(&format!("    ... and {} more", g.count - SHOWN)));
            }
        }
    }

    /// The failures block, with each failure optionally wrapped in a CI
    /// log-folding construct so the job UI collapses tracebacks per test.
    /// `--tb=no` prints nothing (pytest drops its FAILURES and ERRORS
    /// sections); `--tb=line` prints pytest's headerless form: the failure
    /// text, its captured output, then the `path:line: message` crash line
    /// the worker appended to the text. Headers name the phase as pytest
    /// does: `FAILED` for a test, `ERROR at setup of` / `ERROR at teardown
    /// of` for a fixture error, `ERROR collecting` for a collection error.
    pub fn print_failures(&self, sink: &mut Sink, wrap: FailureWrap) {
        let palette = sink.palette();
        let tb = sink.tb_style();
        if tb == TbStyle::No {
            return;
        }
        // `header` is the whole headline, e.g. `FAILED [gw0] a.py::t`.
        let open = |header: &str, idx: usize| -> String {
            match wrap {
                FailureWrap::Plain => {
                    format!("\n{}", palette.bold_red(&format!("--- {header} ---")))
                }
                FailureWrap::GitlabSection => {
                    // Section ids must be unique within the job log; the
                    // pid keeps concurrent monorepo children (whose output
                    // the parent reprints) from colliding.
                    let id = format!("rstest_fail_{}_{idx}", std::process::id());
                    format!(
                        "\n\x1b[0Ksection_start:{}:{id}[collapsed=true]\r\x1b[0K{}",
                        crate::time::now_epoch_secs(),
                        palette.bold_red(&format!("--- {header} ---"))
                    )
                }
                // The `+++` group header IS the headline in the Buildkite log
                // UI - no extra dashes.
                FailureWrap::BuildkiteGroup => {
                    format!("\n+++ {}", palette.bold_red(header))
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
        for f in &self.failures {
            let (nodeid, longrepr) = (&f.nodeid, &f.longrepr);
            // Quarantined failures print in their own section instead.
            if self.tests.get(nodeid).is_some_and(|e| e.quarantined) {
                continue;
            }
            let attribution = f.worker.map(|w| format!("[gw{w}] ")).unwrap_or_default();
            let label = match f.when.as_str() {
                "setup" => "ERROR at setup of",
                "teardown" => "ERROR at teardown of",
                _ => "FAILED",
            };
            // Only a call-phase failure carries the crash line (pytest's
            // FAILURES section); setup/teardown errors keep their header,
            // as in pytest's ERRORS section.
            let line_form = tb == TbStyle::Line && f.when == "call";
            let (body, crash) = if line_form {
                match longrepr.rsplit_once('\n') {
                    Some((body, crash)) => (body, Some(crash)),
                    None => ("", Some(longrepr.as_str())),
                }
            } else {
                (longrepr.as_str(), None)
            };
            if !line_form || wrap != FailureWrap::Plain {
                sink.out_line(&open(&format!("{label} {attribution}{nodeid}"), idx));
            } else if idx == 0 {
                // The blank line a header would have opened with.
                sink.out_line("");
            }
            if !body.is_empty() || crash.is_none() {
                sink.out_line(body);
            }
            for (name, content) in &f.sections {
                sink.out_line(&format!(
                    "{}\n{}",
                    palette.yellow(&format!("--------- {name} ---------")),
                    content.trim_end()
                ));
            }
            if let Some(crash) = crash {
                sink.out_line(crash);
            }
            if let Some(c) = close(idx) {
                sink.out_line(&c);
            }
            idx += 1;
        }
        for (nodeid, longrepr) in &self.collect_errors {
            // Synthetic `<...>` entries are orchestrator notes, not modules.
            let header = if nodeid.starts_with('<') {
                format!("ERROR {nodeid}")
            } else {
                format!("ERROR collecting {nodeid}")
            };
            sink.out_line(&open(&header, idx));
            sink.out_line(longrepr);
            if let Some(c) = close(idx) {
                sink.out_line(&c);
            }
            idx += 1;
        }
    }

    /// pytest's "short test summary info" block, driven by the resolved `-r`
    /// chars: one line per report in the chars' order (`FAILED`, `ERROR`,
    /// `PASSED`, `XFAIL`, `XPASS`, folded `SKIPPED [N]`). `msg_width` trims
    /// each failure message to its first line within that many columns, as
    /// pytest does off CI; `None` prints it whole (CI, `-vv`). Tests are
    /// listed in file and line order, so a parallel run prints a stable list.
    pub fn print_short_summary(&self, sink: &mut Sink, chars: &str, msg_width: Option<usize>) {
        let palette = sink.palette();
        let order = |nodeid: &str| {
            let file = nodeid.split("::").next().unwrap_or(nodeid);
            let line = self.tests.get(nodeid).and_then(|e| e.lineno);
            (file.to_string(), line, nodeid.to_string())
        };
        let mut failures: Vec<&super::Failure> = self
            .failures
            .iter()
            .filter(|f| !self.tests.get(&f.nodeid).is_some_and(|e| e.quarantined))
            .collect();
        failures.sort_by_cached_key(|f| order(&f.nodeid));
        let mut tests: Vec<(&String, &TestEntry)> =
            self.tests.iter().filter(|(_, e)| !e.cached).collect();
        tests.sort_by_cached_key(|(id, _)| order(id));

        let mut lines: Vec<String> = Vec::new();
        let with_message = |word: String, nodeid: &str, msg: Option<&str>| -> String {
            let line = format!("{word} {nodeid}");
            let Some(msg) = msg else { return line };
            match msg_width {
                None => format!("{line} - {msg}"),
                Some(width) => {
                    let used = word_width(&line);
                    match trim_message(msg, width.saturating_sub(used)) {
                        Some(msg) => format!("{line} - {msg}"),
                        None => line,
                    }
                }
            }
        };
        for c in chars.chars() {
            match c {
                'f' | 'E' => {
                    if c == 'E' {
                        for (path, _) in &self.collect_errors {
                            lines.push(format!("{} {path}", palette.red("ERROR")));
                        }
                    }
                    for f in failures.iter().filter(|f| (f.when == "call") == (c == 'f')) {
                        let word = if c == 'f' { "FAILED" } else { "ERROR" };
                        let msg = f
                            .crash
                            .as_deref()
                            .or_else(|| f.longrepr.lines().find(|l| !l.trim().is_empty()));
                        lines.push(with_message(palette.red(word), &f.nodeid, msg));
                    }
                }
                'p' => {
                    for (id, e) in &tests {
                        if !e.wasxfail && e.call.as_deref() == Some("passed") {
                            lines.push(format!("{} {id}", palette.green("PASSED")));
                        }
                    }
                }
                'x' | 'X' => {
                    let word = if c == 'x' { "XFAIL" } else { "XPASS" };
                    for (id, e) in &tests {
                        let skipped = e.setup.as_deref() == Some("skipped")
                            || e.call.as_deref() == Some("skipped");
                        let passed = e.call.as_deref() == Some("passed");
                        if e.wasxfail && (if c == 'x' { skipped } else { passed }) {
                            let mut line = format!("{} {id}", palette.yellow(word));
                            if let Some(reason) = &e.xfail_reason {
                                line.push_str(&format!(" - {reason}"));
                            }
                            lines.push(line);
                        }
                    }
                }
                's' => {
                    // pytest folds skips sharing a location and reason.
                    // (path, line, reason, count), in first-seen order.
                    let mut folds = Vec::new();
                    for (_, e) in &tests {
                        let Some((path, line)) = &e.skip_location else {
                            continue;
                        };
                        let reason = e.skip_reason.as_deref().unwrap_or("");
                        match folds
                            .iter_mut()
                            .find(|(p, l, r, _)| *p == path && *l == line && *r == reason)
                        {
                            Some((_, _, _, n)) => *n += 1,
                            None => folds.push((path, line, reason, 1)),
                        }
                    }
                    for (path, line, reason, n) in folds {
                        let reason = reason.strip_prefix("Skipped: ").unwrap_or(reason);
                        let word = palette.yellow("SKIPPED");
                        lines.push(match line {
                            Some(l) => format!("{word} [{n}] {path}:{l}: {reason}"),
                            None => format!("{word} [{n}] {path}: {reason}"),
                        });
                    }
                }
                _ => {}
            }
        }
        if lines.is_empty() {
            return;
        }
        sink.out_line(&format!(
            "\n{}",
            palette.yellow("=========== short test summary info ===========")
        ));
        for line in lines {
            sink.out_line(&line);
        }
    }
}

/// Visible width of a line that may carry color codes.
fn word_width(line: &str) -> usize {
    let mut width = 0;
    let mut in_escape = false;
    for c in line.chars() {
        match (in_escape, c) {
            (false, '\x1b') => in_escape = true,
            (true, 'm') => in_escape = false,
            (true, _) => {}
            (false, _) => width += 1,
        }
    }
    width
}

/// pytest's `_format_trimmed(" - {}", msg, available)`: the message's first
/// line, ellipsized to fit after the ` - ` separator; `None` when not even
/// the ellipsis fits. Width counts chars.
// ponytail: chars, not wcswidth; wide (CJK/emoji) messages may overrun a column or two.
fn trim_message(msg: &str, available: usize) -> Option<String> {
    let first = msg.lines().next().unwrap_or("");
    const SEP: usize = 3; // " - "
    const ELLIPSIS: &str = "...";
    if SEP + ELLIPSIS.len() > available {
        return None;
    }
    if SEP + first.chars().count() <= available {
        return Some(first.to_string());
    }
    let keep = available - ELLIPSIS.len() - SEP;
    Some(format!(
        "{}{ELLIPSIS}",
        first.chars().take(keep).collect::<String>()
    ))
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
            subtest: false,
            crash: None,
            cause: None,
            xfail_reason: None,
            skip_location: None,
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
                last_failed_epoch: 0,
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
                last_failed_epoch: 0,
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

    #[test]
    fn print_failure_groups_lists_each_cause_once() {
        let mut run = Run::default();
        for i in 0..7 {
            let mut r = report(&format!("t.py::test_{i}"), "call", "failed");
            r.cause = Some(proto::Cause {
                exc: "ConnectionRefusedError".into(),
                message: "[Errno 111] Connection refused".into(),
                at: Some("app/db.py:12".into()),
            });
            run.record(None, r);
        }
        run.record(None, report("t.py::alone", "call", "failed"));
        let (mut sink, cap) = Sink::captured();
        run.print_failure_groups(&mut sink);
        let out = cap.out();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "=========== failures by cause ===========");
        assert_eq!(
            lines[2],
            "7 tests: ConnectionRefusedError: [Errno 111] Connection refused"
        );
        assert_eq!(lines[3], "  at app/db.py:12");
        assert_eq!(lines[4], "    t.py::test_0");
        assert_eq!(lines[9], "    ... and 2 more");
        assert_eq!(lines.len(), 10, "{out}");
        // No shared cause: nothing at all.
        let mut run = Run::default();
        run.record(None, report("t.py::alone", "call", "failed"));
        let (mut sink, cap) = Sink::captured();
        run.print_failure_groups(&mut sink);
        assert_eq!(cap.out(), "");
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

    #[test]
    fn print_failures_tb_no_prints_nothing() {
        let mut run = Run::default();
        full(&mut run, "a.py::bad", "failed");
        run.collect_error("c.py".into(), "ImportError: boom".into());
        let (mut sink, cap) = Sink::captured();
        sink.set_tb_style(TbStyle::No);
        run.print_failures(&mut sink, FailureWrap::Plain);
        assert!(cap.out().is_empty(), "--tb=no: {}", cap.out());
    }

    #[test]
    fn print_failures_tb_line_is_headerless_with_crash_line_last() {
        let mut run = Run::default();
        let mut r = report("a.py::bad", "call", "failed");
        r.longrepr = Some("E   AssertionError: no\n/abs/a.py:3: AssertionError: no".into());
        r.sections = vec![("Captured stdout call".into(), "hello\n".into())];
        run.record(None, report("a.py::bad", "setup", "passed"));
        run.record(None, r);
        // A setup error keeps pytest's headed ERRORS form.
        run.record(None, report("a.py::err", "setup", "failed"));
        let (mut sink, cap) = Sink::captured();
        sink.set_tb_style(TbStyle::Line);
        run.print_failures(&mut sink, FailureWrap::Plain);
        let out = cap.out();
        assert!(!out.contains("FAILED a.py::bad"), "no header:\n{out}");
        let e = out.find("E   AssertionError").unwrap();
        let captured = out.find("hello").unwrap();
        let crash = out.find("/abs/a.py:3: AssertionError: no").unwrap();
        assert!(e < captured && captured < crash, "pytest's order:\n{out}");
        assert!(
            out.contains("--- ERROR at setup of a.py::err ---"),
            "error keeps header:\n{out}"
        );
    }

    #[test]
    fn print_failures_heads_errors_by_phase_like_pytest() {
        let mut run = Run::default();
        run.record(Some(0), report("a.py::t", "setup", "passed"));
        run.record(Some(0), report("a.py::t", "call", "failed"));
        run.record(Some(0), report("a.py::t", "teardown", "failed"));
        run.record(Some(1), report("a.py::s", "setup", "failed"));
        run.collect_error("c.py".into(), "ImportError: boom".into());
        run.collect_error("<worker gw1>".into(), "crashed".into());
        let (mut sink, cap) = Sink::captured();
        run.print_failures(&mut sink, FailureWrap::Plain);
        let out = cap.out();
        assert!(out.contains("--- FAILED [gw0] a.py::t ---"), "{out}");
        assert!(
            out.contains("--- ERROR at teardown of [gw0] a.py::t ---"),
            "{out}"
        );
        assert!(
            out.contains("--- ERROR at setup of [gw1] a.py::s ---"),
            "{out}"
        );
        assert!(out.contains("--- ERROR collecting c.py ---"), "{out}");
        assert!(out.contains("--- ERROR <worker gw1> ---"), "{out}");
        let (mut sink, cap) = Sink::captured();
        run.print_failures(&mut sink, FailureWrap::BuildkiteGroup);
        assert!(
            cap.out().contains("+++ ERROR at teardown of [gw0] a.py::t"),
            "{}",
            cap.out()
        );
    }

    #[test]
    fn print_failures_tb_line_keeps_the_teardown_error_header_after_a_call_failure() {
        // The call failed AND teardown errored: only the call report carries
        // the `--tb=line` crash line, so the teardown error stays headed.
        let mut run = Run::default();
        let mut call = report("a.py::t", "call", "failed");
        call.longrepr = Some("E   assert 1 == 2\n/abs/a.py:3: assert 1 == 2".into());
        let mut td = report("a.py::t", "teardown", "failed");
        td.longrepr = Some("E   RuntimeError: teardown boom".into());
        run.record(None, report("a.py::t", "setup", "passed"));
        run.record(None, call);
        run.record(None, td);
        let (mut sink, cap) = Sink::captured();
        sink.set_tb_style(TbStyle::Line);
        run.print_failures(&mut sink, FailureWrap::Plain);
        let out = cap.out();
        assert!(!out.contains("FAILED a.py::t"), "{out}");
        assert!(
            out.contains("--- ERROR at teardown of a.py::t ---\nE   RuntimeError"),
            "{out}"
        );
    }

    // ---- print_short_summary ----

    /// A report carrying the short-summary fields.
    fn rep(nodeid: &str, when: &str, outcome: &str, crash: Option<&str>) -> proto::Report {
        let mut r = report(nodeid, when, outcome);
        r.crash = crash.map(Into::into);
        r
    }

    /// The `-rA` suite from R23: pass + teardown error, fail + teardown
    /// error, setup error, pass, skip, xfail, xpass.
    fn r23_run() -> Run {
        let mut run = Run::default();
        let lineno = |mut r: proto::Report, l: u64| {
            r.lineno = Some(l);
            r
        };
        // Recorded out of file order, as two workers would.
        run.record(
            Some(1),
            lineno(
                rep(
                    "t.py::setup_err",
                    "setup",
                    "failed",
                    Some("ValueError: setup boom"),
                ),
                17,
            ),
        );
        run.record(
            Some(0),
            lineno(rep("t.py::pass_td", "setup", "passed", None), 11),
        );
        run.record(Some(0), rep("t.py::pass_td", "call", "passed", None));
        run.record(
            Some(0),
            rep(
                "t.py::pass_td",
                "teardown",
                "failed",
                Some("RuntimeError: teardown boom"),
            ),
        );
        run.record(
            Some(0),
            lineno(rep("t.py::fail_td", "setup", "passed", None), 14),
        );
        run.record(
            Some(0),
            rep("t.py::fail_td", "call", "failed", Some("assert 1 == 2")),
        );
        run.record(
            Some(0),
            rep(
                "t.py::fail_td",
                "teardown",
                "failed",
                Some("RuntimeError: teardown boom"),
            ),
        );
        run.record(Some(1), lineno(report("t.py::ok", "setup", "passed"), 20));
        run.record(Some(1), report("t.py::ok", "call", "passed"));
        let mut skip = lineno(rep("t.py::skip", "setup", "skipped", None), 23);
        skip.skip_reason = Some("Skipped: nope".into());
        skip.skip_location = Some(("t.py".into(), Some(24)));
        run.record(Some(1), skip);
        for (id, outcome, reason) in [
            ("t.py::xf", "skipped", "known"),
            ("t.py::xp", "passed", "lucky"),
        ] {
            run.record(Some(1), report(id, "setup", "passed"));
            let mut call = report(id, "call", outcome);
            call.wasxfail = true;
            call.xfail_reason = Some(reason.into());
            run.record(Some(1), call);
        }
        run
    }

    fn summary(run: &Run, chars: &str, width: Option<usize>) -> String {
        let (mut sink, cap) = Sink::captured();
        run.print_short_summary(&mut sink, chars, width);
        cap.out()
    }

    #[test]
    fn short_summary_default_lists_failures_then_errors_in_file_order() {
        let out = summary(&r23_run(), "fE", None);
        assert_eq!(
            out,
            "\n=========== short test summary info ===========\n\
             FAILED t.py::fail_td - assert 1 == 2\n\
             ERROR t.py::pass_td - RuntimeError: teardown boom\n\
             ERROR t.py::fail_td - RuntimeError: teardown boom\n\
             ERROR t.py::setup_err - ValueError: setup boom\n"
        );
    }

    #[test]
    fn short_summary_all_chars_match_pytest_ra() {
        let out = summary(
            &r23_run(),
            &crate::reporting::report::report_chars(["-rA"]),
            None,
        );
        let lines: Vec<&str> = out.lines().skip(2).collect();
        assert_eq!(
            lines,
            [
                "PASSED t.py::pass_td",
                "PASSED t.py::ok",
                "SKIPPED [1] t.py:24: nope",
                "XFAIL t.py::xf - known",
                "XPASS t.py::xp - lucky",
                "ERROR t.py::pass_td - RuntimeError: teardown boom",
                "ERROR t.py::fail_td - RuntimeError: teardown boom",
                "ERROR t.py::setup_err - ValueError: setup boom",
                "FAILED t.py::fail_td - assert 1 == 2",
            ],
            "{out}"
        );
    }

    #[test]
    fn short_summary_folds_skips_and_skips_quarantined_cached_and_empty() {
        let mut run = Run::default();
        for id in ["a.py::one", "a.py::two"] {
            let mut r = report(id, "setup", "skipped");
            r.skip_reason = Some("Skipped: slow".into());
            r.skip_location = Some(("a.py".into(), None));
            run.record(None, r);
        }
        full(&mut run, "a.py::flake", "failed");
        run.quarantine(|id| id == "a.py::flake");
        run.record_cached("a.py::cached".into());
        run.collect_error("c.py".into(), "ImportError: boom".into());
        let out = summary(&run, "sfEp", None);
        assert_eq!(
            out.lines().skip(2).collect::<Vec<_>>(),
            ["SKIPPED [2] a.py: slow", "ERROR c.py"],
            "{out}"
        );
        assert_eq!(
            summary(&Run::default(), "fE", None),
            "",
            "nothing to list, no header"
        );
        assert_eq!(summary(&r23_run(), "", None), "", "-rN prints nothing");
    }

    #[test]
    fn short_summary_trims_messages_to_the_terminal_like_pytest() {
        let mut run = Run::default();
        run.record(
            None,
            rep(
                "t.py::a",
                "setup",
                "failed",
                Some("RuntimeError: teardown boom\nmore"),
            ),
        );
        // Without a crash message the first text line of the failure stands in.
        let mut crashed = report("t.py::b", "call", "failed");
        crashed.longrepr = Some("\nworker crashed\ndetails".into());
        run.record(None, crashed);
        // pytest's `_format_trimmed`: 36 columns leave 23 after the nodeid,
        // so 17 chars of the message, then the ellipsis.
        let out = summary(&run, "fE", Some(36));
        assert!(
            out.contains("ERROR t.py::a - RuntimeError: tea...\n"),
            "{out}"
        );
        assert!(out.contains("FAILED t.py::b - worker crashed\n"), "{out}");
        // Not even the ellipsis fits: no message at all.
        assert!(summary(&run, "E", Some(18)).contains("ERROR t.py::a\n"));
        // Untrimmed (CI, -vv): the whole message, every line.
        assert!(summary(&run, "E", None).contains("teardown boom\nmore"));
    }

    #[test]
    fn word_width_ignores_color_codes() {
        assert_eq!(super::word_width("\x1b[31mERROR\x1b[0m t.py::a"), 13);
    }
}
