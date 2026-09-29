//! Orchestrator-side junitxml: under a worker pool every session writing
//! `--junitxml` would clobber the same file, so rstest intercepts the flag
//! and writes the merged document here.
//!
//! The document is pytest's own: each worker runs pytest's `LogXML` and
//! streams every finished `<testcase>` element (`JunitCase`) plus the suite
//! attributes (`JunitSuite`), so `junit_family`, `junit_logging`,
//! `junit_suite_name`, `--junit-prefix` and the `record_*` fixtures all come
//! out exactly as under pytest. rstest only adds its own signals as standard
//! `<property>` extensions (`quarantined`, `flaky`), drops the `<failure>` of
//! a quarantined test so junit-gating CI stays green, and synthesizes an
//! element, in pytest's shape, for a test no worker finished (a crash, a
//! `--worker-timeout` kill, an `--incremental` cached pass).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use crate::reporting::report::{Run, TestEntry};

/// The junit pieces streamed by the workers during a run.
#[derive(Debug, Default)]
pub struct JunitParts {
    /// nodeid -> serialized `<testcase>` element(s) of its last attempt.
    cases: HashMap<String, Vec<String>>,
    /// nodeids in the order their first element arrived (pytest's own order
    /// in a single session).
    arrival: Vec<String>,
    /// Collection order, when a pool knows it: the merged document follows it
    /// instead of the interleaved arrival order.
    collection: Option<Vec<String>>,
    suite: Option<Suite>,
    /// `record_testsuite_property` elements, deduplicated across workers.
    properties: Vec<String>,
    /// Never-finalized testcases (collection/internal errors). Every pool
    /// worker collects, so these are deduplicated.
    extra: Vec<String>,
}

#[derive(Debug)]
struct Suite {
    name: String,
    timestamp: String,
    hostname: String,
}

impl JunitParts {
    pub fn record_case(&mut self, nodeid: String, cases: Vec<String>) {
        if !self.cases.contains_key(&nodeid) {
            self.arrival.push(nodeid.clone());
        }
        self.cases.insert(nodeid, cases);
    }

    pub fn record_suite(
        &mut self,
        name: String,
        timestamp: String,
        hostname: String,
        properties: Vec<String>,
        extra: Vec<String>,
    ) {
        // The first session to finish names the suite and dates it.
        self.suite.get_or_insert(Suite {
            name,
            timestamp,
            hostname,
        });
        for p in properties {
            if !self.properties.contains(&p) {
                self.properties.push(p);
            }
        }
        for e in extra {
            if !self.extra.contains(&e) {
                self.extra.push(e);
            }
        }
    }

    pub fn set_collection_order(&mut self, ids: &[String]) {
        self.collection = Some(ids.to_vec());
    }

    /// Document order: collection order when known, else arrival order, then
    /// any test with no streamed element (synthesized), sorted.
    fn order<'a>(&'a self, run: &'a Run) -> Vec<&'a str> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out = Vec::new();
        let primary: Box<dyn Iterator<Item = &String>> = match &self.collection {
            Some(ids) => Box::new(ids.iter().chain(self.arrival.iter())),
            None => Box::new(self.arrival.iter()),
        };
        for id in primary.chain(run.tests().keys()) {
            let known = self.cases.contains_key(id) || run.tests().contains_key(id);
            if known && seen.insert(id.as_str()) {
                out.push(id.as_str());
            }
        }
        out
    }
}

pub fn write(path: &Path, run: &Run, suite_seconds: f64) -> Result<()> {
    let parts = &run.junit;
    // pytest opens collection-error testcases during collection, so they
    // precede every test's element.
    let mut cases: Vec<String> = parts.extra.clone();
    for nodeid in parts.order(run) {
        let entry = run.tests().get(nodeid);
        match (parts.cases.get(nodeid), entry) {
            // A crash's outcome is rstest's, not pytest's: synthesize it.
            (Some(streamed), Some(e)) if !e.crashed => {
                cases.extend(streamed.iter().map(|c| decorate(c, e)));
            }
            (Some(streamed), None) => cases.extend(streamed.iter().cloned()),
            (_, Some(e)) => cases.push(synthesize(nodeid, e, run)),
            (None, None) => {}
        }
    }

    let (mut failures, mut errors, mut skipped) = (0usize, 0usize, 0usize);
    for c in &cases {
        failures += count_tag(c, "failure");
        errors += count_tag(c, "error");
        skipped += count_tag(c, "skipped");
    }
    let (name, stamp) = match &parts.suite {
        Some(s) => (
            s.name.as_str(),
            format!(
                r#" timestamp="{}" hostname="{}""#,
                esc_attr(&s.timestamp),
                esc_attr(&s.hostname)
            ),
        ),
        None => ("pytest", String::new()),
    };
    let mut xml = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><testsuites name="pytest tests"><testsuite name="{}" errors="{errors}" failures="{failures}" skipped="{skipped}" tests="{}" time="{suite_seconds:.3}"{stamp}"#,
        esc_attr(name),
        cases.len(),
    );
    if parts.properties.is_empty() && cases.is_empty() {
        xml.push_str(" />");
    } else {
        xml.push('>');
        if !parts.properties.is_empty() {
            xml.push_str("<properties>");
            parts.properties.iter().for_each(|p| xml.push_str(p));
            xml.push_str("</properties>");
        }
        cases.iter().for_each(|c| xml.push_str(c));
        xml.push_str("</testsuite>");
    }
    xml.push_str("</testsuites>");
    super::write_output(path, xml)
}

/// Add rstest's signals to a streamed pytest element.
fn decorate(case: &str, e: &TestEntry) -> String {
    let mut case = case.to_string();
    if e.quarantined {
        // Non-fatal by contract: no <failure>/<error>, so junit gates agree
        // with rstest's exit status; the property keeps it trackable.
        case = strip_element(&strip_element(&case, "failure"), "error");
        case = add_property(&case, "quarantined");
    }
    if e.flaky {
        // Passed only after reruns; JUnit has no standard flaky element.
        case = add_property(&case, "flaky");
    }
    case
}

/// A `<testcase>` in pytest's xunit2 shape for a test no worker finished.
fn synthesize(nodeid: &str, e: &TestEntry, run: &Run) -> String {
    let (classname, name) = split_nodeid(nodeid);
    let head = format!(
        r#"<testcase classname="{}" name="{}" time="{:.3}""#,
        esc_attr(&classname),
        esc_attr(&name),
        e.duration.unwrap_or(0.0)
    );
    let text = run.failure_text(nodeid).unwrap_or("");
    let body = match e.outcome() {
        "quarantined" => {
            r#"<properties><property name="quarantined" value="true" /></properties>"#.to_string()
        }
        "errors" => format!(
            r#"<error message="{}">{}</error>"#,
            esc_attr(&crash_message(text, "error")),
            esc(text)
        ),
        "failed" => format!(
            r#"<failure message="{}">{}</failure>"#,
            esc_attr(&crash_message(text, "failed")),
            esc(text)
        ),
        "skipped" => format!(
            r#"<skipped type="pytest.skip" message="{}" />"#,
            esc_attr(e.skip_reason.as_deref().unwrap_or(""))
        ),
        "xfailed" => format!(
            r#"<skipped type="pytest.xfail" message="{}" />"#,
            esc_attr(e.skip_reason.as_deref().unwrap_or(""))
        ),
        _ if e.flaky => {
            r#"<properties><property name="flaky" value="true" /></properties>"#.to_string()
        }
        _ => String::new(),
    };
    if body.is_empty() {
        format!("{head} />")
    } else {
        format!("{head}>{body}</testcase>")
    }
}

/// pytest's `message` is the crash line; from a rendered traceback that is
/// the last `E   ` line (else the first non-empty line, else `fallback`).
fn crash_message(text: &str, fallback: &str) -> String {
    text.lines()
        .rev()
        .find_map(|l| l.strip_prefix("E   "))
        .or_else(|| text.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or(fallback)
        .trim()
        .to_string()
}

/// Occurrences of a `<tag` start in serialized XML. Text and attribute values
/// are escaped (`&lt;`), so every literal `<tag` is a real element.
fn count_tag(xml: &str, tag: &str) -> usize {
    let open = format!("<{tag}");
    xml.match_indices(&open)
        .filter(|(i, _)| matches!(xml.as_bytes().get(i + open.len()), Some(b' ' | b'>' | b'/')))
        .count()
}

/// Remove every `<tag ...>...</tag>` / `<tag ... />` element.
fn strip_element(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        if !matches!(
            rest.as_bytes().get(i + open.len()),
            Some(b' ' | b'>' | b'/')
        ) {
            out.push_str(&rest[..i + open.len()]);
            rest = &rest[i + open.len()..];
            continue;
        }
        out.push_str(&rest[..i]);
        let Some(gt) = rest[i..].find('>') else {
            return out + &rest[i..];
        };
        let after_start = i + gt + 1;
        rest = if rest[..after_start].ends_with("/>") {
            &rest[after_start..]
        } else {
            match rest[after_start..].find(&close) {
                Some(j) => &rest[after_start + j + close.len()..],
                None => "",
            }
        };
    }
    out.push_str(rest);
    out
}

/// Add `<property name=NAME value="true" />` to a `<testcase>`, after any
/// properties pytest already wrote (properties come first, per the schema).
fn add_property(case: &str, name: &str) -> String {
    let prop = format!(r#"<property name="{name}" value="true" />"#);
    let Some(gt) = case.find('>') else {
        return case.to_string();
    };
    if case[..gt].ends_with('/') {
        let head = case[..gt - 1].trim_end();
        return format!("{head}><properties>{prop}</properties></testcase>");
    }
    if let Some(end) = case.find("</properties>") {
        return format!("{}{prop}{}", &case[..end], &case[end..]);
    }
    format!(
        "{}<properties>{prop}</properties>{}",
        &case[..=gt],
        &case[gt + 1..]
    )
}

/// pytest's `mangle_test_address`: params stay whole, path components and
/// classes join with dots, `.py` dropped; name = the final component.
fn split_nodeid(nodeid: &str) -> (String, String) {
    let (path, params) = match nodeid.find('[') {
        Some(i) => nodeid.split_at(i),
        None => (nodeid, ""),
    };
    let mut names: Vec<String> = path.split("::").map(str::to_string).collect();
    if let Some(first) = names.first_mut() {
        let dotted = first.replace(['/', '\\'], ".");
        *first = dotted.strip_suffix(".py").unwrap_or(&dotted).to_string();
    }
    if names.len() == 1 {
        // A bare file (collection error): pytest's classname is empty.
        names.insert(0, String::new());
    }
    let mut name = names.pop().unwrap_or_default();
    name.push_str(params);
    (names.join("."), name)
}

/// ElementTree's attribute escaping: text escaping plus whitespace as
/// character references, so values roundtrip.
fn esc_attr(s: &str) -> String {
    esc(s)
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
        .replace('\t', "&#09;")
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

    fn write_xml(run: &Run, tag: &str) -> String {
        let path =
            std::env::temp_dir().join(format!("rstest-junit-{tag}-{}.xml", std::process::id()));
        write(&path, run, 1.0).unwrap();
        let xml = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        xml
    }

    #[test]
    fn streamed_elements_are_written_verbatim_in_pytest_order_and_shape() {
        let mut run = Run::default();
        run.record(None, rep("b.py::t", "call", "passed", None));
        run.record(None, rep("a.py::t", "call", "failed", None));
        // Arrival order (b before a) is pytest's own order in one session.
        run.junit.record_case(
            "b.py::t".into(),
            vec![r#"<testcase classname="b" name="t" time="0.001" />"#.into()],
        );
        run.junit.record_case(
            "a.py::t".into(),
            vec![r#"<testcase classname="a" name="t" time="0.002"><failure message="assert 1 == 2">E</failure></testcase>"#.into()],
        );
        run.junit.record_suite(
            "mysuite".into(),
            "2026-09-27T12:00:00+03:00".into(),
            "host".into(),
            vec![r#"<property name="k" value="v" />"#.into()],
            vec![r#"<testcase classname="" name="c" time="0.000"><error message="collection failure">x</error></testcase>"#.into()],
        );
        let xml = write_xml(&run, "streamed");
        assert!(
            xml.starts_with(r#"<?xml version="1.0" encoding="utf-8"?><testsuites name="pytest tests"><testsuite name="mysuite" errors="1" failures="1" skipped="0" tests="3" time="1.000" timestamp="2026-09-27T12:00:00+03:00" hostname="host"><properties><property name="k" value="v" /></properties><testcase classname="" name="c""#),
            "{xml}"
        );
        let b = xml.find(r#"classname="b""#).unwrap();
        let a = xml.find(r#"classname="a""#).unwrap();
        assert!(b < a, "arrival order kept: {xml}");
        assert!(xml.ends_with("</testsuite></testsuites>"), "{xml}");
    }

    #[test]
    fn collection_order_wins_over_arrival_in_a_pool() {
        let mut run = Run::default();
        for id in ["b.py::t", "a.py::t"] {
            run.record(None, rep(id, "call", "passed", None));
            let (c, n) = split_nodeid(id);
            run.junit.record_case(
                id.into(),
                vec![format!(
                    r#"<testcase classname="{c}" name="{n}" time="0.000" />"#
                )],
            );
        }
        run.junit
            .set_collection_order(&["a.py::t".into(), "b.py::t".into()]);
        let xml = write_xml(&run, "collorder");
        assert!(xml.find(r#"classname="a""#).unwrap() < xml.find(r#"classname="b""#).unwrap());
    }

    #[test]
    fn quarantine_strips_failure_and_flaky_adds_property() {
        let failed = r#"<testcase classname="a" name="q" time="0.1"><properties><property name="user" value="1" /></properties><failure message="m">E</failure></testcase>"#;
        let mut e = TestEntry {
            quarantined: true,
            ..TestEntry::default()
        };
        assert_eq!(
            decorate(failed, &e),
            r#"<testcase classname="a" name="q" time="0.1"><properties><property name="user" value="1" /><property name="quarantined" value="true" /></properties></testcase>"#
        );
        e.quarantined = false;
        e.flaky = true;
        assert_eq!(
            decorate(r#"<testcase classname="a" name="f" time="0.1" />"#, &e),
            r#"<testcase classname="a" name="f" time="0.1"><properties><property name="flaky" value="true" /></properties></testcase>"#
        );
    }

    #[test]
    fn empty_run_is_a_self_closed_suite() {
        let xml = write_xml(&Run::default(), "empty");
        assert!(
            xml.ends_with(r#"tests="0" time="1.000" /></testsuites>"#),
            "{xml}"
        );
    }

    #[test]
    fn counts_only_real_element_starts() {
        assert_eq!(
            count_tag(r#"<failure message="&lt;failure">x</failure>"#, "failure"),
            1
        );
        assert_eq!(count_tag("<errors/><error />", "error"), 1);
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
        // Params stay whole even with `::` or `/` inside (pytest partitions at `[`).
        assert_eq!(
            split_nodeid("t.py::test_x[a::b/c]"),
            ("t".to_string(), "test_x[a::b/c]".to_string())
        );
        assert_eq!(split_nodeid("t.py"), (String::new(), "t".to_string()));
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
            xml.contains(r#"<property name="flaky" value="true" />"#),
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
        assert!(xml.contains("<error message=\"boom\">"), "{xml}");
        assert!(
            xml.contains(r#"<skipped type="pytest.skip" message="no mac" />"#),
            "{xml}"
        );
        assert!(
            xml.contains(r#"<property name="quarantined" value="true" />"#),
            "{xml}"
        );
        // The plain pass is a self-closed testcase with no child element.
        assert!(xml.contains(r#"name="plain" time="0.000" />"#), "{xml}");
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
            xml.contains(r#"<skipped type="pytest.xfail" message="expected fail" />"#),
            "{xml}"
        );
    }
}
