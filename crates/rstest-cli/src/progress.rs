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
        if self.mode == Mode::Verbose {
            return self.on_report_verbose(worker, r);
        }
        if self.mode == Mode::Bar {
            return self.on_report_bar(worker, r);
        }
        // Github shares the dots char stream below (the human-readable log);
        // its annotations are emitted from the aggregate at end-of-run.
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
        if self.mode == Mode::Verbose || self.mode == Mode::Bar {
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
