//! CI-native failure/flaky annotations: GitHub Actions `::error`/`::warning`
//! workflow commands, Azure `##vso[task.logissue]` commands, and Buildkite
//! flaky annotations, emitted from an aggregate Run at end-of-run.

use super::report;

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
    let rel = nodeid.split("::").next().unwrap_or(nodeid);
    match prefix {
        Some(p) => format!("{p}/{rel}"),
        None => rel.to_string(),
    }
}

/// Whether any phase (setup/call/teardown) of this test failed.
fn is_failed(entry: &report::TestEntry) -> bool {
    entry.call.as_deref() == Some("failed")
        || entry.setup.as_deref() == Some("failed")
        || entry.teardown.as_deref() == Some("failed")
}

/// Plural suffix for a rerun count.
fn plural(n: u32) -> &'static str {
    if n > 1 {
        "s"
    } else {
        ""
    }
}

pub(crate) fn print_github_annotations(run: &report::Run) {
    // Under a monorepo the parent runs us with cwd=project, so nodeid paths
    // are project-relative; GitHub resolves annotation `file` from the repo
    // root, so prefix the project's root-relative path (set by the parent).
    let prefix = mono_prefix();
    for (nodeid, entry) in run.tests() {
        if !is_failed(entry) {
            continue;
        }
        let file = source_path(nodeid, &prefix);
        let mut props = format!("file={},title={}", gh_prop(&file), gh_prop(nodeid));
        if let Some(l) = entry.lineno {
            props.push_str(&format!(",line={}", l + 1));
        }
        let msg = entry.longrepr.as_deref().unwrap_or("test failed");
        println!("::error {props}::{}", gh_data(msg));
    }
    // Flaky-passed tests (green only after reruns) surface as warnings:
    // the run is green, but the flake is visible on the PR without
    // opening the junit/log.
    for (nodeid, attempts) in &run.flaky {
        let Some(entry) = run.tests().get(nodeid) else {
            continue;
        };
        let file = source_path(nodeid, &prefix);
        let mut props = format!("file={},title={}", gh_prop(&file), gh_prop(nodeid));
        if let Some(l) = entry.lineno {
            props.push_str(&format!(",line={}", l + 1));
        }
        println!(
            "::warning {props}::flaky: passed only after {attempts} rerun{}",
            plural(*attempts)
        );
    }
}

/// Emit Azure Pipelines `##vso[task.logissue ...]` commands per failed test,
/// which Azure renders as inline issues on the PR (same mapping as GitHub).
/// Flaky-passed tests follow as `type=warning`; messages collapse to one line.
pub(crate) fn print_azure_annotations(run: &report::Run) {
    let prefix = mono_prefix();
    for (nodeid, entry) in run.tests() {
        if !is_failed(entry) {
            continue;
        }
        let mut props = format!(
            "type=error;sourcepath={}",
            az_prop(&source_path(nodeid, &prefix))
        );
        if let Some(l) = entry.lineno {
            props.push_str(&format!(";linenumber={}", l + 1));
        }
        let msg = entry.longrepr.as_deref().unwrap_or("test failed");
        println!("##vso[task.logissue {props}]{}: {}", nodeid, az_line(msg));
    }
    for (nodeid, attempts) in &run.flaky {
        let Some(entry) = run.tests().get(nodeid) else {
            continue;
        };
        let mut props = format!(
            "type=warning;sourcepath={}",
            az_prop(&source_path(nodeid, &prefix))
        );
        if let Some(l) = entry.lineno {
            props.push_str(&format!(";linenumber={}", l + 1));
        }
        println!(
            "##vso[task.logissue {props}]{nodeid}: flaky, passed only after {attempts} rerun{}",
            plural(*attempts)
        );
    }
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
pub(crate) fn buildkite_flaky_annotate(run: &report::Run) {
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
            eprintln!("rstest: skipping Buildkite flaky annotation (buildkite-agent: {e})");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(md.as_bytes());
    }
    if let Err(e) = child.wait() {
        eprintln!("rstest: buildkite-agent annotate failed: {e}");
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
    fn github_annotations_cover_fail_and_flaky_branches() {
        print_github_annotations(&sample_run());
    }

    #[test]
    fn azure_annotations_cover_fail_and_flaky_branches() {
        print_azure_annotations(&sample_run());
    }

    #[test]
    fn buildkite_annotate_is_noop_without_flaky() {
        // Empty flaky => early return before any env/agent interaction.
        buildkite_flaky_annotate(&report::Run::default());
    }
}
