//! pytest-style live progress: one status char per test as reports stream
//! in, wrapped with a running percentage when the total is known.

use crate::reporting::color::Palette;
use crate::reporting::sink::Sink;
use crate::reporting::status::StatusFooter;
use crate::scheduling::proto::Report;

/// The pytest outcome a phase report represents, decoupled from how any one
/// renderer draws it. Every renderer (dots, verbose, bar, TAP, TeamCity)
/// classifies through [`outcome_kind`] so xfail/xpass/skip/error stay
/// identical across output formats. `None` from `outcome_kind` means the
/// phase draws nothing (a passed/skipped setup that isn't the decisive
/// phase, or a passed teardown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeKind {
    Pass,
    XPass,
    Fail,
    Skip,
    XFail,
    SetupError,
    TeardownError,
}

impl OutcomeKind {
    /// Whether this phase advances the finished-test counter. Every kind does
    /// except a failed teardown - the test already counted at its call phase.
    fn counts_done(self) -> bool {
        self != OutcomeKind::TeardownError
    }

    /// pytest's single status char (dots / Github / Gitlab / Buildkite / Azure
    /// streams): '.' pass, 'X' xpass, 'F' fail, 's' skip, 'x' xfail, 'E' error.
    fn dot(self) -> char {
        use OutcomeKind::*;
        match self {
            Pass => '.',
            XPass => 'X',
            Fail => 'F',
            Skip => 's',
            XFail => 'x',
            // Failed setup/teardown both draw 'E'.
            SetupError | TeardownError => 'E',
        }
    }

    /// The pytest -v outcome word.
    fn word(self) -> &'static str {
        use OutcomeKind::*;
        match self {
            Pass => "PASSED",
            XPass => "XPASS",
            Fail => "FAILED",
            Skip => "SKIPPED",
            XFail => "XFAIL",
            SetupError | TeardownError => "ERROR",
        }
    }

    /// The sugar-style bar symbol plus its palette color (green pass /
    /// red fail+error / yellow skip+xfail+xpass).
    fn symbol(self) -> (&'static str, fn(&Palette, &str) -> String) {
        use OutcomeKind::*;
        match self {
            Pass => ("✓", Palette::green),
            XPass => ("X", Palette::yellow),
            Fail => ("✗", Palette::red),
            Skip => ("s", Palette::yellow),
            XFail => ("x", Palette::yellow),
            SetupError | TeardownError => ("E", Palette::red),
        }
    }
}

/// Classify a phase report, or `None` when it renders nothing. One char per
/// TEST: the call report, a non-passed setup (no call follows), or a failed
/// teardown (its own marker after the call already printed).
fn outcome_kind(r: &Report) -> Option<OutcomeKind> {
    use OutcomeKind::*;
    Some(match (r.when.as_str(), r.outcome.as_str()) {
        ("call", "passed") => {
            if r.wasxfail {
                XPass
            } else {
                Pass
            }
        }
        ("call", "failed") => Fail,
        ("call" | "setup", "skipped") => {
            if r.wasxfail {
                XFail
            } else {
                Skip
            }
        }
        ("setup", "failed") => SetupError,
        ("teardown", "failed") => TeardownError,
        _ => return None,
    })
}

#[derive(Default, Clone, Copy, PartialEq)]
pub enum Mode {
    #[default]
    Dots,
    /// pytest -v: one `nodeid OUTCOME [ pct%]` line per test.
    Verbose,
    /// pytest-sugar-style: per-test result line inline, plus a live filled
    /// progress bar in the footer. The parallel-safe answer to sugar, which
    /// can't render under a worker pool.
    Bar,
    /// GitHub Actions: dots plus `::error file=,title=,line=::` workflow
    /// annotations per failure (emitted at end-of-run from the aggregate).
    Github,
    /// Newline-delimited JSON: one `testreport` object per phase report,
    /// closed by a `sessionfinish` envelope. stdout is pure NDJSON, no
    /// banner/footer/summary. For editors/tooling.
    Json,
    /// TAP version 13: `ok N - nodeid` per test, failure text as `#`
    /// diagnostics, trailing `1..N` plan. stdout is a pure TAP stream, no
    /// banner or summary. For TAP harnesses (prove, Jenkins TAP plugin).
    Tap,
    /// TeamCity service messages: a `testStarted`/`testFinished` pair per
    /// test (plus `testFailed`/`testIgnored`). Banner and summary stay -
    /// TeamCity ignores non-service lines.
    Teamcity,
    /// GitLab CI: dots plus each failure wrapped in a collapsed
    /// `section_start`/`section_end` block at end-of-run.
    Gitlab,
    /// Buildkite: dots plus each failure under an auto-expanded `+++`
    /// group header at end-of-run.
    Buildkite,
    /// Azure Pipelines: dots plus `##vso[task.logissue ...]` commands per
    /// failure (`type=warning` for flaky-passed), emitted at end-of-run and
    /// surfaced inline on the PR file view.
    Azure,
}

/// Orchestrator-side rendering of the live test stream: the per-test glyph/
/// line output in the selected [`Mode`], plus (in pool mode on a tty) the
/// per-worker [`StatusFooter`].
#[derive(Default)]
pub struct Progress {
    done: usize,
    col: usize,
    total: Option<usize>,
    mode: Mode,
    footer: Option<StatusFooter>,
}

const WIDTH: usize = 72;

impl Progress {
    /// Set the total test count (drives the percentage and the progress bar).
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

    /// Note that `worker` began running `nodeid` (updates the footer's
    /// per-worker current-test line).
    pub fn item_started(&mut self, sink: &mut Sink, worker: usize, nodeid: String) {
        if let Some(f) = &mut self.footer {
            f.item_started(sink.out(), worker, nodeid);
        }
    }

    /// Note that `worker` finished its current test (clears its footer line).
    pub fn item_finished(&mut self, sink: &mut Sink, worker: usize) {
        if let Some(f) = &mut self.footer {
            f.item_finished(sink.out(), worker);
        }
    }

    /// Repaint the footer's elapsed timers between reports (tty only).
    pub fn tick(&mut self, sink: &mut Sink) {
        if let Some(f) = &mut self.footer {
            f.tick(sink.out());
        }
    }

    fn out_inline(&mut self, sink: &mut Sink, text: &str) {
        match &mut self.footer {
            Some(f) => f.print_inline(sink.out(), text),
            None => sink.out_inline(text),
        }
    }

    fn out_line(&mut self, sink: &mut Sink, text: &str) {
        match &mut self.footer {
            Some(f) => f.print_line(sink.out(), text),
            None => sink.out_line(text),
        }
    }

    /// The running-percentage suffix ` [ NN%]`, clamped to 100 — reruns push
    /// `done` past `total`, so an unclamped `done*100/t` prints `[103%]`. The
    /// ONE clamp site for every renderer's percentage. `None` when the total is
    /// unknown, so each caller supplies its own no-total fallback.
    fn pct_suffix(&self) -> Option<String> {
        match self.total {
            Some(t) if t > 0 => Some(format!(" [{:3}%]", (self.done * 100 / t).min(100))),
            _ => None,
        }
    }

    /// Select the output style (dots/verbose/bar/json/…).
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if let Some(f) = &mut self.footer {
            f.set_bar(mode == Mode::Bar);
        }
    }

    /// pytest's char per outcome: '.' pass, 'F' fail, 's' skip, 'x' xfail,
    /// 'X' xpass, 'E' setup/teardown error. One char per TEST: on the call
    /// report, a non-passed setup (no call follows), or a failed teardown.
    pub fn on_report(&mut self, sink: &mut Sink, worker: Option<usize>, r: &Report) {
        if self.mode == Mode::Json {
            return Self::on_report_json(sink, worker, r);
        }
        if self.mode == Mode::Tap {
            return self.on_report_tap(sink, r);
        }
        if self.mode == Mode::Teamcity {
            return self.on_report_teamcity(sink, r);
        }
        if self.mode == Mode::Verbose {
            return self.on_report_verbose(sink, worker, r);
        }
        if self.mode == Mode::Bar {
            return self.on_report_bar(sink, worker, r);
        }
        let palette = sink.palette();
        // Github/Gitlab/Buildkite share the dots char stream below; their
        // annotations / fold markers are emitted from the aggregate at
        // end-of-run.
        let Some(kind) = outcome_kind(r) else {
            return;
        };
        if kind.counts_done() {
            self.done += 1;
        }
        let painted = palette.outcome(&kind.dot().to_string());
        self.out_inline(sink, &painted);
        self.col += 1;
        if self.col >= WIDTH {
            self.col = 0;
            // No total yet: show the raw done count instead of a percentage.
            let tail = self
                .pct_suffix()
                .unwrap_or_else(|| format!(" [{}]", self.done));
            self.out_line(sink, &tail);
        }
    }

    /// pytest -v: `nodeid OUTCOME [ pct%]` per test, ERROR lines for
    /// failed setup/teardown phases.
    fn on_report_verbose(&mut self, sink: &mut Sink, worker: Option<usize>, r: &Report) {
        let Some(kind) = outcome_kind(r) else {
            return;
        };
        if kind.counts_done() {
            self.done += 1;
        }
        let pct = self.pct_suffix().unwrap_or_default();
        let prefix = worker.map(|w| format!("[gw{w}] ")).unwrap_or_default();
        let line = format!(
            "{prefix}{} {}{pct}",
            r.nodeid,
            sink.palette().outcome(kind.word())
        );
        self.out_line(sink, &line);
    }

    /// pytest-sugar-style per-test line: `<sym> nodeid  dur [pct%]`, with
    /// the failure repr inlined right under a failing test. Symbol colored
    /// by outcome (green pass / red fail+error / yellow skip+xfail+xpass).
    fn on_report_bar(&mut self, sink: &mut Sink, worker: Option<usize>, r: &Report) {
        let Some(kind) = outcome_kind(r) else {
            return;
        };
        let (sym, color) = kind.symbol();
        if kind.counts_done() {
            self.done += 1;
        }
        let pct = self.pct_suffix().unwrap_or_default();
        let dur = if r.duration >= 0.0005 {
            format!("  {:.2}s", r.duration)
        } else {
            String::new()
        };
        let prefix = worker.map(|w| format!("[gw{w}] ")).unwrap_or_default();
        let palette = sink.palette(); // Copy - avoids borrowing sink during out_line
        let painted_sym = color(&palette, sym);
        let meta = format!("{dur}{pct}");
        let tail = if meta.is_empty() {
            String::new()
        } else {
            palette.dim(&meta)
        };
        self.out_line(sink, &format!("{prefix}{painted_sym} {}{tail}", r.nodeid));
        // Sugar shows failures the moment they happen - inline the repr.
        if r.outcome == "failed" {
            if let Some(repr) = &r.longrepr {
                let header = palette.bold_red(&format!("  ── {} ──", r.nodeid));
                self.out_line(sink, &header);
                for l in repr.trim_end().lines() {
                    self.out_line(sink, &format!("  {l}"));
                }
            }
        }
    }

    /// One TAP test point per test as it finishes; failure text follows as
    /// `#` diagnostic lines. The trailing plan comes from [`tap_plan`] so
    /// the count always matches the points emitted.
    fn on_report_tap(&mut self, sink: &mut Sink, r: &Report) {
        let Some(line) = tap_result_line(self.done + 1, r) else {
            return;
        };
        self.done += 1;
        sink.out_line(&line);
        if r.outcome == "failed" {
            if let Some(repr) = &r.longrepr {
                for l in repr.trim_end().lines() {
                    sink.out_line(&format!("# {l}"));
                }
            }
        }
    }

    /// Close a TAP stream: the trailing `1..N` plan (valid TAP when the
    /// plan comes last), N = test points actually emitted.
    pub fn tap_plan(&self, sink: &mut Sink) {
        sink.out_line(&format!("1..{}", self.done));
    }

    /// One TeamCity service-message group per test. Retroactive
    /// `testStarted`/`testFinished` pairs are fine (duration rides on the
    /// attribute); emitting the group at once avoids parallel interleaving.
    fn on_report_teamcity(&mut self, sink: &mut Sink, r: &Report) {
        let Some(messages) = teamcity_messages(r) else {
            return;
        };
        if r.when != "teardown" {
            self.done += 1;
        }
        sink.out_line(&messages);
    }

    /// One NDJSON object per phase report, straight to stdout (no footer in
    /// Json mode). `longrepr` rides only on failures (it's large); `worker`
    /// only in pool runs.
    fn on_report_json(sink: &mut Sink, worker: Option<usize>, r: &Report) {
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
        sink.out_line(&obj.to_string());
    }

    /// Close the dot line before failures/summary print.
    pub fn finish(&mut self, sink: &mut Sink) {
        if let Some(f) = &mut self.footer {
            f.finish(sink.out());
        }
        if matches!(
            self.mode,
            Mode::Verbose | Mode::Bar | Mode::Tap | Mode::Teamcity
        ) {
            return;
        }
        if self.col > 0 {
            match self.pct_suffix() {
                Some(tail) => sink.out_line(&tail),
                None => sink.out_line(""),
            }
        }
    }
}

/// The TAP test point for a phase report, or None when it emits nothing.
/// xfail = `not ok # TODO`, xpass = `ok # TODO`, skip = `ok # SKIP`. A
/// failed teardown gets its own point so the plan matches points emitted.
fn tap_result_line(n: usize, r: &Report) -> Option<String> {
    use OutcomeKind::*;
    let kind = outcome_kind(r)?;
    let directive = |tag: &str, reason: Option<&str>| match reason {
        Some(why) if !why.is_empty() => format!(" # {tag} {}", why.replace(['\n', '\r'], " ")),
        _ => format!(" # {tag}"),
    };
    let line = match kind {
        Pass => format!("ok {n} - {}", r.nodeid),
        XPass => format!(
            "ok {n} - {}{}",
            r.nodeid,
            directive("TODO", Some("unexpectedly passed"))
        ),
        Fail => format!("not ok {n} - {}", r.nodeid),
        XFail => format!(
            "not ok {n} - {}{}",
            r.nodeid,
            directive("TODO", Some("expected failure"))
        ),
        Skip => format!(
            "ok {n} - {}{}",
            r.nodeid,
            directive("SKIP", r.skip_reason.as_deref())
        ),
        SetupError => format!("not ok {n} - {} # setup error", r.nodeid),
        TeardownError => format!("not ok {n} - {} # teardown error", r.nodeid),
    };
    Some(line)
}

/// The TeamCity service-message group for a phase report, or None when it
/// emits nothing. `testStarted` precedes every result so output attributes
/// correctly; duration rides on `testFinished` in milliseconds.
fn teamcity_messages(r: &Report) -> Option<String> {
    use OutcomeKind::*;
    let kind = outcome_kind(r)?;
    let name = tc_escape(&r.nodeid);
    let started = format!("##teamcity[testStarted name='{name}']");
    let finished = format!(
        "##teamcity[testFinished name='{name}' duration='{}']",
        (r.duration * 1000.0).round() as u64
    );
    let middle = match kind {
        // xpass rides through as a plain pass (TeamCity has no xpass concept).
        Pass | XPass => None,
        Fail | SetupError | TeardownError => {
            let details = r.longrepr.as_deref().unwrap_or("");
            Some(format!(
                "##teamcity[testFailed name='{name}' message='{} failed' details='{}']",
                r.when,
                tc_escape(details)
            ))
        }
        Skip | XFail => {
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
    };
    Some(match middle {
        Some(m) => format!("{started}\n{m}\n{finished}"),
        None => format!("{started}\n{finished}"),
    })
}

/// TeamCity WARNING build messages for tests that passed only after reruns.
/// The run stays green; these surface the flake as build-log warnings.
/// Empty when nothing flaked.
pub fn teamcity_flaky_messages(flaky: &[(String, u32)]) -> String {
    flaky
        .iter()
        .map(|(nodeid, attempts)| {
            format!(
                "##teamcity[message text='flaky: {} passed only after {attempts} rerun{}' status='WARNING']",
                tc_escape(nodeid),
                if *attempts > 1 { "s" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
            thread_delta: None,
            fd_delta: None,
            sections: Vec::new(),
            lineno: None,
        }
    }

    #[test]
    fn pct_suffix_clamps_and_reports_no_total() {
        let mut p = Progress::default();
        // No total: no percentage (caller supplies its own fallback).
        assert_eq!(p.pct_suffix(), None);
        p.set_total(4);
        p.done = 2;
        assert_eq!(p.pct_suffix().as_deref(), Some(" [ 50%]"));
        // Reruns can push done past total; every renderer must clamp to 100,
        // never print [150%] (the dots mid-run tail used to skip this clamp).
        p.done = 6;
        assert_eq!(p.pct_suffix().as_deref(), Some(" [100%]"));
    }

    #[test]
    fn dots_tail_clamps_at_line_wrap() {
        // Drive the dots stream past a wrap with done > total (reruns) and
        // assert the wrapped `[NN%]` tail never exceeds 100%.
        let mut p = Progress::default();
        p.set_total(1);
        let (mut sink, buf) = Sink::captured();
        for _ in 0..WIDTH {
            p.on_report(&mut sink, None, &report("call", "passed"));
        }
        let out = buf.out();
        assert!(out.contains(" [100%]"), "{out}");
        // No three-digit-over-100 percentage leaked (unclamped would be 7200%).
        for bad in [" [101%]", " [102%]", " [150%]", " [7200%]"] {
            assert!(!out.contains(bad), "leaked {bad}: {out}");
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

    #[test]
    fn teamcity_flaky_warns_per_test() {
        assert_eq!(teamcity_flaky_messages(&[]), "");
        let flaky = vec![
            ("tests/test_a.py::test_x[a]".to_string(), 1u32),
            ("tests/test_b.py::test_y".to_string(), 3u32),
        ];
        let msgs = teamcity_flaky_messages(&flaky);
        let lines: Vec<&str> = msgs.lines().collect();
        assert_eq!(
            lines[0],
            "##teamcity[message text='flaky: tests/test_a.py::test_x|[a|] passed only after 1 rerun' status='WARNING']"
        );
        assert!(lines[1].contains("passed only after 3 reruns' status='WARNING'"));
    }
}
