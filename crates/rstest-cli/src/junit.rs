//! Orchestrator-side junitxml: under a worker pool every session writing
//! `--junitxml` would clobber the same file, so rstest intercepts the flag
//! and renders the merged result here (pytest junit_family="xunit2" shape).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use crate::report::Run;

pub fn write(path: &Path, run: &Run, suite_seconds: f64) -> Result<()> {
    let mut tests = 0u64;
    let mut failures = 0u64;
    let mut errors = 0u64;
    let mut skipped = 0u64;
    let mut body = String::new();

    for (nodeid, entry) in run.tests() {
        tests += 1;
        let (classname, name) = split_nodeid(nodeid);
        let time = entry.duration.unwrap_or(0.0);
        let _ = write!(
            body,
            r#"<testcase classname="{}" name="{}" time="{time:.3}""#,
            esc(&classname),
            esc(&name)
        );
        let setup_failed = entry.setup.as_deref() == Some("failed");
        let teardown_failed = entry.teardown.as_deref() == Some("failed");
        let call = entry.call.as_deref();
        if setup_failed || teardown_failed {
            errors += 1;
            let text = run.failure_text(nodeid).unwrap_or("error");
            let _ = write!(
                body,
                "><error message=\"{}\">{}</error></testcase>",
                esc("error"),
                esc(text)
            );
        } else if call == Some("failed") {
            failures += 1;
            let text = run.failure_text(nodeid).unwrap_or("failed");
            let _ = write!(
                body,
                "><failure message=\"{}\">{}</failure></testcase>",
                esc("failed"),
                esc(text)
            );
        } else if call == Some("skipped") || entry.setup.as_deref() == Some("skipped") {
            skipped += 1;
            let reason = entry.skip_reason.as_deref().unwrap_or("skipped");
            let _ = write!(body, "><skipped message=\"{}\"/></testcase>", esc(reason));
        } else if entry.flaky {
            // Passed only after reruns: JUnit has no standard flaky element,
            // so flag it the standard-extension way — a testcase property —
            // for dashboards that read junit rather than --report-json.
            body.push_str(
                "><properties><property name=\"flaky\" value=\"true\"/></properties></testcase>",
            );
        } else {
            body.push_str("/>");
        }
        body.push('\n');
    }

    let xml = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites>
<testsuite name="rstest" errors="{errors}" failures="{failures}" skipped="{skipped}" tests="{tests}" time="{suite_seconds:.3}">
{body}</testsuite>
</testsuites>
"#
    );
    std::fs::write(path, xml)?;
    Ok(())
}

/// pytest classname convention: path components + classes joined with dots,
/// file extension dropped; name = the final component (with params).
fn split_nodeid(nodeid: &str) -> (String, String) {
    let mut parts: Vec<&str> = nodeid.split("::").collect();
    let name = parts.pop().unwrap_or(nodeid).to_string();
    let file = parts.first().copied().unwrap_or("");
    let module = file.trim_end_matches(".py").replace(['/', '\\'], ".");
    let mut classname = module;
    for cls in parts.iter().skip(1) {
        classname.push('.');
        classname.push_str(cls);
    }
    (classname, name)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_metacharacters() {
        assert_eq!(esc(r#"a<b & "c">"#), "a&lt;b &amp; &quot;c&quot;&gt;");
    }

    #[test]
    fn nodeid_to_classname() {
        assert_eq!(
            split_nodeid("tests/sub/test_a.py::TestX::test_one[p1]"),
            (
                "tests.sub.test_a.TestX".to_string(),
                "test_one[p1]".to_string()
            )
        );
        assert_eq!(
            split_nodeid("test_a.py::test_plain"),
            ("test_a".to_string(), "test_plain".to_string())
        );
    }

    #[test]
    fn renders_counts_and_flaky_property() {
        let mut run = crate::report::Run::default();
        for (nodeid, outcome) in [("a.py::ok", "passed"), ("a.py::bad", "failed")] {
            run.record(
                None,
                crate::proto::Report {
                    nodeid: nodeid.into(),
                    when: "call".into(),
                    outcome: outcome.into(),
                    duration: 0.01,
                    longrepr: (outcome == "failed").then(|| "assert 1 == 2".into()),
                    wasxfail: false,
                    skip_reason: None,
                    cpu: None,
                    sections: Vec::new(),
                    lineno: None,
                },
            );
        }
        run.mark_flaky("a.py::ok".into(), 1);
        let path =
            std::env::temp_dir().join(format!("rstest-junit-test-{}.xml", std::process::id()));
        write(&path, &run, 1.5).unwrap();
        let xml = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(xml.contains(r#"failures="1""#), "{xml}");
        assert!(xml.contains(r#"tests="2""#), "{xml}");
        assert!(
            xml.contains(r#"<property name="flaky" value="true"/>"#),
            "{xml}"
        );
        assert!(xml.contains("assert 1 == 2"), "{xml}");
    }
}
