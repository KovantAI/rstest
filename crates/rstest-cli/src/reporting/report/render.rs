//! Terminal rendering for a [`Run`]: the `print_*` blocks that write the
//! flaky / quarantined / durations / failures sections to stdout. Split from the
//! data model in [`super`] so recording/accounting stays free of presentation.
//! A child module, so it still reaches `Run`'s private fields.

use super::{FailureWrap, Run, TestEntry};

impl Run {
    pub fn print_flaky(
        &self,
        palette: &crate::reporting::color::Palette,
        history: &std::collections::HashMap<String, crate::reporting::flakes::FlakeStats>,
        wrap: FailureWrap,
    ) {
        if self.flaky.is_empty() {
            return;
        }
        let header = palette.yellow("=========== flaky tests (passed after rerun) ===========");
        // GitLab has no per-line warning command; fold the whole flaky block
        // into one collapsed section so it's tucked away but greppable - the
        // CI-native analogue of GitHub's `::warning` flaky annotations.
        let gitlab = wrap == FailureWrap::GitlabSection;
        let id = format!("rstest_flaky_{}", std::process::id());
        if gitlab {
            println!(
                "\n\x1b[0Ksection_start:{}:{id}[collapsed=true]\r\x1b[0K{header}",
                crate::time::now_epoch_secs()
            );
        } else {
            println!("\n{header}");
        }
        for (nodeid, attempts) in &self.flaky {
            let past = history
                .get(nodeid)
                .filter(|h| h.flaky + h.failed > 0)
                .map(|h| format!("; flaked {}x before, failed {}x", h.flaky, h.failed))
                .unwrap_or_default();
            println!(
                "  {nodeid}  ({attempts} rerun{}{past})",
                if *attempts > 1 { "s" } else { "" }
            );
        }
        if gitlab {
            println!(
                "\x1b[0Ksection_end:{}:{id}\r\x1b[0K",
                crate::time::now_epoch_secs()
            );
        }
    }

    /// The quarantined-failures section: visible (with tracebacks - a
    /// quarantined test still needs fixing), never fatal.
    pub fn print_quarantined(
        &self,
        palette: &crate::reporting::color::Palette,
        history: &std::collections::HashMap<String, crate::reporting::flakes::FlakeStats>,
    ) {
        let quarantined: Vec<(&String, &TestEntry)> =
            self.tests.iter().filter(|(_, e)| e.quarantined).collect();
        if quarantined.is_empty() {
            return;
        }
        println!(
            "\n{}",
            palette.yellow("=========== quarantined failures (known-flaky, non-fatal) ===========")
        );
        for (nodeid, entry) in quarantined {
            let past = history
                .get(nodeid)
                .filter(|h| h.flaky + h.failed > 0)
                .map(|h| format!("  (flaked {}x, failed {}x before)", h.flaky, h.failed))
                .unwrap_or_default();
            println!(
                "\n{}{past}",
                palette.yellow(&format!("--- QUARANTINED {nodeid} ---"))
            );
            if let Some(repr) = &entry.longrepr {
                println!("{repr}");
            }
        }
    }

    /// pytest's "slowest N durations" block (terminal summary_durations):
    /// every phase, sorted slowest first; below `min` hidden unless `vv`,
    /// with pytest's hidden-count note. `n == 0` means all.
    pub fn print_durations(
        &self,
        n: usize,
        min: f64,
        vv: bool,
        palette: &crate::reporting::color::Palette,
    ) {
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
        println!("\n{}", palette.yellow(&header));
        let mut hidden = 0usize;
        for (duration, when, nodeid) in rows {
            if !vv && *duration < min {
                hidden += 1;
                continue;
            }
            println!("{duration:.2}s {when:<8} {nodeid}");
        }
        if hidden > 0 {
            // pytest's exact wording - tooling greps for it.
            println!("\n({hidden} durations < {min}s hidden.  Use -vv to show these durations.)");
        }
    }

    /// The failures block, with each failure optionally wrapped in a CI
    /// log-folding construct so the job UI collapses tracebacks per test.
    pub fn print_failures(&self, palette: &crate::reporting::color::Palette, wrap: FailureWrap) {
        let open = |header: &str, idx: usize| match wrap {
            FailureWrap::Plain => {
                println!(
                    "\n{}",
                    palette.bold_red(&format!("--- FAILED {header} ---"))
                );
            }
            FailureWrap::GitlabSection => {
                // Section ids must be unique within the job log; the
                // pid keeps concurrent monorepo children (whose output
                // the parent reprints) from colliding.
                let id = format!("rstest_fail_{}_{idx}", std::process::id());
                println!(
                    "\n\x1b[0Ksection_start:{}:{id}[collapsed=true]\r\x1b[0K{}",
                    crate::time::now_epoch_secs(),
                    palette.bold_red(&format!("--- FAILED {header} ---"))
                );
            }
            // The `+++` group header IS the headline in the Buildkite log
            // UI - no extra dashes.
            FailureWrap::BuildkiteGroup => {
                println!("\n+++ {}", palette.bold_red(&format!("FAILED {header}")));
            }
        };
        let close = |idx: usize| {
            if wrap == FailureWrap::GitlabSection {
                let id = format!("rstest_fail_{}_{idx}", std::process::id());
                println!(
                    "\x1b[0Ksection_end:{}:{id}\r\x1b[0K",
                    crate::time::now_epoch_secs()
                );
            }
        };
        let mut idx = 0usize;
        for (worker, nodeid, longrepr, sections) in &self.failures {
            // Quarantined failures print in their own section instead.
            if self.tests.get(nodeid).is_some_and(|e| e.quarantined) {
                continue;
            }
            let attribution = worker.map(|w| format!("[gw{w}] ")).unwrap_or_default();
            open(&format!("{attribution}{nodeid}"), idx);
            println!("{longrepr}");
            for (name, content) in sections {
                println!(
                    "{}\n{}",
                    palette.yellow(&format!("--------- {name} ---------")),
                    content.trim_end()
                );
            }
            close(idx);
            idx += 1;
        }
        for (nodeid, longrepr) in &self.collect_errors {
            open(nodeid, idx);
            println!("{longrepr}");
            close(idx);
            idx += 1;
        }
    }
}
