//! pytest-style live progress: one status char per test as reports stream
//! in, wrapped with a running percentage when the total is known.

use crate::reporting::color::Palette;
use crate::reporting::report::{trim_message, wcswidth, TbStyle};
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
    /// A failed subtest: pytest's `u` / `SUBFAILED`.
    SubFail,
    /// A passed, skipped or xfailed subtest pytest shows (see
    /// [`Report::subtest_shown`]): `SUBPASSED` / `SUBSKIPPED` / `SUBXFAIL`.
    SubPass,
    SubSkip,
    SubXFail,
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

    fn is_subtest(self) -> bool {
        use OutcomeKind::*;
        matches!(self, SubFail | SubPass | SubSkip | SubXFail)
    }

    /// pytest's single status char (dots / Github / Gitlab / Buildkite / Azure
    /// streams): '.' pass, 'X' xpass, 'F' fail, 's' skip, 'x' xfail, 'E' error.
    fn dot(self) -> char {
        use OutcomeKind::*;
        match self {
            Pass => '.',
            XPass => 'X',
            Fail => 'F',
            SubFail | SubPass => 'u',
            SubSkip => '-',
            SubXFail => 'y',
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
            SubFail => "SUBFAILED",
            SubPass => "SUBPASSED",
            SubSkip => "SUBSKIPPED",
            SubXFail => "SUBXFAIL",
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
            Pass | SubPass => ("✓", Palette::green),
            XPass => ("X", Palette::yellow),
            Fail | SubFail => ("✗", Palette::red),
            Skip | SubSkip => ("s", Palette::yellow),
            XFail | SubXFail => ("x", Palette::yellow),
            SetupError | TeardownError => ("E", Palette::red),
        }
    }
}

/// Classify a phase report, or `None` when it renders nothing. One char per
/// TEST: the call report, a non-passed setup (no call follows), or a failed
/// teardown (its own marker after the call already printed).
fn outcome_kind(r: &Report) -> Option<OutcomeKind> {
    use OutcomeKind::*;
    // A failed subtest shows as pytest's `u`; passing/skipped ones only
    // when pytest shows them too (`verbosity_subtests`, -v).
    if r.subtest {
        return match (r.outcome.as_str(), r.wasxfail) {
            ("failed", _) => Some(SubFail),
            _ if !r.subtest_shown => None,
            ("skipped", true) => Some(SubXFail),
            ("passed", false) => Some(SubPass),
            ("skipped", false) => Some(SubSkip),
            _ => None,
        };
    }
    // Any phase but setup/teardown is the test itself, as pytest shows it
    // (pytest-forked reports a crashed child as `when="???"`).
    let when = match r.when.as_str() {
        w @ ("setup" | "teardown") => w,
        _ => "call",
    };
    Some(match (when, r.outcome.as_str()) {
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
    /// Byte-exact mode's default: the single pytest session writes its own
    /// terminal output to the inherited stdout, so rstest renders nothing
    /// live and no summary. Only chosen when no `--output` is set.
    Pytest,
}

/// Orchestrator-side rendering of the live test stream: the per-test glyph/
/// line output in the selected [`Mode`], plus (in pool mode on a tty) the
/// per-worker [`StatusFooter`].
#[derive(Default)]
pub struct Progress {
    done: usize,
    /// Tests already counted by a failed subtest, awaiting their own report.
    subtest_counted: std::collections::HashSet<String>,
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

    /// The total test count, once known.
    pub fn total(&self) -> Option<usize> {
        self.total
    }

    /// Enable the live per-worker status footer (pool mode). `live` is the
    /// palette's interactive-terminal decision; the footer is a no-op
    /// without it.
    pub fn enable_footer(&mut self, workers: usize, live: bool) {
        let mut footer = StatusFooter::new(workers, live);
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

    /// Advance the finished-test counter once per test, as pytest counts
    /// nodeids: a test's first failed subtest counts it, and neither its
    /// later subtests nor its own report count it again.
    fn count(&mut self, kind: OutcomeKind, r: &Report) {
        let first = match kind {
            k if k.is_subtest() => self.subtest_counted.insert(r.nodeid.clone()),
            _ => kind.counts_done() && !self.subtest_counted.remove(&r.nodeid),
        };
        if first {
            self.done += 1;
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
        if self.mode == Mode::Pytest {
            return;
        }
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
        self.count(kind, r);
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
        self.count(kind, r);
        let pct = self.pct_suffix().unwrap_or_default();
        let prefix = worker.map(|w| format!("[gw{w}] ")).unwrap_or_default();
        // pytest names the subtest: `SUBFAILED(i=1)`.
        let desc = r.subtest_desc.as_deref().unwrap_or_default();
        let word = format!("{}{desc}", kind.word());
        // pytest: ` (reason)` after SKIPPED / XFAIL / XPASS, trimmed at -v to
        // what fits before the ` [100%]` column, whole at -vv. Measured
        // without the `[gwN] ` prefix so the text matches pytest's.
        let plain = format!("{} {word}", r.nodeid);
        let reason = skip_reason(r)
            .filter(|reason| !reason.is_empty())
            .and_then(|reason| {
                if sink.very_verbose() {
                    return Some(reason.to_string());
                }
                let used = wcswidth(&plain).unwrap_or(plain.chars().count());
                let available =
                    sink.columns() as isize - used as isize - " [100%]".len() as isize - 1;
                trim_message(reason, available)
            })
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default();
        let line = format!(
            "{prefix}{} {}{reason}{pct}",
            r.nodeid,
            sink.palette().outcome(&word)
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
        self.count(kind, r);
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
        // `--tb=no` shows no failure text at all (as pytest).
        if r.outcome == "failed" && sink.tb_style() != TbStyle::No {
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

    /// Close a TAP stream: a `not ok` point per collection error (its
    /// traceback as `#` diagnostics), then the trailing `1..N` plan (valid TAP
    /// when the plan comes last), N = test points actually emitted. Without
    /// the collect-error points a suite whose only module fails to import
    /// would close as `1..0`, which TAP consumers read as skip-all (green).
    pub fn tap_plan(&mut self, sink: &mut Sink, collect_errors: &[(String, String)]) {
        for (path, longrepr) in collect_errors {
            self.done += 1;
            sink.out_line(&format!(
                "not ok {} - {} # collection error",
                self.done, path
            ));
            for l in longrepr.trim_end().lines() {
                sink.out_line(&format!("# {l}"));
            }
        }
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
        sink.out_line(&testreport_json(worker, r).to_string());
    }

    /// Stream a `collecterror` line to stdout, but only under `--output json`
    /// (other modes render collection errors in the end-of-run summary, so
    /// emitting mid-stream would corrupt their human output). The `--stream-json`
    /// side channel gets the same event independently via
    /// [`Sink::emit_collect_error`]. No-op in every non-json mode.
    pub fn on_collect_error(&self, sink: &mut Sink, path: &str, longrepr: &str) {
        if self.mode == Mode::Json {
            sink.out_line(&collecterror_json(path, longrepr).to_string());
        }
    }

    /// Close the dot line before failures/summary print.
    pub fn finish(&mut self, sink: &mut Sink) {
        if let Some(f) = &mut self.footer {
            f.finish(sink.out());
        }
        if matches!(
            self.mode,
            Mode::Verbose | Mode::Bar | Mode::Tap | Mode::Teamcity | Mode::Pytest
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

/// Serialize one phase [`Report`] as the streaming-JSON `testreport` object —
/// the single source of truth for that shape, shared by `--output json`
/// (stdout) and `--stream-json` (side channel), so both stay byte-identical.
/// `lineno`/`worker` are omitted when absent; `longrepr` rides failures only.
pub(crate) fn testreport_json(worker: Option<usize>, r: &Report) -> serde_json::Value {
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
    // Call-phase CPU time (process_time), present when measured (`--doctor` or a
    // stream consumer). wall (`duration`) ≫ `cpu` ⇒ the test waited (sleep/IO)
    // rather than computed — the inline wait-bound signal for editors. Setup /
    // teardown reports also carry CPU for the doctor; the stream keeps its
    // documented call-phase-only shape.
    if let Some(c) = r.cpu.filter(|_| r.when == "call") {
        obj["cpu"] = c.into();
    }
    if r.outcome == "failed" {
        if let Some(lr) = &r.longrepr {
            obj["longrepr"] = lr.as_str().into();
        }
    }
    // Captured stdout/stderr/log, present when a stream consumer requested it
    // (`--output json` / `--stream-json` set RSTEST_STREAM_OUTPUT) or on a
    // failure. Omitted when empty so quiet tests stay one compact line.
    if !r.sections.is_empty() {
        obj["sections"] = r
            .sections
            .iter()
            .map(|(name, text)| serde_json::json!({"name": name, "text": text}))
            .collect::<Vec<_>>()
            .into();
    }
    obj
}

/// Serialize a collection/import failure as the streaming-JSON `collecterror`
/// object — shared by `--output json` (stdout) and `--stream-json` (side
/// channel) so a Test Explorer can mark a failing file red mid-run instead of
/// waiting for the end-of-run snapshot. `path` is the failing collector
/// (rootdir-relative file / nodeid); `longrepr` is the traceback.
pub(crate) fn collecterror_json(path: &str, longrepr: &str) -> serde_json::Value {
    serde_json::json!({
        "event": "collecterror",
        "path": path,
        "longrepr": longrepr,
    })
}

/// pytest's `_get_raw_skip_reason`: the user's part of a skip/xfail/xpass
/// reason, without the `Skipped: ` / `reason: ` prefixes pytest adds.
fn skip_reason(r: &Report) -> Option<&str> {
    if let Some(reason) = r.xfail_reason.as_deref() {
        return Some(reason.strip_prefix("reason: ").unwrap_or(reason));
    }
    if r.outcome != "skipped" {
        return None;
    }
    let reason = r.skip_reason.as_deref()?;
    Some(match reason.strip_prefix("Skipped: ") {
        Some(rest) => rest,
        None if reason == "Skipped" => "",
        None => reason,
    })
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
        Fail | SubFail => format!("not ok {n} - {}", r.nodeid),
        SubPass | SubSkip | SubXFail => return None,
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
        Fail | SubFail | SetupError | TeardownError => {
            let details = r.longrepr.as_deref().unwrap_or("");
            Some(format!(
                "##teamcity[testFailed name='{name}' message='{} failed' details='{}']",
                r.when,
                tc_escape(details)
            ))
        }
        // Not a test of its own: the parent's report carries the verdict.
        SubPass | SubSkip | SubXFail => return None,
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

/// TeamCity messages for collection errors: a failed pseudo-test per broken
/// collector (as the teamcity-messages pytest plugin reports them), so a
/// module that cannot import fails the build's test view instead of only
/// the exit code. Empty when collection was clean.
pub fn teamcity_collect_error_messages(errors: &[(String, String)]) -> String {
    errors
        .iter()
        .map(|(path, longrepr)| {
            let name = tc_escape(path);
            format!(
                "##teamcity[testStarted name='{name}']\n\
                 ##teamcity[testFailed name='{name}' message='collection error' details='{}']\n\
                 ##teamcity[testFinished name='{name}' duration='0']",
                tc_escape(longrepr)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        }
    }

    #[test]
    fn a_plugin_phase_shows_as_the_test_outcome() {
        // pytest-forked's crash report has `when="???"`: a failure, as pytest shows it.
        assert!(matches!(
            outcome_kind(&report("???", "failed")),
            Some(OutcomeKind::Fail)
        ));
    }

    #[test]
    fn only_failed_subtests_show_in_progress() {
        let sub = |outcome| Report {
            subtest: true,
            crash: None,
            network: Vec::new(),
            marker_timeout: None,
            no_rerun: false,
            subtest_desc: None,
            subtest_shown: false,
            ..report("call", outcome)
        };
        assert!(outcome_kind(&sub("passed")).is_none());
        assert!(outcome_kind(&sub("skipped")).is_none());
        assert!(matches!(
            outcome_kind(&sub("failed")),
            Some(OutcomeKind::SubFail)
        ));
    }

    #[test]
    fn a_failed_subtest_draws_u_and_counts_its_test_once_like_pytest() {
        let sub = |desc: &str| Report {
            subtest: true,
            subtest_desc: Some(desc.into()),
            ..report("call", "failed")
        };
        let (mut sink, buf) = Sink::captured();
        let mut p = Progress::default();
        p.set_total(2);
        p.on_report(&mut sink, None, &sub("(i=1)"));
        p.on_report(&mut sink, None, &sub("(i=2)"));
        p.on_report(&mut sink, None, &report("call", "failed"));
        assert_eq!(buf.out(), "uuF");
        assert_eq!(p.done, 1, "one test, counted once");

        // -v: the subtest's line is named and sits at the test's percentage.
        let (mut sink, buf) = Sink::captured();
        let mut p = Progress::default();
        p.set_mode(Mode::Verbose);
        p.set_total(2);
        p.on_report(&mut sink, None, &sub("(i=1)"));
        p.on_report(&mut sink, None, &report("call", "failed"));
        assert_eq!(
            buf.out(),
            "tests/test_a.py::test_x SUBFAILED(i=1) [ 50%]\n\
             tests/test_a.py::test_x FAILED [ 50%]\n"
        );
    }

    #[test]
    fn shown_subtests_draw_like_pytest_at_verbose() {
        // verbosity_subtests > 0 (-v): pytest prints passed and skipped
        // subtests too, at the test's percentage; the test counts once.
        let sub = |outcome, desc: &str| Report {
            subtest: true,
            subtest_shown: true,
            subtest_desc: Some(desc.into()),
            ..report("call", outcome)
        };
        let (mut sink, buf) = Sink::captured();
        let mut p = Progress::default();
        p.set_mode(Mode::Verbose);
        p.set_total(2);
        p.on_report(&mut sink, None, &sub("passed", "(i=0)"));
        p.on_report(&mut sink, None, &sub("skipped", "[sk]"));
        p.on_report(&mut sink, None, &report("call", "passed"));
        assert_eq!(
            buf.out(),
            "tests/test_a.py::test_x SUBPASSED(i=0) [ 50%]\n\
             tests/test_a.py::test_x SUBSKIPPED[sk] [ 50%]\n\
             tests/test_a.py::test_x PASSED [ 50%]\n"
        );
        assert_eq!(p.done, 1);
        // Dots: pytest's `u` and `-`. TAP: not test points of their own.
        let (mut sink, buf) = Sink::captured();
        let mut p = Progress::default();
        p.on_report(&mut sink, None, &sub("passed", ""));
        p.on_report(&mut sink, None, &sub("skipped", ""));
        assert_eq!(buf.out(), "u-");
        assert!(tap_result_line(1, &sub("passed", "")).is_none());
        assert!(teamcity_messages(&sub("passed", "")).is_none());
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
    fn testreport_json_includes_sections_when_present_and_omits_when_empty() {
        // Empty sections → no `sections` key (quiet tests stay compact).
        let quiet = testreport_json(Some(0), &report("call", "passed"));
        assert!(quiet.get("sections").is_none());

        // Populated sections (a stream consumer asked for output) → array of
        // {name, text}.
        let mut r = report("call", "passed");
        r.sections = vec![("Captured stdout call".into(), "hi\n".into())];
        let obj = testreport_json(Some(0), &r);
        assert_eq!(obj["sections"][0]["name"], "Captured stdout call");
        assert_eq!(obj["sections"][0]["text"], "hi\n");
    }

    #[test]
    fn on_report_in_json_mode_writes_a_testreport_to_stdout() {
        // The Mode::Json dispatch renders one NDJSON testreport line on stdout
        // (the --output json live stream), via the shared serializer.
        let mut p = Progress::default();
        p.set_mode(Mode::Json);
        let (mut sink, buf) = Sink::captured();
        p.on_report(&mut sink, Some(2), &report("call", "passed"));
        let obj: serde_json::Value = serde_json::from_str(buf.out().trim()).unwrap();
        assert_eq!(obj["event"], "testreport");
        assert_eq!(obj["outcome"], "passed");
        assert_eq!(obj["worker"], "gw2");
    }

    #[test]
    fn testreport_json_includes_cpu_only_when_measured() {
        // Not measured (plain run) → no `cpu` key.
        assert!(testreport_json(None, &report("call", "passed"))
            .get("cpu")
            .is_none());
        // Measured → cpu present (wall ≫ cpu is the wait-bound signal).
        let mut r = report("call", "passed");
        r.cpu = Some(0.0001);
        assert_eq!(testreport_json(None, &r)["cpu"], 0.0001);
    }

    #[test]
    fn on_collect_error_streams_json_only_in_json_mode() {
        // Json mode: a collecterror line lands on stdout.
        let mut p = Progress::default();
        p.set_mode(Mode::Json);
        let (mut sink, buf) = Sink::captured();
        p.on_collect_error(&mut sink, "tests/test_x.py", "ImportError: boom");
        let obj: serde_json::Value = serde_json::from_str(buf.out().trim()).unwrap();
        assert_eq!(obj["event"], "collecterror");
        assert_eq!(obj["path"], "tests/test_x.py");

        // Any other mode: nothing on stdout (the summary renders it at the end).
        let mut p = Progress::default();
        p.set_mode(Mode::Dots);
        let (mut sink, buf) = Sink::captured();
        p.on_collect_error(&mut sink, "tests/test_x.py", "boom");
        assert_eq!(buf.out(), "");
    }

    #[test]
    fn tap_plan_counts_collect_errors_as_failed_points() {
        let mut p = Progress::default();
        p.set_mode(Mode::Tap);
        let (mut sink, buf) = Sink::captured();
        p.on_report(&mut sink, None, &report("call", "passed"));
        let errs = vec![(
            "tests/test_c.py".to_string(),
            "Traceback:\nE   ModuleNotFoundError: No module named 'nope'\n".to_string(),
        )];
        p.tap_plan(&mut sink, &errs);
        assert_eq!(
            buf.out(),
            "ok 1 - tests/test_a.py::test_x\n\
             not ok 2 - tests/test_c.py # collection error\n\
             # Traceback:\n\
             # E   ModuleNotFoundError: No module named 'nope'\n\
             1..2\n"
        );
        // Clean collection: just the plan.
        let mut p = Progress::default();
        let (mut sink, buf) = Sink::captured();
        p.tap_plan(&mut sink, &[]);
        assert_eq!(buf.out(), "1..0\n");
    }

    #[test]
    fn teamcity_collect_errors_fail_a_pseudo_test() {
        assert_eq!(teamcity_collect_error_messages(&[]), "");
        let msgs = teamcity_collect_error_messages(&[("t/x.py".into(), "E   boom\n[x]".into())]);
        assert_eq!(
            msgs,
            "##teamcity[testStarted name='t/x.py']\n\
             ##teamcity[testFailed name='t/x.py' message='collection error' details='E   boom|n|[x|]']\n\
             ##teamcity[testFinished name='t/x.py' duration='0']"
        );
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
    fn json_report_emits_sections_on_any_outcome() {
        // Passing report with captured output: sections ride as {name, text}
        // objects even though the outcome isn't a failure.
        let mut r = report("call", "passed");
        r.sections = vec![
            ("Captured stdout".into(), "hello\nworld".into()),
            ("Captured stderr".into(), "boom".into()),
        ];
        let (mut sink, buf) = Sink::captured();
        Progress::on_report_json(&mut sink, Some(1), &r);
        let obj: serde_json::Value =
            serde_json::from_str(buf.out().trim()).expect("valid json line");
        assert_eq!(obj["outcome"], "passed");
        assert_eq!(obj["worker"], "gw1");
        assert_eq!(
            obj["sections"],
            serde_json::json!([
                {"name": "Captured stdout", "text": "hello\nworld"},
                {"name": "Captured stderr", "text": "boom"},
            ])
        );
    }

    #[test]
    fn json_report_omits_sections_when_empty() {
        let (mut sink, buf) = Sink::captured();
        Progress::on_report_json(&mut sink, None, &report("call", "passed"));
        let obj: serde_json::Value =
            serde_json::from_str(buf.out().trim()).expect("valid json line");
        assert!(obj.get("sections").is_none(), "{obj}");
        // Non-failure keeps longrepr off the wire too.
        assert!(obj.get("longrepr").is_none(), "{obj}");
    }

    fn xfailed(mut r: Report) -> Report {
        r.wasxfail = true;
        r
    }

    /// One report per outcome kind, plus the phases that draw nothing.
    fn every_kind() -> Vec<Report> {
        vec![
            report("setup", "passed"),
            report("call", "passed"),
            xfailed(report("call", "passed")),
            report("call", "failed"),
            report("call", "skipped"),
            xfailed(report("setup", "skipped")),
            report("setup", "failed"),
            report("teardown", "failed"),
            report("teardown", "passed"),
        ]
    }

    #[test]
    fn dots_draw_one_char_per_outcome_and_close_with_the_percentage() {
        let mut p = Progress::default();
        p.set_total(6);
        assert_eq!(p.total(), Some(6));
        let (mut sink, buf) = Sink::captured();
        for r in every_kind() {
            p.on_report(&mut sink, None, &r);
        }
        // A failed teardown draws 'E' but its test was already counted.
        assert_eq!(p.done, 6);
        p.finish(&mut sink);
        assert_eq!(buf.out(), ".XFsxEE [100%]\n");

        // No total: the line just ends.
        let mut p = Progress::default();
        let (mut sink, buf) = Sink::captured();
        p.on_report(&mut sink, None, &report("call", "passed"));
        p.finish(&mut sink);
        assert_eq!(buf.out(), ".\n");
        // Nothing drawn: finish adds nothing.
        let (mut sink, buf) = Sink::captured();
        Progress::default().finish(&mut sink);
        assert_eq!(buf.out(), "");
    }

    #[test]
    fn dots_wrap_with_a_raw_count_when_the_total_is_unknown() {
        let mut p = Progress::default();
        let (mut sink, buf) = Sink::captured();
        for _ in 0..WIDTH + 1 {
            p.on_report(&mut sink, None, &report("call", "passed"));
        }
        p.finish(&mut sink);
        assert_eq!(buf.out(), format!("{} [{WIDTH}]\n.\n", ".".repeat(WIDTH)));
    }

    #[test]
    fn verbose_lines_name_the_worker_and_outcome_word() {
        let mut p = Progress::default();
        p.set_mode(Mode::Verbose);
        let (mut sink, buf) = Sink::captured();
        for r in every_kind() {
            p.on_report(&mut sink, Some(1), &r);
        }
        p.finish(&mut sink);
        let id = "tests/test_a.py::test_x";
        let want: String = [
            "PASSED", "XPASS", "FAILED", "SKIPPED", "XFAIL", "ERROR", "ERROR",
        ]
        .iter()
        .map(|w| format!("[gw1] {id} {w}\n"))
        .collect();
        assert_eq!(buf.out(), want, "no total: no percentage, no closing line");
    }

    #[test]
    fn verbose_lines_show_skip_and_xfail_reasons_as_pytest() {
        let skipped = |reason: &str| Report {
            skip_reason: Some(reason.into()),
            ..report("call", "skipped")
        };
        let xfail = |outcome, reason: &str| Report {
            xfail_reason: Some(reason.into()),
            ..xfailed(report("call", outcome))
        };
        let long = "x".repeat(50);
        let reports = [
            skipped("Skipped: no db"),
            skipped("Skipped"), // bare pytest.skip(): no reason shown
            xfail("skipped", "reason: upstream bug"),
            xfail("passed", "fixed?"),
            skipped(&format!("Skipped: {long}")),
            skipped("Skipped: first\nsecond"),
        ];
        let render = |very_verbose| {
            let mut p = Progress::default();
            p.set_mode(Mode::Verbose);
            let (mut sink, buf) = Sink::captured(); // 80 columns
            sink.set_very_verbose(very_verbose);
            for r in &reports {
                p.on_report(&mut sink, Some(1), r);
            }
            buf.out()
        };
        let id = "[gw1] tests/test_a.py::test_x";
        // -v: 80 - len("<nodeid> SKIPPED") - len(" [100%]") - 1 = 41 columns,
        // the `[gw1] ` prefix left out as pytest has none.
        let trimmed = format!("{}...", "x".repeat(35));
        assert_eq!(
            render(false),
            format!(
                "{id} SKIPPED (no db)\n\
                 {id} SKIPPED\n\
                 {id} XFAIL (upstream bug)\n\
                 {id} XPASS (fixed?)\n\
                 {id} SKIPPED ({trimmed})\n\
                 {id} SKIPPED (first)\n"
            )
        );
        // -vv: whole reasons.
        assert_eq!(
            render(true),
            format!(
                "{id} SKIPPED (no db)\n\
                 {id} SKIPPED\n\
                 {id} XFAIL (upstream bug)\n\
                 {id} XPASS (fixed?)\n\
                 {id} SKIPPED ({long})\n\
                 {id} SKIPPED (first\nsecond)\n"
            )
        );
    }

    #[test]
    fn bar_lines_carry_symbol_duration_percentage_and_inline_failure() {
        let mut p = Progress::default();
        p.set_mode(Mode::Bar);
        p.set_total(7);
        let (mut sink, buf) = Sink::captured();
        for r in every_kind() {
            p.on_report(&mut sink, None, &r);
        }
        p.finish(&mut sink);
        let id = "tests/test_a.py::test_x";
        let fail_body = format!("  ── {id} ──\n  assert 1 == 2\n  line two\n");
        assert_eq!(
            buf.out(),
            format!(
                "✓ {id}  1.23s [ 14%]\n\
                 X {id}  1.23s [ 28%]\n\
                 ✗ {id}  1.23s [ 42%]\n{fail_body}\
                 s {id}  1.23s [ 57%]\n\
                 x {id}  1.23s [ 71%]\n\
                 E {id}  1.23s [ 85%]\n{fail_body}\
                 E {id}  1.23s [ 85%]\n{fail_body}"
            )
        );

        // No total, no measurable duration, `--tb=no`: bare line, no repr.
        let mut p = Progress::default();
        p.set_mode(Mode::Bar);
        let (mut sink, buf) = Sink::captured();
        sink.set_tb_style(TbStyle::No);
        let mut r = report("call", "failed");
        r.duration = 0.0001;
        p.on_report(&mut sink, Some(3), &r);
        assert_eq!(buf.out(), format!("[gw3] ✗ {id}\n"));
    }

    #[test]
    fn teamcity_mode_streams_one_group_per_test() {
        let mut p = Progress::default();
        p.set_mode(Mode::Teamcity);
        let (mut sink, buf) = Sink::captured();
        p.on_report(&mut sink, None, &report("setup", "passed"));
        p.on_report(&mut sink, None, &report("call", "passed"));
        p.on_report(&mut sink, None, &report("teardown", "failed"));
        let mut skip = report("call", "skipped");
        skip.skip_reason = Some("no db".into());
        p.on_report(&mut sink, None, &skip);
        p.on_report(&mut sink, None, &xfailed(report("call", "skipped")));
        p.on_report(&mut sink, None, &report("call", "skipped"));
        p.finish(&mut sink);
        // The teardown failure is reported but not counted as a test.
        assert_eq!(p.done, 4);
        let n = "tests/test_a.py::test_x";
        let start = format!("##teamcity[testStarted name='{n}']");
        let end = format!("##teamcity[testFinished name='{n}' duration='1234']");
        let ignored = |m: &str| format!("##teamcity[testIgnored name='{n}' message='{m}']");
        assert_eq!(
            buf.out(),
            [
                format!("{start}\n{end}"),
                format!(
                    "{start}\n##teamcity[testFailed name='{n}' message='teardown failed' \
                     details='assert 1 == 2|nline two']\n{end}"
                ),
                format!("{start}\n{}\n{end}", ignored("no db")),
                format!("{start}\n{}\n{end}", ignored("expected failure (xfail)")),
                format!("{start}\n{}\n{end}", ignored("skipped")),
            ]
            .map(|g| g + "\n")
            .concat()
        );
    }

    #[test]
    fn tap_failure_text_becomes_diagnostics() {
        let mut p = Progress::default();
        p.set_mode(Mode::Tap);
        let (mut sink, buf) = Sink::captured();
        p.on_report(&mut sink, None, &report("setup", "passed"));
        p.on_report(&mut sink, None, &report("call", "failed"));
        p.on_report(&mut sink, None, &report("teardown", "failed"));
        let mut skip = report("call", "skipped");
        skip.skip_reason = Some("a\nb".into());
        p.on_report(&mut sink, None, &skip);
        p.finish(&mut sink);
        p.tap_plan(&mut sink, &[]);
        assert_eq!(
            buf.out(),
            "not ok 1 - tests/test_a.py::test_x\n\
             # assert 1 == 2\n\
             # line two\n\
             not ok 2 - tests/test_a.py::test_x # teardown error\n\
             # assert 1 == 2\n\
             # line two\n\
             ok 3 - tests/test_a.py::test_x # SKIP a b\n\
             1..3\n"
        );
        let mut setup_err = report("setup", "failed");
        setup_err.longrepr = None;
        assert_eq!(
            tap_result_line(9, &setup_err).unwrap(),
            "not ok 9 - tests/test_a.py::test_x # setup error"
        );
        let mut empty_reason = report("call", "skipped");
        empty_reason.skip_reason = Some(String::new());
        assert_eq!(
            tap_result_line(1, &empty_reason).unwrap(),
            "ok 1 - tests/test_a.py::test_x # SKIP"
        );
    }

    #[test]
    fn pytest_mode_renders_nothing() {
        let mut p = Progress::default();
        p.set_mode(Mode::Pytest);
        p.set_total(1);
        let (mut sink, buf) = Sink::captured();
        p.on_report(&mut sink, None, &report("call", "failed"));
        p.finish(&mut sink);
        assert_eq!(buf.out(), "");
        assert_eq!(p.done, 0);
    }

    #[test]
    fn json_failure_carries_longrepr_and_lineno() {
        let mut r = report("call", "failed");
        r.lineno = Some(7);
        r.duration = 0.123456;
        let obj = testreport_json(None, &r);
        assert_eq!(obj["longrepr"], "assert 1 == 2\nline two");
        assert_eq!(obj["lineno"], 7);
        assert_eq!(obj["duration"], 0.1235);
        assert!(obj.get("worker").is_none());
        // cpu rides the call phase only.
        let mut setup = report("setup", "passed");
        setup.cpu = Some(0.5);
        assert!(testreport_json(None, &setup).get("cpu").is_none());
    }

    #[test]
    fn footer_routes_output_and_tracks_workers() {
        // A non-live footer is transparent: the dots stream is unchanged.
        let mut p = Progress::default();
        p.enable_footer(2, false);
        p.set_total(2);
        let (mut sink, buf) = Sink::captured();
        p.item_started(&mut sink, 0, "tests/test_a.py::test_x".into());
        p.on_report(&mut sink, Some(0), &report("call", "passed"));
        p.item_finished(&mut sink, 0);
        p.tick(&mut sink);
        p.finish(&mut sink);
        assert_eq!(buf.out(), ". [ 50%]\n");

        // A live footer paints the running test and the bar header (the mode
        // set after enabling reaches the footer), and is wiped by finish().
        let mut p = Progress::default();
        p.enable_footer(1, true);
        p.set_mode(Mode::Bar);
        p.set_total(4);
        let (mut sink, buf) = Sink::captured();
        p.item_started(&mut sink, 0, "tests/test_live.py::t".into());
        let painted = buf.out();
        assert!(painted.contains("tests/test_live.py::t"), "{painted:?}");
        assert!(painted.contains("   0% (0/4)"), "{painted:?}");
        p.on_report(&mut sink, Some(0), &report("call", "passed"));
        p.item_finished(&mut sink, 0);
        p.tick(&mut sink);
        p.finish(&mut sink);
        let out = buf.out();
        assert!(
            out.contains("\x1b[0J[gw0] ✓ tests/test_a.py::test_x  1.23s [ 25%]\n"),
            "{out:?}"
        );
        assert!(out.contains("  25% (1/4)"), "{out:?}");
        assert!(
            out.ends_with("\x1b[0J"),
            "finish erases the footer: {out:?}"
        );

        // Dots mode with a live footer: a plain counter header, and dots are
        // reprinted after each repaint.
        let mut p = Progress::default();
        p.enable_footer(1, true);
        p.set_total(2);
        let (mut sink, buf) = Sink::captured();
        p.on_report(&mut sink, None, &report("call", "passed"));
        let out = buf.out();
        assert!(out.starts_with(".\n\x1b[2m[0/2]\x1b[0m\n"), "{out:?}");
        assert!(out.ends_with("\x1b[3A\r."), "{out:?}");
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
