//! CI-native failure/flaky annotations: GitHub Actions `::error`/`::warning`
//! workflow commands, Azure `##vso[task.logissue]` commands, and Buildkite
//! flaky annotations, emitted from an aggregate Run at end-of-run.

use super::report;
use super::sink::Sink;

/// The `RSTEST_MONO_PROJECT` prefix (root-relative project path), set by the
/// monorepo parent so child annotations resolve from the repo root, not cwd.
fn mono_prefix() -> Option<String> {
    std::env::var("RSTEST_MONO_PROJECT")
        .ok()
        .filter(|p| !p.is_empty())
}

/// The source file of a nodeid (everything before `::`), prefixed with the
/// monorepo project path when present.
fn source_path(nodeid: &str, prefix: &Option<String>) -> String {
    let rel = crate::text::nodeid_file(nodeid);
    match prefix {
        Some(p) => format!("{p}/{rel}"),
        None => rel.to_string(),
    }
}

/// Plural suffix for a rerun count.
fn plural(n: u32) -> &'static str {
    if n > 1 {
        "s"
    } else {
        ""
    }
}

/// A CI backend's per-annotation line format. The failed/flaky iteration is
/// identical across backends (GitHub, Azure) — only the wire format differs —
/// so it lives once in [`print_annotations`] and each backend supplies just the
/// two line formatters. `file` is the already-prefixed source path; the backend
/// applies its own escaping. `lineno` is pytest's 0-based line (add 1 for the
/// 1-based CI convention).
trait Annotator {
    /// One error/issue line for a failed test; `msg` is its failure text
    /// (already defaulted to "test failed").
    fn error_line(&self, nodeid: &str, file: &str, lineno: Option<u64>, msg: &str) -> String;
    /// One warning line for a flaky-passed test (green only after `attempts`).
    fn warning_line(&self, nodeid: &str, file: &str, lineno: Option<u64>, attempts: u32) -> String;
}

/// Drive an [`Annotator`] over a finished run: an error line per failed test,
/// then a warning line per flaky-passed test.
fn print_annotations(sink: &mut Sink, run: &report::Run, a: &dyn Annotator) {
    // Under a monorepo the parent runs us with cwd=project, so nodeid paths
    // are project-relative; CI resolves the annotation file from the repo
    // root, so prefix the project's root-relative path (set by the parent).
    let prefix = mono_prefix();
    for (nodeid, entry) in run.tests() {
        if !entry.any_phase_failed() {
            continue;
        }
        let file = source_path(nodeid, &prefix);
        let msg = entry.longrepr.as_deref().unwrap_or("test failed");
        sink.out_line(&a.error_line(nodeid, &file, entry.lineno, msg));
    }
    // Flaky-passed tests (green only after reruns) surface as warnings: the run
    // is green, but the flake is visible on the PR without opening the junit/log.
    for (nodeid, attempts) in &run.flaky {
        let Some(entry) = run.tests().get(nodeid) else {
            continue;
        };
        let file = source_path(nodeid, &prefix);
        sink.out_line(&a.warning_line(nodeid, &file, entry.lineno, *attempts));
    }
}

/// GitHub Actions workflow commands: `::error file=,title=,line=::msg`.
struct Github;

impl Github {
    /// The shared `file=,title=[,line=]` property list for both error and
    /// warning lines.
    fn props(nodeid: &str, file: &str, lineno: Option<u64>) -> String {
        let mut props = format!("file={},title={}", gh_prop(file), gh_prop(nodeid));
        if let Some(l) = lineno {
            props.push_str(&format!(",line={}", l + 1));
        }
        props
    }
}

impl Annotator for Github {
    fn error_line(&self, nodeid: &str, file: &str, lineno: Option<u64>, msg: &str) -> String {
        format!(
            "::error {}::{}",
            Self::props(nodeid, file, lineno),
            gh_data(msg)
        )
    }

    fn warning_line(&self, nodeid: &str, file: &str, lineno: Option<u64>, attempts: u32) -> String {
        format!(
            "::warning {}::flaky: passed only after {attempts} rerun{}",
            Self::props(nodeid, file, lineno),
            plural(attempts)
        )
    }
}

/// Azure Pipelines logging commands: `##vso[task.logissue type=;sourcepath=;
/// linenumber=]nodeid: msg`, rendered as inline PR issues (same mapping as
/// GitHub). Messages collapse to one line.
struct Azure;

impl Azure {
    /// The `type=<kind>;sourcepath=[;linenumber=]` property list.
    fn props(kind: &str, file: &str, lineno: Option<u64>) -> String {
        let mut props = format!("type={kind};sourcepath={}", az_prop(file));
        if let Some(l) = lineno {
            props.push_str(&format!(";linenumber={}", l + 1));
        }
        props
    }
}

impl Annotator for Azure {
    fn error_line(&self, nodeid: &str, file: &str, lineno: Option<u64>, msg: &str) -> String {
        format!(
            "##vso[task.logissue {}]{nodeid}: {}",
            Self::props("error", file, lineno),
            az_line(msg)
        )
    }

    fn warning_line(&self, nodeid: &str, file: &str, lineno: Option<u64>, attempts: u32) -> String {
        format!(
            "##vso[task.logissue {}]{nodeid}: flaky, passed only after {attempts} rerun{}",
            Self::props("warning", file, lineno),
            plural(attempts)
        )
    }
}

pub(crate) fn print_github_annotations(sink: &mut Sink, run: &report::Run) {
    print_annotations(sink, run, &Github);
}

pub(crate) fn print_azure_annotations(sink: &mut Sink, run: &report::Run) {
    print_annotations(sink, run, &Azure);
}

/// Azure logissue property value: `;` and `]` would end the property list /
/// command, newlines would split the log line.
fn az_prop(s: &str) -> String {
    az_line(s).replace(';', "%3B").replace(']', "%5D")
}

/// Collapse to the first line for a single-line Azure log message.
fn az_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// Buildkite: surface flaky-passed tests as a `warning` annotation on the
/// build page, best-effort (a missing/failing `buildkite-agent` must not fail
/// the run). No-op off Buildkite or when nothing flaked.
pub(crate) fn buildkite_flaky_annotate(sink: &mut Sink, run: &report::Run) {
    if run.flaky.is_empty()
        || std::env::var("BUILDKITE")
            .ok()
            .filter(|v| !v.is_empty())
            .is_none()
    {
        return;
    }
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut md = String::from("**Flaky tests** (passed only after reruns):\n\n");
    for (nodeid, attempts) in &run.flaky {
        md.push_str(&format!(
            "- `{nodeid}` — {attempts} rerun{}\n",
            plural(*attempts)
        ));
    }
    let child = Command::new("buildkite-agent")
        .args([
            "annotate",
            "--style",
            "warning",
            "--context",
            "rstest-flaky",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            sink.warn(&format!(
                "rstest: skipping Buildkite flaky annotation (buildkite-agent: {e})"
            ));
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(md.as_bytes());
    }
    if let Err(e) = child.wait() {
        sink.warn(&format!("rstest: buildkite-agent annotate failed: {e}"));
    }
}

/// Escape a GitHub workflow-command message (the part after `::`).
fn gh_data(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escape a workflow-command property value (stricter: `:` and `,` too).
fn gh_prop(s: &str) -> String {
    gh_data(s).replace(':', "%3A").replace(',', "%2C")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_escaping_covers_command_metacharacters() {
        // message (data): only % \r \n are special
        assert_eq!(gh_data("a%b\nc\rd"), "a%25b%0Ac%0Dd");
        // property: also : and , so the key=value list can't be broken
        assert_eq!(gh_prop("pkg::test[a,b]"), "pkg%3A%3Atest[a%2Cb]");
        // % must escape first, or the other escapes' %XX would double-encode
        assert_eq!(gh_data("100%"), "100%25");
    }

    #[test]
    fn azure_logissue_escaping() {
        // property value: ; and ] would break the command; newlines collapse
        assert_eq!(az_prop("a;b]c"), "a%3Bb%5Dc");
        // message keeps only the first line, trimmed
        assert_eq!(az_line("  first line  \nsecond\nthird"), "first line");
        assert_eq!(az_line(""), "");
    }

    #[test]
    fn source_path_strips_nodeid_and_applies_prefix() {
        // The file is everything before `::`; a monorepo prefix is prepended.
        assert_eq!(source_path("a/b.py::test_x", &None), "a/b.py");
        assert_eq!(
            source_path("a/b.py::test_x", &Some("proj".into())),
            "proj/a/b.py"
        );
        // No `::` => the whole string is the path.
        assert_eq!(source_path("bare", &None), "bare");
    }

    #[test]
    fn plural_suffix_only_past_one() {
        assert_eq!(plural(0), "");
        assert_eq!(plural(1), "");
        assert_eq!(plural(2), "s");
    }

    // Build a Run with a call-failed test (line + longrepr), a setup-failed test
    // (no line, no longrepr), and a flaky-passed test, then drive both printers.
    // Output goes to the captured test stdout; the point is to cover every
    // branch (is_failed variants, lineno Some/None, longrepr Some/None, flaky).
    fn report_of(
        nodeid: &str,
        when: &str,
        outcome: &str,
        lineno: Option<u64>,
        longrepr: Option<&str>,
    ) -> crate::scheduling::proto::Report {
        crate::scheduling::proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: outcome.into(),
            duration: 0.0,
            longrepr: longrepr.map(Into::into),
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            thread_delta: None,
            fd_delta: None,
            sections: Vec::new(),
            lineno,
        }
    }

    fn sample_run() -> report::Run {
        let mut run = report::Run::default();
        // call-failed with location + traceback
        run.record(
            Some(0),
            report_of(
                "a.py::test_call",
                "call",
                "failed",
                Some(41),
                Some("assert x\nframe"),
            ),
        );
        // setup-failed, no location, no longrepr => falls back to "test failed"
        run.record(
            Some(1),
            report_of("b.py::test_setup", "setup", "failed", None, None),
        );
        // a passing test that later flakes green
        run.record(
            Some(0),
            report_of("c.py::test_flk", "call", "passed", Some(9), None),
        );
        run.mark_flaky("c.py::test_flk".into(), 2);
        run
    }

    #[test]
    fn github_annotations_emit_exact_lines() {
        let (mut sink, cap) = Sink::captured();
        print_github_annotations(&mut sink, &sample_run());
        let out = cap.out();
        let lines: Vec<&str> = out.lines().collect();
        // call-failed: file+title+line (0-based 41 -> 1-based 42), repr as data.
        assert_eq!(
            lines[0],
            "::error file=a.py,title=a.py%3A%3Atest_call,line=42::assert x%0Aframe"
        );
        // setup-failed: no lineno prop, longrepr absent -> "test failed".
        assert_eq!(
            lines[1],
            "::error file=b.py,title=b.py%3A%3Atest_setup::test failed"
        );
        // flaky warning last: green run, surfaced as a warning with rerun count.
        assert_eq!(
            lines[2],
            "::warning file=c.py,title=c.py%3A%3Atest_flk,line=10::flaky: passed only after 2 reruns"
        );
    }

    #[test]
    fn azure_annotations_emit_exact_lines() {
        let (mut sink, cap) = Sink::captured();
        print_azure_annotations(&mut sink, &sample_run());
        let out = cap.out();
        let lines: Vec<&str> = out.lines().collect();
        // call-failed: type=error, linenumber 1-based, message on first repr line.
        assert_eq!(
            lines[0],
            "##vso[task.logissue type=error;sourcepath=a.py;linenumber=42]a.py::test_call: assert x"
        );
        assert_eq!(
            lines[1],
            "##vso[task.logissue type=error;sourcepath=b.py]b.py::test_setup: test failed"
        );
        assert_eq!(
            lines[2],
            "##vso[task.logissue type=warning;sourcepath=c.py;linenumber=10]c.py::test_flk: flaky, passed only after 2 reruns"
        );
    }

    #[test]
    fn buildkite_annotate_is_noop_without_flaky() {
        // Empty flaky => early return before any env/agent interaction.
        buildkite_flaky_annotate(&mut Sink::captured().0, &report::Run::default());
    }
}
