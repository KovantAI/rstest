use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::proto;

/// Per-test phase outcomes, mirroring the compat-harness recorder schema
/// (rstest-research/harness/recorder.py) so `diff_snapshots.py` can gate
/// rstest output directly against pytest baselines.
#[derive(Debug, Default, Serialize)]
pub struct TestEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teardown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub wasxfail: bool,
    /// Worker that produced the final outcome (pool runs only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    #[serde(skip)]
    pub cpu: Option<f64>,
    /// Passed only after one or more reruns (--reruns).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub flaky: bool,
    /// Failure text (assertion repr / traceback), failures only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longrepr: Option<String>,
    /// The outcome was fabricated because the worker died on this test
    /// (crash or --worker-timeout kill), not produced by pytest.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub crashed: bool,
    /// Source line of the test (0-based, from pytest's report.location),
    /// for editor mapping. None when pytest reports no location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineno: Option<u64>,
    /// Failed, but matched the --quarantine list: reported distinctly,
    /// never fatal to the run.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub quarantined: bool,
}

/// Run-level metadata for the report-json envelope (schema 5).
pub struct RunMeta {
    pub exitstatus: i32,
    pub duration_seconds: f64,
    pub started_at_epoch: u64,
    pub workers: usize,
    pub argv: Vec<String>,
}

/// A recorded failure: (nodeid, longrepr, sections of (header, body)).
type Failure = (Option<usize>, String, String, Vec<(String, String)>);

#[derive(Debug, Default)]
pub struct Run {
    tests: BTreeMap<String, TestEntry>,
    collect_errors: Vec<(String, String)>,
    failures: Vec<Failure>,
    /// Modules/dirs skipped at collection (count into "skipped", as pytest does).
    pub collect_skips: u64,
    /// nodeids that passed only after rerun(s), with attempt counts.
    pub flaky: Vec<(String, u32)>,
    /// Record (duration, phase, nodeid) for every phase report — only when
    /// --durations was requested (3 entries per test; pandas-scale memory
    /// is real, so default off).
    pub track_phase_durations: bool,
    phase_durations: Vec<(f64, String, String)>,
}

impl Run {
    pub fn record(&mut self, worker: Option<usize>, r: proto::Report) {
        if self.track_phase_durations {
            self.phase_durations
                .push((r.duration, r.when.clone(), r.nodeid.clone()));
        }
        if r.outcome == "failed" {
            self.failures.push((
                worker,
                r.nodeid.clone(),
                r.longrepr.clone().unwrap_or_default(),
                r.sections.clone(),
            ));
        }
        let entry = self.tests.entry(r.nodeid).or_default();
        if let Some(w) = worker {
            entry.worker = Some(format!("gw{w}"));
        }
        if r.outcome == "failed" && entry.longrepr.is_none() {
            // Machine consumers need the WHY, not just the phase verdict
            // (the agent-fleet persona's #1 blocker). Capped: longreprs
            // can be huge at pandas scale.
            entry.longrepr = r.longrepr.as_deref().map(|t| {
                let mut t = t.to_string();
                t.truncate(20_000);
                t
            });
        }
        let outcome = Some(r.outcome);
        match r.when.as_str() {
            "setup" => entry.setup = outcome,
            "call" => {
                entry.call = outcome;
                entry.duration = Some((r.duration * 10_000.0).round() / 10_000.0);
                entry.cpu = r.cpu;
            }
            "teardown" => entry.teardown = outcome,
            _ => {}
        }
        entry.wasxfail |= r.wasxfail;
        if entry.lineno.is_none() {
            entry.lineno = r.lineno;
        }
        if entry.skip_reason.is_none() {
            entry.skip_reason = r.skip_reason;
        }
    }

    pub fn collect_error(&mut self, path: String, longrepr: String) {
        self.collect_errors.push((path, longrepr));
    }

    /// Flag an entry whose failure was fabricated by the orchestrator
    /// (worker crash / watchdog kill) rather than reported by pytest.
    pub fn mark_crashed(&mut self, nodeid: &str) {
        if let Some(e) = self.tests.get_mut(nodeid) {
            e.crashed = true;
        }
    }

    /// Record that a test passed only after `attempts` reruns.
    pub fn mark_flaky(&mut self, nodeid: String, attempts: u32) {
        if let Some(e) = self.tests.get_mut(&nodeid) {
            e.flaky = true;
        }
        self.flaky.push((nodeid, attempts));
    }

    pub fn print_flaky(
        &self,
        palette: &crate::color::Palette,
        history: &std::collections::HashMap<String, crate::flakes::FlakeStats>,
    ) {
        if self.flaky.is_empty() {
            return;
        }
        println!(
            "\n{}",
            palette.yellow("=========== flaky tests (passed after rerun) ===========")
        );
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
    }

    /// The quarantined-failures section: visible (with tracebacks — a
    /// quarantined test still needs fixing), never fatal.
    pub fn print_quarantined(
        &self,
        palette: &crate::color::Palette,
        history: &std::collections::HashMap<String, crate::flakes::FlakeStats>,
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

    /// All test entries, for doctor analysis.
    pub fn tests(&self) -> &BTreeMap<String, TestEntry> {
        &self.tests
    }

    /// pytest's "slowest N durations" block (terminal summary_durations):
    /// every phase, sorted slowest first; below `min` hidden unless `vv`,
    /// with pytest's hidden-count note. `n == 0` means all.
    pub fn print_durations(&self, n: usize, min: f64, vv: bool, palette: &crate::color::Palette) {
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
            // pytest's exact wording — tooling greps for it.
            println!("\n({hidden} durations < {min}s hidden.  Use -vv to show these durations.)");
        }
    }

    /// nodeid -> call duration, for the duration cache (LPT scheduling).
    pub fn durations(&self) -> impl Iterator<Item = (&String, f64)> {
        self.tests
            .iter()
            .filter_map(|(id, e)| e.duration.map(|d| (id, d)))
    }

    pub fn print_failures(&self, palette: &crate::color::Palette) {
        for (worker, nodeid, longrepr, sections) in &self.failures {
            // Quarantined failures print in their own section instead.
            if self.tests.get(nodeid).is_some_and(|e| e.quarantined) {
                continue;
            }
            let attribution = worker.map(|w| format!("[gw{w}] ")).unwrap_or_default();
            println!(
                "\n{}\n{longrepr}",
                palette.bold_red(&format!("--- FAILED {attribution}{nodeid} ---"))
            );
            for (name, content) in sections {
                println!(
                    "{}\n{}",
                    palette.yellow(&format!("--------- {name} ---------")),
                    content.trim_end()
                );
            }
        }
        for (nodeid, longrepr) in &self.collect_errors {
            println!(
                "\n{}\n{longrepr}",
                palette.bold_red(&format!("--- FAILED {nodeid} ---"))
            );
        }
    }

    /// nodeids with any failed phase — the merged `lastfailed` truth.
    pub fn failed_nodeids(&self) -> impl Iterator<Item = &String> {
        self.tests.iter().filter_map(|(id, e)| {
            let failed = [&e.setup, &e.call, &e.teardown]
                .iter()
                .any(|p| p.as_deref() == Some("failed"));
            failed.then_some(id)
        })
    }

    /// (nodeid, longrepr) pairs for failed tests (junit rendering).
    pub fn failure_text(&self, nodeid: &str) -> Option<&str> {
        self.failures
            .iter()
            .find(|(_, id, _, _)| id == nodeid)
            .map(|(_, _, repr, _)| repr.as_str())
    }

    pub fn all_passed(&self) -> bool {
        self.collect_errors.is_empty()
            && self.tests.values().all(|e| {
                e.quarantined
                    || (e.setup.as_deref() != Some("failed")
                        && e.call.as_deref() != Some("failed")
                        && e.teardown.as_deref() != Some("failed"))
            })
    }

    /// Demote failures matching the --quarantine list: they classify and
    /// count as "quarantined", never fail the run, and print in their own
    /// section. Returns the demoted nodeids (empty = nothing matched).
    /// Only actual failures are touched — a quarantined test that passes
    /// stays a plain pass.
    pub fn quarantine(&mut self, matches: impl Fn(&str) -> bool) -> Vec<String> {
        let mut demoted = Vec::new();
        for (nodeid, e) in &mut self.tests {
            let failed = e.setup.as_deref() == Some("failed")
                || e.call.as_deref() == Some("failed")
                || e.teardown.as_deref() == Some("failed");
            if failed && matches(nodeid) {
                e.quarantined = true;
                demoted.push(nodeid.clone());
            }
        }
        demoted
    }

    /// pytest-style "N passed, N failed, ..." counts derived from phases:
    /// a test counts by its call outcome; setup/teardown failures count as
    /// errors; setup skips count as skipped (matches pytest accounting).
    pub fn summary_line(&self) -> String {
        self.counts()
            .iter()
            .filter(|(_, v)| **v > 0)
            .map(|(k, v)| format!("{v} {}", k.replace('_', " ")))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Outcome counts with pytest accounting — the single source of
    /// truth for the terminal summary line AND the report-json envelope
    /// (consumers must never re-derive these by walking `tests`). All
    /// keys always present (zeros included) for a stable shape.
    pub fn counts(&self) -> BTreeMap<&'static str, u64> {
        let mut counts: BTreeMap<&'static str, u64> = [
            ("passed", 0),
            ("failed", 0),
            ("errors", 0),
            ("skipped", 0),
            ("xfailed", 0),
            ("xpassed", 0),
            ("flaky", 0),
            ("quarantined", 0),
            ("collect_errors", 0),
        ]
        .into();
        for entry in self.tests.values() {
            *counts.entry(classify(entry)).or_default() += 1;
        }
        *counts.entry("flaky").or_default() += self.flaky.len() as u64;
        *counts.entry("skipped").or_default() += self.collect_skips;
        *counts.entry("collect_errors").or_default() += self.collect_errors.len() as u64;
        counts
    }

    pub fn write_snapshot(&self, path: &Path, run_meta: &RunMeta) -> Result<()> {
        #[derive(Serialize)]
        struct Snapshot<'a> {
            meta: BTreeMap<&'static str, serde_json::Value>,
            collect_errors: Vec<&'a String>,
            tests: &'a BTreeMap<String, TestEntry>,
        }
        let mut meta = BTreeMap::new();
        meta.insert("runner", "rstest".into());
        // Version history: 1 unversioned original; 2 added
        // longrepr/crashed + the version field; 3 added the envelope
        // (counts, duration_seconds, started_at_epoch, workers, argv);
        // 4 added per-test lineno (0-based, from pytest report.location);
        // 5 added quarantined (per-test flag + counts key).
        meta.insert("schema", 5.into());
        meta.insert("exitstatus", run_meta.exitstatus.into());
        meta.insert(
            "counts",
            serde_json::to_value(self.counts()).unwrap_or_default(),
        );
        meta.insert(
            "duration_seconds",
            ((run_meta.duration_seconds * 100.0).round() / 100.0).into(),
        );
        meta.insert("started_at_epoch", run_meta.started_at_epoch.into());
        meta.insert("workers", run_meta.workers.into());
        meta.insert(
            "argv",
            serde_json::to_value(&run_meta.argv).unwrap_or_default(),
        );
        let snap = Snapshot {
            meta,
            collect_errors: self.collect_errors.iter().map(|(p, _)| p).collect(),
            tests: &self.tests,
        };
        std::fs::write(path, serde_json::to_vec(&snap)?)?;
        Ok(())
    }
}

fn classify(e: &TestEntry) -> &'static str {
    if e.quarantined {
        return "quarantined";
    }
    let setup = e.setup.as_deref();
    if setup == Some("failed") || e.teardown.as_deref() == Some("failed") {
        return "errors";
    }
    if setup == Some("skipped") || e.call.as_deref() == Some("skipped") {
        return if e.wasxfail { "xfailed" } else { "skipped" };
    }
    match e.call.as_deref() {
        Some("passed") => {
            if e.wasxfail {
                "xpassed"
            } else {
                "passed"
            }
        }
        Some("failed") => "failed",
        _ => "errors",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(nodeid: &str, when: &str, outcome: &str) -> proto::Report {
        proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: outcome.into(),
            duration: 0.123456,
            longrepr: (outcome == "failed").then(|| "boom".into()),
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            sections: Vec::new(),
            lineno: None,
        }
    }

    fn full(run: &mut Run, nodeid: &str, outcome: &str) {
        run.record(None, report(nodeid, "setup", "passed"));
        run.record(None, report(nodeid, "call", outcome));
        run.record(None, report(nodeid, "teardown", "passed"));
    }

    #[test]
    fn quarantine_demotes_only_matching_failures() {
        let mut run = Run::default();
        full(&mut run, "a.py::known_flake", "failed");
        full(&mut run, "a.py::real_bug", "failed");
        full(&mut run, "a.py::listed_but_green", "passed");
        let demoted =
            run.quarantine(|id| id == "a.py::known_flake" || id == "a.py::listed_but_green");
        assert_eq!(demoted, vec!["a.py::known_flake"]);
        let counts = run.counts();
        assert_eq!(counts["quarantined"], 1);
        assert_eq!(counts["failed"], 1);
        assert_eq!(counts["passed"], 1); // green listed test stays a pass
        assert!(!run.all_passed()); // the real bug still fails the run
        run.quarantine(|_| true);
        assert!(run.all_passed()); // everything quarantined -> green
        assert!(run.summary_line().contains("2 quarantined"));
        // lastfailed still remembers quarantined failures (--lf must rerun them)
        assert_eq!(run.failed_nodeids().count(), 2);
    }

    #[test]
    fn summary_counts_by_phase() {
        let mut run = Run::default();
        full(&mut run, "a.py::pass1", "passed");
        full(&mut run, "a.py::pass2", "passed");
        full(&mut run, "a.py::fail", "failed");
        full(&mut run, "a.py::skip", "skipped");
        // setup failure counts as error, not failure
        run.record(None, report("a.py::err", "setup", "failed"));
        assert_eq!(
            run.summary_line(),
            "1 errors, 1 failed, 2 passed, 1 skipped"
        );
        assert!(!run.all_passed());
    }

    #[test]
    fn all_passed_and_lastfailed() {
        let mut run = Run::default();
        full(&mut run, "a.py::ok", "passed");
        assert!(run.all_passed());
        full(&mut run, "a.py::bad", "failed");
        let failed: Vec<&String> = run.failed_nodeids().collect();
        assert_eq!(failed, vec!["a.py::bad"]);
    }

    #[test]
    fn collect_errors_block_all_passed() {
        let mut run = Run::default();
        full(&mut run, "a.py::ok", "passed");
        run.collect_error("b.py".into(), "ImportError".into());
        assert!(!run.all_passed());
        assert!(run.summary_line().contains("1 collect errors"));
    }

    #[test]
    fn xfail_accounting() {
        let mut run = Run::default();
        run.record(None, report("a.py::xf", "setup", "passed"));
        let mut r = report("a.py::xf", "call", "skipped");
        r.wasxfail = true;
        run.record(None, r);
        run.record(None, report("a.py::xf", "teardown", "passed"));
        assert_eq!(run.summary_line(), "1 xfailed");
        assert!(run.all_passed());
    }

    #[test]
    fn duration_rounding_and_worker_attribution() {
        let mut run = Run::default();
        run.record(Some(3), report("a.py::t", "call", "passed"));
        let entry = &run.tests()["a.py::t"];
        assert_eq!(entry.duration, Some(0.1235));
        assert_eq!(entry.worker.as_deref(), Some("gw3"));
    }

    #[test]
    fn lineno_recorded_from_any_phase() {
        let mut run = Run::default();
        let mut setup = report("a.py::t", "setup", "passed");
        setup.lineno = Some(11);
        run.record(None, setup);
        // A later phase without a lineno must not clobber the recorded one.
        run.record(None, report("a.py::t", "call", "passed"));
        assert_eq!(run.tests()["a.py::t"].lineno, Some(11));
    }

    #[test]
    fn flaky_marks_entry_and_summary() {
        let mut run = Run::default();
        full(&mut run, "a.py::wobbly", "passed");
        run.mark_flaky("a.py::wobbly".into(), 2);
        assert!(run.tests()["a.py::wobbly"].flaky);
        assert!(run.summary_line().contains("1 flaky"));
    }

    #[test]
    fn phase_durations_only_tracked_on_request() {
        let mut run = Run::default();
        run.record(
            None,
            proto::Report {
                nodeid: "a.py::t".into(),
                when: "call".into(),
                outcome: "passed".into(),
                duration: 1.0,
                longrepr: None,
                wasxfail: false,
                skip_reason: None,
                cpu: None,
                sections: Vec::new(),
                lineno: None,
            },
        );
        assert!(run.phase_durations.is_empty());
        run.track_phase_durations = true;
        run.record(
            None,
            proto::Report {
                nodeid: "a.py::t2".into(),
                when: "setup".into(),
                outcome: "passed".into(),
                duration: 0.5,
                longrepr: None,
                wasxfail: false,
                skip_reason: None,
                cpu: None,
                sections: Vec::new(),
                lineno: None,
            },
        );
        assert_eq!(run.phase_durations.len(), 1);
        assert_eq!(run.phase_durations[0].1, "setup");
    }
}
