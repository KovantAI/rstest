//! Orchestrator-side junitxml: under a worker pool every session writing
//! `--junitxml` would clobber the same file, so rstest intercepts the flag
//! and renders the merged result here (pytest junit_family="xunit2" shape).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use crate::reporting::report::Run;

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
        // Route off the shared outcome bucket rather than re-deriving phase
        // logic here, so junit can never disagree with the summary/report-json
        // over what a given entry IS. xpassed lands in the `_` arm (a plain
        // pass in junit terms); xfailed rides the skipped arm, as pytest does.
        match entry.outcome() {
            "quarantined" => {
                // No <failure>/<error> element (junit-gating CI must stay
                // green) but flagged the property way, like flaky, so
                // dashboards can track the quarantine set.
                body.push_str(
                    "><properties><property name=\"quarantined\" value=\"true\"/></properties></testcase>",
                );
            }
            "errors" => {
                errors += 1;
                let text = run.failure_text(nodeid).unwrap_or("error");
                let _ = write!(
                    body,
                    "><error message=\"{}\">{}</error></testcase>",
                    esc("error"),
                    esc(text)
                );
            }
            "failed" => {
                failures += 1;
                let text = run.failure_text(nodeid).unwrap_or("failed");
                let _ = write!(
                    body,
                    "><failure message=\"{}\">{}</failure></testcase>",
                    esc("failed"),
                    esc(text)
                );
            }
            "skipped" | "xfailed" => {
                skipped += 1;
                let reason = entry.skip_reason.as_deref().unwrap_or("skipped");
                let _ = write!(body, "><skipped message=\"{}\"/></testcase>", esc(reason));
            }
            _ if entry.flaky => {
                // Passed only after reruns: JUnit has no standard flaky element,
                // so flag it the standard-extension way (a testcase property)
                // for dashboards that read junit rather than --report-json.
                body.push_str(
                    "><properties><property name=\"flaky\" value=\"true\"/></properties></testcase>",
                );
            }
            _ => {
                body.push_str("/>");
            }
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
    // Not `nodeid_file`: a bare-file nodeid (collection error, no `::`) must
    // yield an empty classname here, whereas `nodeid_file` returns the file.
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
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            // Chars illegal in XML 1.0 even as numeric char refs (NUL, \x1B
            // from ANSI-colored capture, C1 controls, the Unicode
            // noncharacters, ...) make strict CI parsers reject the whole file.
            // pytest (junit_family=xunit2) replaces them with a visible `#xNN`
            // token — `#x{:02X}` up to 0xFF, `#x{:04X}` above — so mirror that.
            c if is_illegal_xml_char(c as u32) => {
                let cp = c as u32;
                if cp <= 0xFF {
                    let _ = write!(out, "#x{cp:02X}");
                } else {
                    let _ = write!(out, "#x{cp:04X}");
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Whether `cp` is illegal in an XML 1.0 document (its `Char` production
/// forbids it even as a numeric character reference). Mirrors pytest's
/// `_junitxml.bin_xml_escape` illegal-char set: the C0 controls except
/// `\t \n \r`, the C1 controls (0x7F-0x84, 0x86-0x9F; 0x85 NEL is legal), and
/// the per-plane noncharacters (0xFDD0-0xFDEF plus every `*FFFE`/`*FFFF`).
/// Surrogates (0xD800-0xDFFF) can't occur in a Rust `char`, so aren't checked.
fn is_illegal_xml_char(cp: u32) -> bool {
    matches!(cp,
        0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F
        | 0x7F..=0x84 | 0x86..=0x9F
        | 0xFDD0..=0xFDEF
    ) || (cp & 0xFFFE) == 0xFFFE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_metacharacters() {
        assert_eq!(esc(r#"a<b & "c">"#), "a&lt;b &amp; &quot;c&quot;&gt;");
    }

    #[test]
    fn strips_illegal_control_bytes() {
        // NUL, \x0B, \x1B (ANSI ESC) are illegal in XML 1.0 even as char refs;
        // rendered as pytest's visible #xNN token. \t \n \r pass through.
        assert_eq!(esc("a\x00b\x0Bc\x1Bd"), "a#x00b#x0Bc#x1Bd");
        assert_eq!(esc("a\tb\nc\rd"), "a\tb\nc\rd");
    }

    #[test]
    fn strips_c1_controls_and_noncharacters() {
        // C1 controls (DEL 0x7F, 0x9F) are illegal too; 0x85 (NEL) is legal.
        assert_eq!(esc("a\x7fb\u{9f}c"), "a#x7Fb#x9Fc");
        assert_eq!(esc("a\u{85}b"), "a\u{85}b");
        // Unicode noncharacters are illegal in XML 1.0 and, being > 0xFF, use
        // the 4-hex form — a bare `#x02` here would still be rejected.
        assert_eq!(esc("a\u{fffe}b\u{ffff}c"), "a#xFFFEb#xFFFFc");
        assert_eq!(esc("a\u{fdd0}b"), "a#xFDD0b");
        assert_eq!(esc("a\u{1fffe}b"), "a#x1FFFEb");
        // Legal astral chars (e.g. emoji) are untouched.
        assert_eq!(esc("a\u{1f600}b"), "a\u{1f600}b");
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
        let mut run = crate::reporting::report::Run::default();
        for (nodeid, outcome) in [("a.py::ok", "passed"), ("a.py::bad", "failed")] {
            run.record(
                None,
                crate::scheduling::proto::Report {
                    nodeid: nodeid.into(),
                    when: "call".into(),
                    outcome: outcome.into(),
                    duration: 0.01,
                    longrepr: (outcome == "failed").then(|| "assert 1 == 2".into()),
                    wasxfail: false,
                    skip_reason: None,
                    cpu: None,
                    thread_delta: None,
                    fd_delta: None,
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

    fn rep(
        nodeid: &str,
        when: &str,
        outcome: &str,
        skip_reason: Option<&str>,
    ) -> crate::scheduling::proto::Report {
        crate::scheduling::proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: outcome.into(),
            duration: 0.0,
            longrepr: (outcome == "failed").then(|| "boom".into()),
            wasxfail: false,
            skip_reason: skip_reason.map(Into::into),
            cpu: None,
            thread_delta: None,
            fd_delta: None,
            sections: Vec::new(),
            lineno: None,
        }
    }

    #[test]
    fn renders_error_skipped_quarantined_and_plain_pass() {
        let mut run = crate::reporting::report::Run::default();
        run.record(None, rep("a.py::plain", "call", "passed", None)); // -> `/>`
        run.record(None, rep("a.py::setup_err", "setup", "failed", None)); // -> <error>
        run.record(None, rep("a.py::skip", "call", "skipped", Some("no mac"))); // -> <skipped>
        run.record(None, rep("a.py::quar", "call", "failed", None)); // will be quarantined
                                                                     // Quarantine the failing test => no <failure>, a property instead.
        run.quarantine(|nodeid| nodeid.contains("quar"));

        let path =
            std::env::temp_dir().join(format!("rstest-junit-kinds-{}.xml", std::process::id()));
        write(&path, &run, 2.0).unwrap();
        let xml = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(xml.contains(r#"tests="4""#), "{xml}");
        assert!(xml.contains(r#"errors="1""#), "{xml}");
        assert!(xml.contains(r#"skipped="1""#), "{xml}");
        assert!(xml.contains("<error message=\"error\">"), "{xml}");
        assert!(xml.contains(r#"<skipped message="no mac"/>"#), "{xml}");
        assert!(
            xml.contains(r#"<property name="quarantined" value="true"/>"#),
            "{xml}"
        );
        // The plain pass is a self-closed testcase with no child element.
        assert!(xml.contains(r#"name="plain" time="0.000"/>"#), "{xml}");
        // Quarantined failure must NOT surface as a <failure> (gate stays green).
        assert!(!xml.contains("<failure"), "{xml}");
    }

    #[test]
    fn xfailed_routes_to_skipped_via_shared_outcome() {
        // outcome()=="xfailed" (call skipped + wasxfail) must render as
        // <skipped>, like pytest — proving junit rides the shared bucket, not a
        // private re-derivation that would drop the xfail into a plain pass.
        let mut run = crate::reporting::report::Run::default();
        run.record(None, rep("a.py::xf", "setup", "passed", None));
        let mut r = rep("a.py::xf", "call", "skipped", Some("expected fail"));
        r.wasxfail = true;
        run.record(None, r);
        run.record(None, rep("a.py::xf", "teardown", "passed", None));

        let path =
            std::env::temp_dir().join(format!("rstest-junit-xfail-{}.xml", std::process::id()));
        write(&path, &run, 1.0).unwrap();
        let xml = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(xml.contains(r#"skipped="1""#), "{xml}");
        assert!(
            xml.contains(r#"<skipped message="expected fail"/>"#),
            "{xml}"
        );
    }
}
