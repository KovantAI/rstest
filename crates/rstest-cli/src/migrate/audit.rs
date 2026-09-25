//! `rstest audit`: the one-command parallel-safety check. Runs the suite under
//! `-n auto` (optionally repeated, since parallel flakiness is probabilistic),
//! classifies every test that fails *only* in parallel against the `-n 0`
//! oracle, and emits a ready-to-paste `@pytest.mark.serial` fix-list.
//!
//! The discrimination (serial ×2 + loadfile) and the polluter/verdict logic are
//! reused verbatim from `migrate-check` ([`classify_failures`]); audit adds the
//! repeat loop, the serial-candidate partition, and the paste-able conftest
//! block.

use std::path::Path;

use anyhow::Result;

use super::bisect::MAXFAIL_LIFT;
use super::classify::{classify_failures, Verdict};
use super::{run_session_report, Outcomes, Phase};
use crate::reporting::sink::Sink;

/// Whether a verdict is fixable by pinning the test to `@pytest.mark.serial`.
/// Isolation / wall-clock stop failing once the test runs alone after the
/// parallel phase. Order-dependency does not: the test needs what ran before
/// it in the same process (a sibling in its file), and the serial phase runs
/// it apart from those siblings, so it would keep failing; `--dist loadfile`
/// keeps the file together instead. An intrinsic flake or a pre-existing
/// (`-n 0` too) failure is a different problem serial can't paper over.
fn serial_fixable(v: Verdict) -> bool {
    matches!(v, Verdict::Isolation | Verdict::WallClock)
}

/// The classifier verdicts split into the audit buckets.
struct Buckets {
    serial: Vec<(String, Verdict)>,
    /// Pass under `--dist loadfile`; fixed by keeping the file together, not
    /// by serial (see [`serial_fixable`]).
    order: Vec<String>,
    intrinsic: Vec<String>,
    /// Missing from a follow-up run, so unclassified; never put in the
    /// serial fix-list, but still fails the gate (safety is unknown).
    inconclusive: Vec<String>,
    preexisting: usize,
}

impl Buckets {
    fn parallel_safe(&self) -> bool {
        self.serial.is_empty()
            && self.order.is_empty()
            && self.intrinsic.is_empty()
            && self.inconclusive.is_empty()
    }
}

/// Split the classifier verdicts into the audit buckets, each sorted by nodeid
/// for stable output. Pure over the verdict list so it is unit-testable
/// without driving child sessions.
fn partition(verdicts: &[(String, Verdict)]) -> Buckets {
    let mut serial: Vec<(String, Verdict)> = verdicts
        .iter()
        .filter(|(_, v)| serial_fixable(*v))
        .cloned()
        .collect();
    serial.sort_by(|a, b| a.0.cmp(&b.0));
    let ids = |want: Verdict| {
        let mut out: Vec<String> = verdicts
            .iter()
            .filter(|(_, v)| *v == want)
            .map(|(n, _)| n.clone())
            .collect();
        out.sort();
        out
    };
    let preexisting = verdicts
        .iter()
        .filter(|(_, v)| matches!(v, Verdict::NotParallel))
        .count();
    Buckets {
        serial,
        order: ids(Verdict::OrderDependency),
        intrinsic: ids(Verdict::IntrinsicFlake),
        inconclusive: ids(Verdict::Inconclusive),
        preexisting,
    }
}

/// The copy-paste conftest snippet that marks `nodeids` serial. Emitting a
/// nodeid-keyed `pytest_collection_modifyitems` hook (rather than asking the
/// user to decorate each test) means the whole fix is one paste, no per-test
/// edits. Flush-left so it is valid Python as-is (the `--audit-json` string is
/// written straight to a file by tooling); the terminal report indents it at
/// print time. Pure so the exact text is asserted in tests.
fn serial_conftest_block(nodeids: &[String]) -> String {
    let mut s = String::from("import pytest\n\n_RSTEST_SERIAL = {\n");
    for id in nodeids {
        // A JSON string literal is also a valid Python one (`\"`, `\\`,
        // `\uXXXX`); Rust's `{:?}` is not (it can emit `\u{301}`).
        let lit = serde_json::to_string(id).expect("a String always serializes");
        s.push_str(&format!("    {lit},\n"));
    }
    s.push_str(
        "}\n\n\ndef pytest_collection_modifyitems(items):\n\
         \x20   for item in items:\n\
         \x20       if item.nodeid in _RSTEST_SERIAL:\n\
         \x20           item.add_marker(pytest.mark.serial)\n",
    );
    s
}

/// Indent every non-empty line of `block` by `pad` for the terminal report.
fn indent(block: &str, pad: &str) -> String {
    block
        .lines()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{pad}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Merge one parallel run's outcomes into the running union: a test that failed
/// in ANY run is kept as a failure (with that run's timing, for the wait-bound
/// signal); the union covers tests seen in any run. Probabilistic parallel
/// flakes surface as long as they fail in at least one of the repeats.
fn merge_worst(acc: &mut Outcomes, run: Outcomes) {
    for (nodeid, rec) in run {
        acc.entry(nodeid)
            .and_modify(|e| {
                if rec.phase == Phase::Fail {
                    *e = rec;
                }
            })
            .or_insert(rec);
    }
}

/// Run the parallel-safety audit. Exit code: 0 = clean under `-n auto`, 1 = at
/// least one parallel-only failure (serial-fixable, intrinsic flake or
/// inconclusive), 2 = the
/// parallel run produced no snapshot to diff.
pub fn run_audit(
    python: &Path,
    args: &[String],
    repeat: u32,
    json_path: Option<&Path>,
    sink: &mut Sink,
) -> Result<i32> {
    let runs = repeat.max(1);
    // Written up front and overwritten on success, so every early exit (a
    // refused run, an error from a child session) leaves `ran: false` behind
    // instead of a stale result from an earlier run for a CI gate to read.
    if let Some(path) = json_path {
        std::fs::write(path, serde_json::to_string_pretty(&not_run_doc())?)?;
    }
    sink.warn(&format!(
        "rstest audit: running -n auto {runs}× to surface parallel-only failures…"
    ));
    // Lift -x/--maxfail (last wins, so after the user's args and addopts): a
    // parallel pass cut short at the first failure would audit only part of
    // the suite and could still report it parallel-safe.
    let par_args: Vec<String> = args
        .iter()
        .cloned()
        .chain([MAXFAIL_LIFT.to_string()])
        .collect();
    let mut par = Outcomes::new();
    for _ in 0..runs {
        // No report at all means the child refused to dispatch; a report with
        // zero tests (e.g. a `-m` that matches nothing) is handled below.
        let Some(o) = run_session_report(python, &["-n", "auto"], &par_args)? else {
            sink.out_line(
                "rstest audit: rstest produced no run (it may have refused to dispatch — \
                 often an unstable parametrize id). Run `rstest migrate-check` to see why.",
            );
            return Ok(2);
        };
        merge_worst(&mut par, o);
    }
    if par.is_empty() {
        // Nothing selected means nothing unsafe: not a refused run (exit 2)
        // and not a failure, but say so, since it is usually a selection typo.
        if let Some(path) = json_path {
            let doc = audit_doc(0, &partition(&[]));
            std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        }
        sink.out_line(
            "rstest audit: no tests were selected (check your -k/-m/path args); \
             nothing to audit.",
        );
        return Ok(0);
    }

    // classify_failures runs the -n 0 oracle and a loadfile discriminator, so a
    // test that also fails serially is caught as NOT PARALLEL-SPECIFIC. Both
    // repeat as many times as the parallel pass: union-of-N parallel failures
    // against fixed single-shot discriminators would misclassify intermittent
    // failures more often the higher N goes.
    let verdicts = classify_failures(python, args, &par, runs, sink)?;
    let b = partition(&verdicts);
    let Buckets {
        serial,
        order,
        intrinsic,
        inconclusive,
        preexisting,
    } = &b;
    let preexisting = *preexisting;

    if let Some(path) = json_path {
        let doc = audit_doc(par.len(), &b);
        std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    }

    sink.out_line("\n================= rstest audit =================");
    if b.parallel_safe() {
        sink.out_line(&format!(
            "  ✓ parallel-safe: all {} tests that pass at -n 0 also pass at -n auto.",
            par.len()
        ));
        if preexisting > 0 {
            sink.out_line(&format!(
                "  ({preexisting} test(s) already fail at -n 0 — pre-existing, not a \
                 parallelism issue.)"
            ));
        }
        sink.out_line("================================================");
        return Ok(0);
    }

    if !serial.is_empty() {
        sink.out_line(&format!(
            "  ⚠ {} test(s) pass serially but fail under -n auto — mark them serial:\n",
            serial.len()
        ));
        for (nodeid, v) in serial {
            sink.out_line(&format!("    {nodeid}   [{}]", v.title()));
        }
        let ids: Vec<String> = serial.iter().map(|(n, _)| n.clone()).collect();
        sink.out_line(
            "\n  Fix-list — paste into conftest.py to run these last, alone \
             (@pytest.mark.serial):\n",
        );
        sink.out_line(&indent(&serial_conftest_block(&ids), "    "));
        // A second module-level def silently rebinds the name, so an existing
        // hook of the same name would stop running.
        sink.out_line(
            "\n  If conftest.py already defines pytest_collection_modifyitems, paste only \
             _RSTEST_SERIAL and add the loop to that hook instead: a second definition \
             replaces the first.\n",
        );
        // The real, non-stopgap fix per verdict present.
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        sink.out_line("  Serial is a STOPGAP; the underlying fixes:");
        for (_, v) in serial {
            if seen.insert(v.title()) {
                sink.out_line(&format!("    {}: {}", v.title(), v.advice().1));
            }
        }
    }

    if !order.is_empty() {
        sink.out_line(&format!(
            "\n  {} test(s) are ORDER DEPENDENT: they pass at -n 0 and under --dist loadfile \
             but fail when their file is split across workers. Serial won't fix these (it \
             runs them apart from the tests they depend on):",
            order.len()
        ));
        for nodeid in order.iter().take(8) {
            sink.out_line(&format!("    {nodeid}"));
        }
        if order.len() > 8 {
            sink.out_line(&format!("    … and {} more", order.len() - 8));
        }
        sink.out_line(&format!("  Fix: {}", Verdict::OrderDependency.advice().1));
    }

    if !intrinsic.is_empty() {
        sink.out_line(&format!(
            "\n  {} test(s) are INTRINSIC FLAKES (serial repeats disagree) — parallelism \
             only exposed them; serial won't fix these:",
            intrinsic.len()
        ));
        for nodeid in intrinsic.iter().take(8) {
            sink.out_line(&format!("    {nodeid}"));
        }
        if intrinsic.len() > 8 {
            sink.out_line(&format!("    … and {} more", intrinsic.len() - 8));
        }
    }

    if !inconclusive.is_empty() {
        sink.out_line(&format!(
            "\n  {} test(s) are INCONCLUSIVE: they failed under -n auto but did not run in \
             the -n 0 / loadfile follow-up runs, so they can't be classified:",
            inconclusive.len()
        ));
        for nodeid in inconclusive.iter().take(8) {
            sink.out_line(&format!("    {nodeid}"));
        }
        if inconclusive.len() > 8 {
            sink.out_line(&format!("    … and {} more", inconclusive.len() - 8));
        }
        sink.out_line(&format!("  Fix: {}", Verdict::Inconclusive.advice().1));
    }

    if preexisting > 0 {
        sink.out_line(&format!(
            "\n  ({preexisting} test(s) already fail at -n 0 — pre-existing, not a \
             parallelism issue; not listed.)"
        ));
    }
    sink.out_line("================================================");
    // Any parallel-only failure (serial-fixable, order-dependent, intrinsic or
    // inconclusive) fails the gate; pre-existing -n 0 failures do not (they are
    // not a parallel-safety issue).
    Ok(1)
}

/// The `--audit-json` envelope.
fn audit_doc(tests: usize, b: &Buckets) -> serde_json::Value {
    let serial_json: Vec<serde_json::Value> = b
        .serial
        .iter()
        .map(|(nodeid, v)| {
            serde_json::json!({
                "nodeid": nodeid,
                "verdict": v.title(),
                "fix": v.advice().1,
            })
        })
        .collect();
    serde_json::json!({
        "meta": { "runner": "rstest", "kind": "audit", "schema": 1 },
        "ran": true,
        "parallel_safe": b.parallel_safe(),
        "tests": tests,
        "serial_candidates": serial_json,
        "serial_conftest": serial_conftest_block(
            &b.serial.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
        ),
        "order_dependent": b.order,
        "intrinsic_flakes": b.intrinsic,
        "inconclusive": b.inconclusive,
        "preexisting_failures": b.preexisting,
    })
}

/// The `--audit-json` envelope when the `-n auto` pass produced no run.
fn not_run_doc() -> serde_json::Value {
    serde_json::json!({
        "meta": { "runner": "rstest", "kind": "audit", "schema": 1 },
        "ran": false,
        "parallel_safe": false,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{Outcomes, Phase, Rec};
    use super::*;

    fn rec(phase: Phase) -> Rec {
        Rec {
            phase,
            wall: 0.0,
            cpu: None,
        }
    }

    #[test]
    fn partition_splits_by_verdict_kind() {
        let verdicts = vec![
            ("z.py::iso".to_string(), Verdict::Isolation),
            ("a.py::order".to_string(), Verdict::OrderDependency),
            ("m.py::wall".to_string(), Verdict::WallClock),
            ("f.py::flake".to_string(), Verdict::IntrinsicFlake),
            ("b.py::bug".to_string(), Verdict::NotParallel),
            ("q.py::gone".to_string(), Verdict::Inconclusive),
        ];
        let Buckets {
            serial,
            order,
            intrinsic,
            inconclusive,
            preexisting,
        } = partition(&verdicts);
        // serial-fixable sorted by nodeid.
        assert_eq!(
            serial.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["m.py::wall", "z.py::iso"]
        );
        // Order-dependent tests need their file kept together (loadfile);
        // serial would run them apart from the siblings they depend on.
        assert_eq!(order, ["a.py::order"]);
        assert_eq!(intrinsic, ["f.py::flake"]);
        // Inconclusive is never a serial candidate: there is no evidence it
        // passes serially.
        assert_eq!(inconclusive, ["q.py::gone"]);
        assert_eq!(preexisting, 1);
    }

    #[test]
    fn serial_conftest_block_is_pasteable() {
        let block = serial_conftest_block(&["tests/test_a.py::test_x".to_string()]);
        assert!(block.contains("import pytest"));
        assert!(block.contains("\"tests/test_a.py::test_x\","));
        assert!(block.contains("item.add_marker(pytest.mark.serial)"));
        assert!(block.contains("if item.nodeid in _RSTEST_SERIAL:"));
        // Top-level statements start at column 0 so the block is valid Python
        // when pasted or written to a file verbatim.
        assert!(block.starts_with("import pytest\n"));
        assert!(block.contains("\n_RSTEST_SERIAL = {\n"));
        assert!(block.contains("\ndef pytest_collection_modifyitems(items):\n"));
    }

    #[test]
    fn serial_conftest_block_quotes_nodeids_as_python_literals() {
        // Rust `{:?}` would write the combining accent as `\u{301}` and the
        // bell as `\u{7}`, both invalid Python escapes.
        let block = serial_conftest_block(&["t.py::test[e\u{301}\"q\\\u{7}]".to_string()]);
        assert!(!block.contains("\\u{"));
        assert!(block.contains("    \"t.py::test[e\u{301}\\\"q\\\\\\u0007]\",\n"));
    }

    #[test]
    fn indent_pads_non_empty_lines_only() {
        assert_eq!(indent("a\n\n  b\n", "    "), "    a\n\n      b");
    }

    #[test]
    fn merge_worst_keeps_a_failure_from_any_run() {
        let mut acc = Outcomes::new();
        merge_worst(&mut acc, [("t".to_string(), rec(Phase::Pass))].into());
        // A later run's failure overrides the earlier pass (probabilistic flake).
        merge_worst(&mut acc, [("t".to_string(), rec(Phase::Fail))].into());
        assert!(matches!(acc["t"].phase, Phase::Fail));
        // A pass never downgrades an existing failure.
        merge_worst(&mut acc, [("t".to_string(), rec(Phase::Pass))].into());
        assert!(matches!(acc["t"].phase, Phase::Fail));
    }

    #[test]
    fn audit_doc_has_versioned_envelope_and_fix_list() {
        let b = Buckets {
            serial: vec![("a.py::iso".to_string(), Verdict::Isolation)],
            order: vec!["o.py::order".to_string()],
            intrinsic: vec!["f.py::flake".to_string()],
            inconclusive: vec!["q.py::gone".to_string()],
            preexisting: 2,
        };
        let doc = audit_doc(10, &b);
        assert_eq!(doc["ran"], true);
        assert_eq!(doc["inconclusive"][0], "q.py::gone");
        assert_eq!(doc["order_dependent"][0], "o.py::order");
        assert_eq!(doc["meta"]["kind"], "audit");
        assert_eq!(doc["meta"]["schema"], 1);
        assert_eq!(doc["parallel_safe"], false);
        assert_eq!(doc["tests"], 10);
        assert_eq!(doc["serial_candidates"][0]["nodeid"], "a.py::iso");
        assert_eq!(
            doc["serial_candidates"][0]["verdict"],
            Verdict::Isolation.title()
        );
        assert_eq!(doc["intrinsic_flakes"][0], "f.py::flake");
        assert_eq!(doc["preexisting_failures"], 2);
        assert!(doc["serial_conftest"]
            .as_str()
            .unwrap()
            .contains("a.py::iso"));
    }

    #[test]
    fn inconclusive_alone_is_not_parallel_safe() {
        let b = partition(&[("q.py::gone".to_string(), Verdict::Inconclusive)]);
        assert!(!b.parallel_safe());
    }

    #[test]
    fn order_dependency_fails_the_gate_but_is_not_a_serial_candidate() {
        let b = partition(&[("o.py::order".to_string(), Verdict::OrderDependency)]);
        assert!(b.serial.is_empty());
        assert!(!b.parallel_safe());
    }

    #[test]
    fn not_run_doc_says_it_did_not_run() {
        let doc = not_run_doc();
        assert_eq!(doc["meta"]["kind"], "audit");
        assert_eq!(doc["ran"], false);
        assert_eq!(doc["parallel_safe"], false);
    }
}
