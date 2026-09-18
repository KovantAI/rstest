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

use super::classify::{classify_failures, Verdict};
use super::{run_session, Outcomes, Phase};
use crate::reporting::sink::Sink;

/// Whether a verdict is fixable by pinning the test to `@pytest.mark.serial`.
/// Isolation / wall-clock / order-dependency all stop failing once the test
/// runs alone after the parallel phase; an intrinsic flake or a pre-existing
/// (`-n 0` too) failure is a different problem serial can't paper over.
fn serial_fixable(v: Verdict) -> bool {
    matches!(
        v,
        Verdict::Isolation | Verdict::WallClock | Verdict::OrderDependency
    )
}

/// Split the classifier verdicts into the three audit buckets, each sorted by
/// nodeid for stable output. Pure over the verdict list so it is unit-testable
/// without driving child sessions.
fn partition(verdicts: &[(String, Verdict)]) -> (Vec<(String, Verdict)>, Vec<String>, usize) {
    let mut serial: Vec<(String, Verdict)> = verdicts
        .iter()
        .filter(|(_, v)| serial_fixable(*v))
        .cloned()
        .collect();
    serial.sort_by(|a, b| a.0.cmp(&b.0));
    let mut intrinsic: Vec<String> = verdicts
        .iter()
        .filter(|(_, v)| matches!(v, Verdict::IntrinsicFlake))
        .map(|(n, _)| n.clone())
        .collect();
    intrinsic.sort();
    let preexisting = verdicts
        .iter()
        .filter(|(_, v)| matches!(v, Verdict::NotParallel))
        .count();
    (serial, intrinsic, preexisting)
}

/// The copy-paste conftest snippet that marks `nodeids` serial. Emitting a
/// nodeid-keyed `pytest_collection_modifyitems` hook (rather than asking the
/// user to decorate each test) means the whole fix is one paste, no per-test
/// edits. Pure so the exact text is asserted in tests.
fn serial_conftest_block(nodeids: &[String]) -> String {
    let mut s = String::from("    import pytest\n\n    _RSTEST_SERIAL = {\n");
    for id in nodeids {
        // nodeids never contain a double-quote, so this is safe unquoted.
        s.push_str(&format!("        {id:?},\n"));
    }
    s.push_str(
        "    }\n\n    def pytest_collection_modifyitems(items):\n\
         \x20       for item in items:\n\
         \x20           if item.nodeid in _RSTEST_SERIAL:\n\
         \x20               item.add_marker(pytest.mark.serial)\n",
    );
    s
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
/// least one parallel-only failure (serial-fixable or intrinsic flake), 2 = the
/// parallel run produced no snapshot to diff.
pub fn run_audit(
    python: &Path,
    args: &[String],
    repeat: u32,
    json_path: Option<&Path>,
    sink: &mut Sink,
) -> Result<i32> {
    let _ = python; // child sessions self-resolve the interpreter, like migrate-check.
    let runs = repeat.max(1);
    sink.warn(&format!(
        "rstest audit: running -n auto {runs}× to surface parallel-only failures…"
    ));
    let mut par = Outcomes::new();
    for _ in 0..runs {
        let o = run_session(&["-n", "auto"], args)?;
        if o.is_empty() {
            sink.out_line(
                "rstest audit: rstest produced no run (it may have refused to dispatch — \
                 often an unstable parametrize id). Run `rstest migrate-check` to see why.",
            );
            return Ok(2);
        }
        merge_worst(&mut par, o);
    }

    // classify_failures runs the -n 0 oracle (×2) and a loadfile discriminator,
    // so a test that also fails serially is caught as NOT PARALLEL-SPECIFIC.
    let verdicts = classify_failures(args, &par, sink)?;
    let (serial, intrinsic, preexisting) = partition(&verdicts);

    if let Some(path) = json_path {
        let doc = audit_doc(par.len(), &serial, &intrinsic, preexisting);
        std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    }

    sink.out_line("\n================= rstest audit =================");
    if serial.is_empty() && intrinsic.is_empty() {
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
        for (nodeid, v) in &serial {
            sink.out_line(&format!("    {nodeid}   [{}]", v.title()));
        }
        let ids: Vec<String> = serial.iter().map(|(n, _)| n.clone()).collect();
        sink.out_line(
            "\n  Fix-list — paste into conftest.py to run these last, alone \
             (@pytest.mark.serial):\n",
        );
        sink.out_line(&serial_conftest_block(&ids));
        // The real, non-stopgap fix per verdict present.
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        sink.out_line("  Serial is a STOPGAP; the underlying fixes:");
        for (_, v) in &serial {
            if seen.insert(v.title()) {
                sink.out_line(&format!("    {}: {}", v.title(), v.advice().1));
            }
        }
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

    if preexisting > 0 {
        sink.out_line(&format!(
            "\n  ({preexisting} test(s) already fail at -n 0 — pre-existing, not a \
             parallelism issue; not listed.)"
        ));
    }
    sink.out_line("================================================");
    // Any parallel-only failure (serial-fixable or intrinsic) fails the gate;
    // pre-existing -n 0 failures do not (they are not a parallel-safety issue).
    Ok(1)
}

/// The `--audit-json` envelope.
fn audit_doc(
    tests: usize,
    serial: &[(String, Verdict)],
    intrinsic: &[String],
    preexisting: usize,
) -> serde_json::Value {
    let serial_json: Vec<serde_json::Value> = serial
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
        "parallel_safe": serial.is_empty() && intrinsic.is_empty(),
        "tests": tests,
        "serial_candidates": serial_json,
        "serial_conftest": serial_conftest_block(
            &serial.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
        ),
        "intrinsic_flakes": intrinsic,
        "preexisting_failures": preexisting,
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
        ];
        let (serial, intrinsic, preexisting) = partition(&verdicts);
        // serial-fixable sorted by nodeid.
        assert_eq!(
            serial.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["a.py::order", "m.py::wall", "z.py::iso"]
        );
        assert_eq!(intrinsic, ["f.py::flake"]);
        assert_eq!(preexisting, 1);
    }

    #[test]
    fn serial_conftest_block_is_pasteable() {
        let block = serial_conftest_block(&["tests/test_a.py::test_x".to_string()]);
        assert!(block.contains("import pytest"));
        assert!(block.contains("\"tests/test_a.py::test_x\","));
        assert!(block.contains("item.add_marker(pytest.mark.serial)"));
        assert!(block.contains("if item.nodeid in _RSTEST_SERIAL:"));
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
        let serial = vec![("a.py::iso".to_string(), Verdict::Isolation)];
        let doc = audit_doc(10, &serial, &["f.py::flake".to_string()], 2);
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
}
