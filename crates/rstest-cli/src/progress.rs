//! pytest-style live progress: one status char per test as reports stream
//! in, wrapped with a running percentage when the total is known.

use std::io::Write;

use crate::color::Palette;
use crate::proto::Report;
use crate::status::StatusFooter;

#[derive(Default, Clone, Copy, PartialEq)]
pub enum Mode {
    #[default]
    Dots,
    /// pytest -v: one `nodeid OUTCOME [ pct%]` line per test.
    Verbose,
    /// pytest-sugar-style: a per-test result line (`✓`/`✗ nodeid`) as each
    /// finishes, failures shown inline, and a live filled progress bar in
    /// the footer (pool mode). The parallel-safe answer to sugar, which
    /// can't render under a worker pool.
    Bar,
    /// GitHub Actions: renders dots for the human-readable log, plus
    /// `::error file=,title=,line=::` workflow annotations for each failure
    /// (emitted at end-of-run from the aggregate, in main).
    Github,
    /// Newline-delimited JSON: one `{"event":"testreport",...}` object per
    /// phase report, streamed live as tests finish, closed by a
    /// `{"event":"sessionfinish",...}` envelope (emitted in main). stdout is
    /// pure NDJSON — no banner, footer, or summary. For editors/tooling.
    Json,
    /// Test Anything Protocol (version 13): `ok N - nodeid` per test as it
    /// finishes, failure text as `#` diagnostics, trailing `1..N` plan
    /// (emitted in main). stdout is a pure TAP stream — no banner or
    /// summary. For TAP harnesses (Jenkins TAP plugin, prove, etc.).
    Tap,
    /// TeamCity service messages: a `testStarted`/`testFinished` pair per
    /// test (plus `testFailed`/`testIgnored`) as each finishes. The normal
    /// banner and summary stay — TeamCity ignores non-service lines.
    Teamcity,
    /// GitLab CI: renders dots for the human-readable log; each failure is
    /// wrapped in a collapsed `section_start`/`section_end` block at
    /// end-of-run, so the job log folds tracebacks per test.
    Gitlab,
    /// Buildkite: renders dots for the human-readable log; each failure is
    /// emitted under an auto-expanded `+++` group header at end-of-run.
    Buildkite,
}

#[derive(Default)]
pub struct Progress {
    done: usize,
    col: usize,
    total: Option<usize>,
    mode: Mode,
    palette: Palette,
    footer: Option<StatusFooter>,
}

const WIDTH: usize = 72;

impl Progress {
    pub fn set_total(&mut self, total: usize) {
        self.total = Some(total);
        if let Some(f) = &mut self.footer {
            f.set_total(total);
        }
    }

    /// Enable the live per-worker status footer (pool mode, tty only).
    pub fn enable_footer(&mut self, workers: usize) {
        let mut footer = StatusFooter::new(workers);
        footer.set_bar(self.mode == Mode::Bar);
        self.footer = Some(footer);
    }

    pub fn item_started(&mut self, worker: usize, nodeid: String) {
        if let Some(f) = &mut self.footer {
            f.item_started(worker, nodeid);
        }
    }

    pub fn item_finished(&mut self, worker: usize) {
        if let Some(f) = &mut self.footer {
            f.item_finished(worker);
        }
    }

    pub fn tick(&mut self) {
        if let Some(f) = &mut self.footer {
            f.tick();
        }
    }

    fn out_inline(&mut self, text: &str) {
        match &mut self.footer {
            Some(f) => f.print_inline(text),
            None => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
        }
    }

    fn out_line(&mut self, text: &str) {
        match &mut self.footer {
            Some(f) => f.print_line(text),
            None => println!("{text}"),
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if let Some(f) = &mut self.footer {
            f.set_bar(mode == Mode::Bar);
        }
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// pytest's char per outcome: '.' pass, 'F' fail, 's' skip, 'x' xfail,
    /// 'X' xpass, 'E' setup/teardown error. One char per TEST — emitted on
    /// the call report, or on a non-passed setup report (no call follows),
    /// or on a failed teardown.
    pub fn on_report(&mut self, worker: Option<usize>, r: &Report) {
        if self.mode == Mode::Json {
            return Self::on_report_json(worker, r);
        }
        if self.mode == Mode::Tap {
            return self.on_report_tap(r);
        }
        if self.mode == Mode::Teamcity {
            return self.on_report_teamcity(r);
        }
        if self.mode == Mode::Verbose {
            return self.on_report_verbose(worker, r);
        }
        if self.mode == Mode::Bar {
            return self.on_report_bar(worker, r);
        }
        // Github/Gitlab/Buildkite share the dots char stream below (the
        // human-readable log); their annotations / fold markers are emitted
        // from the aggregate at end-of-run.
        let ch = match (r.when.as_str(), r.outcome.as_str()) {
            ("call", "passed") => {
                if r.wasxfail {
                    'X'
                } else {
                    '.'
                }
            }
            ("call", "failed") => 'F',
            ("call", "skipped") => {
                if r.wasxfail {
                    'x'
                } else {
                    's'
                }
            }
            ("setup", "failed") => 'E',
            ("setup", "skipped") => {
                if r.wasxfail {
                    'x'
                } else {
                    's'
                }
            }
            ("teardown", "failed") => 'E',
            _ => return,
        };
        let painted = if r.when == "teardown" {
            // The test already printed a char at call time; an error
            // teardown gets its own marker appended.
            self.palette.outcome("E")
        } else {
            self.done += 1;
            self.palette.outcome(&ch.to_string())
        };
        self.out_inline(&painted);
        self.col += 1;
        if self.col >= WIDTH {
            self.col = 0;
            let tail = match self.total {
                Some(t) if t > 0 => format!(" [{:3}%]", self.done * 100 / t),
                _ => format!(" [{}]", self.done),
            };
            self.out_line(&tail);
        }
    }

    /// pytest -v: `nodeid OUTCOME [ pct%]` per test, ERROR lines for
    /// failed setup/teardown phases.
    fn on_report_verbose(&mut self, worker: Option<usize>, r: &Report) {
        let word = match (r.when.as_str(), r.outcome.as_str()) {
            ("call", "passed") => {
                if r.wasxfail {
                    "XPASS"
                } else {
                    "PASSED"
                }
            }
            ("call", "failed") => "FAILED",
            ("call", "skipped") => {
                if r.wasxfail {
                    "XFAIL"
                } else {
                    "SKIPPED"
                }
            }
            ("setup", "failed") | ("teardown", "failed") => "ERROR",
            ("setup", "skipped") => {
                if r.wasxfail {
                    "XFAIL"
                } else {
                    "SKIPPED"
                }
            }
            _ => return,
        };
        if r.when != "teardown" {
            self.done += 1;
        }
        let pct = match self.total {
            Some(t) if t > 0 => format!(" [{:3}%]", (self.done * 100 / t).min(100)),
            _ => String::new(),
        };
        let prefix = worker.map(|w| format!("[gw{w}] ")).unwrap_or_default();
        let line = format!("{prefix}{} {}{pct}", r.nodeid, self.palette.outcome(word));
        self.out_line(&line);
    }

    /// pytest-sugar-style per-test line: `<sym> nodeid  dur [pct%]`, with
    /// the failure repr inlined right under a failing test. Symbol colored
    /// by outcome (green pass / red fail+error / yellow skip+xfail+xpass).
    fn on_report_bar(&mut self, worker: Option<usize>, r: &Report) {
        // (symbol, is the symbol green/red/yellow, counts as a finished test)
        let (sym, color): (&str, fn(&Palette, &str) -> String) =
            match (r.when.as_str(), r.outcome.as_str()) {
                ("call", "passed") if r.wasxfail => ("X", Palette::yellow),
                ("call", "passed") => ("✓", Palette::green),
                ("call", "failed") => ("✗", Palette::red),
                ("call", "skipped") if r.wasxfail => ("x", Palette::yellow),
                ("call", "skipped") => ("s", Palette::yellow),
                ("setup", "failed") | ("teardown", "failed") => ("E", Palette::red),
                ("setup", "skipped") if r.wasxfail => ("x", Palette::yellow),
                ("setup", "skipped") => ("s", Palette::yellow),
                _ => return,
            };
        if r.when != "teardown" {
            self.done += 1;
        }
        let pct = match self.total {
            Some(t) if t > 0 => format!(" [{:>3}%]", (self.done * 100 / t).min(100)),
            _ => String::new(),
        };
        let dur = if r.duration >= 0.0005 {
            format!("  {:.2}s", r.duration)
        } else {
            String::new()
        };
        let prefix = worker.map(|w| format!("[gw{w}] ")).unwrap_or_default();
        let palette = self.palette; // Copy — avoids borrowing self during out_line
        let painted_sym = color(&palette, sym);
        let meta = format!("{dur}{pct}");
        let tail = if meta.is_empty() {
            String::new()
        } else {
            palette.dim(&meta)
        };
        self.out_line(&format!("{prefix}{painted_sym} {}{tail}", r.nodeid));
        // Sugar shows failures the moment they happen — inline the repr.
        if r.outcome == "failed" {
            if let Some(repr) = &r.longrepr {
                let header = palette.bold_red(&format!("  ── {} ──", r.nodeid));
                self.out_line(&header);
                for l in repr.trim_end().lines() {
                    self.out_line(&format!("  {l}"));
                }
            }
        }
    }

    /// One TAP test point per test as it finishes; failure text follows as
    /// `#` diagnostic lines. The trailing plan comes from [`tap_plan`] so
    /// the count always matches the points emitted.
    fn on_report_tap(&mut self, r: &Report) {
        let Some(line) = tap_result_line(self.done + 1, r) else {
            return;
        };
        self.done += 1;
        println!("{line}");
        if r.outcome == "failed" {
            if let Some(repr) = &r.longrepr {
                for l in repr.trim_end().lines() {
                    println!("# {l}");
                }
            }
        }
    }

    /// Close a TAP stream: the trailing `1..N` plan (valid TAP when the
    /// plan comes last), N = test points actually emitted.
    pub fn tap_plan(&self) {
        println!("1..{}", self.done);
    }

    /// One TeamCity service-message group per test as it finishes.
    /// Retroactive `testStarted`/`testFinished` pairs are fine — TeamCity
    /// takes the duration from the attribute, not wall-clock between
    /// messages — and emitting the whole group at once keeps parallel
    /// results from interleaving.
    fn on_report_teamcity(&mut self, r: &Report) {
        let Some(messages) = teamcity_messages(r) else {
            return;
        };
        if r.when != "teardown" {
            self.done += 1;
        }
        println!("{messages}");
    }

    /// One NDJSON object per phase report — the live event stream. Printed
    /// straight to stdout (no footer in Json mode), so consumers get a
    /// parseable line the moment each phase finishes. `longrepr` rides only
    /// on failures (it's large); `worker` only in pool runs.
    fn on_report_json(worker: Option<usize>, r: &Report) {
        let mut obj = serde_json::json!({
            "event": "testreport",
            "nodeid": r.nodeid,
            "when": r.when,
            "outcome": r.outcome,
            "duration": (r.duration * 10_000.0).round() / 10_000.0,
            "wasxfail": r.wasxfail,
        });
        if let Some(w) = worker {
            obj["worker"] = format!("gw{w}").into();
        }
        if let Some(l) = r.lineno {
            obj["lineno"] = l.into();
        }
        if r.outcome == "failed" {
            if let Some(lr) = &r.longrepr {
                obj["longrepr"] = lr.as_str().into();
            }
        }
        println!("{obj}");
    }

    /// Close the dot line before failures/summary print.
    pub fn finish(&mut self) {
        if let Some(f) = &mut self.footer {
            f.finish();
        }
        if matches!(
            self.mode,
            Mode::Verbose | Mode::Bar | Mode::Tap | Mode::Teamcity
        ) {
            return;
        }
        if self.col > 0 {
            match self.total {
                Some(t) if t > 0 => println!(" [{:3}%]", (self.done * 100 / t).min(100)),
                _ => println!(),
            }
        }
    }
}

/// The TAP test point for a phase report, or None when this phase emits
/// nothing (passed setup/teardown, in-progress phases). Mapping follows
/// TAP semantics: xfail = `not ok # TODO` (the harness expects it to
/// fail), xpass = `ok # TODO` (a bonus the harness flags), skip =
/// `ok # SKIP`. A failed teardown gets its own numbered point — the test
/// already reported at call time, and the plan must match points emitted.
fn tap_result_line(n: usize, r: &Report) -> Option<String> {
    let directive = |kind: &str, reason: Option<&str>| match reason {
        Some(why) if !why.is_empty() => format!(" # {kind} {}", why.replace(['\n', '\r'], " ")),
        _ => format!(" # {kind}"),
    };
    let line = match (r.when.as_str(), r.outcome.as_str()) {
        ("call", "passed") if r.wasxfail => {
            format!(
                "ok {n} - {}{}",
                r.nodeid,
                directive("TODO", Some("unexpectedly passed"))
            )
        }
        ("call", "passed") => format!("ok {n} - {}", r.nodeid),
        ("call", "failed") => format!("not ok {n} - {}", r.nodeid),
        ("call", "skipped") | ("setup", "skipped") if r.wasxfail => {
            format!(
                "not ok {n} - {}{}",
                r.nodeid,
                directive("TODO", Some("expected failure"))
            )
        }
        ("call", "skipped") | ("setup", "skipped") => {
            format!(
                "ok {n} - {}{}",
                r.nodeid,
                directive("SKIP", r.skip_reason.as_deref())
            )
        }
        ("setup", "failed") => format!("not ok {n} - {} # setup error", r.nodeid),
        ("teardown", "failed") => format!("not ok {n} - {} # teardown error", r.nodeid),
        _ => return None,
    };
    Some(line)
}

/// The TeamCity service-message group for a phase report, or None when
/// this phase emits nothing. `testStarted` precedes every result so
/// TeamCity attributes output correctly; duration rides on
/// `testFinished` in milliseconds.
fn teamcity_messages(r: &Report) -> Option<String> {
    let name = tc_escape(&r.nodeid);
    let started = format!("##teamcity[testStarted name='{name}']");
    let finished = format!(
        "##teamcity[testFinished name='{name}' duration='{}']",
        (r.duration * 1000.0).round() as u64
    );
    let middle = match (r.when.as_str(), r.outcome.as_str()) {
        ("call", "passed") => None,
        ("call", "failed") | ("setup", "failed") | ("teardown", "failed") => {
            let details = r.longrepr.as_deref().unwrap_or("");
            Some(format!(
                "##teamcity[testFailed name='{name}' message='{} failed' details='{}']",
                r.when,
                tc_escape(details)
            ))
        }
        ("call", "skipped") | ("setup", "skipped") => {
            let why = if r.wasxfail {
                "expected failure (xfail)".to_string()
            } else {
                r.skip_reason.clone().unwrap_or_else(|| "skipped".into())
            };
            Some(format!(
                "##teamcity[testIgnored name='{name}' message='{}']",
                tc_escape(&why)
            ))
        }
        _ => return None,
    };
    Some(match middle {
        Some(m) => format!("{started}\n{m}\n{finished}"),
        None => format!("{started}\n{finished}"),
    })
}

/// TeamCity service-message value escaping.
fn tc_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '|' => out.push_str("||"),
            '\'' => out.push_str("|'"),
            '\n' => out.push_str("|n"),
            '\r' => out.push_str("|r"),
            '[' => out.push_str("|["),
            ']' => out.push_str("|]"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(when: &str, outcome: &str) -> Report {
        Report {
            nodeid: "tests/test_a.py::test_x".into(),
            when: when.into(),
            outcome: outcome.into(),
            duration: 1.234,
            longrepr: (outcome == "failed").then(|| "assert 1 == 2\nline two".into()),
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            sections: Vec::new(),
            lineno: None,
        }
    }

    #[test]
    fn tap_lines() {
        let ok = tap_result_line(1, &report("call", "passed")).unwrap();
        assert_eq!(ok, "ok 1 - tests/test_a.py::test_x");
        let fail = tap_result_line(2, &report("call", "failed")).unwrap();
        assert_eq!(fail, "not ok 2 - tests/test_a.py::test_x");
        let mut skip = report("call", "skipped");
        skip.skip_reason = Some("not on linux".into());
        assert_eq!(
            tap_result_line(3, &skip).unwrap(),
            "ok 3 - tests/test_a.py::test_x # SKIP not on linux"
        );
        let mut xfail = report("call", "skipped");
        xfail.wasxfail = true;
        assert_eq!(
            tap_result_line(4, &xfail).unwrap(),
            "not ok 4 - tests/test_a.py::test_x # TODO expected failure"
        );
        let mut xpass = report("call", "passed");
        xpass.wasxfail = true;
        assert_eq!(
            tap_result_line(5, &xpass).unwrap(),
            "ok 5 - tests/test_a.py::test_x # TODO unexpectedly passed"
        );
        assert!(tap_result_line(6, &report("setup", "passed")).is_none());
        assert!(tap_result_line(6, &report("teardown", "passed")).is_none());
    }

    #[test]
    fn teamcity_triplet_and_escaping() {
        let mut fail = report("call", "failed");
        fail.nodeid = "tests/test_a.py::test_x[a'b]".into();
        let msgs = teamcity_messages(&fail).unwrap();
        let lines: Vec<&str> = msgs.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "##teamcity[testStarted name='tests/test_a.py::test_x|[a|'b|]']"
        );
        assert!(lines[1].starts_with("##teamcity[testFailed "));
        assert!(lines[1].contains("details='assert 1 == 2|nline two'"));
        assert_eq!(
            lines[2],
            "##teamcity[testFinished name='tests/test_a.py::test_x|[a|'b|]' duration='1234']"
        );
        let pass = teamcity_messages(&report("call", "passed")).unwrap();
        assert_eq!(pass.lines().count(), 2);
        assert!(teamcity_messages(&report("setup", "passed")).is_none());
    }
}
