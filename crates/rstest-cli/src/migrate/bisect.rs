//! `rstest bisect <nodeid>`: find the *polluter* — the earlier test(s) whose
//! state leak makes the target fail only when run after them.
//!
//! Order-dependency is the hardest flake class: "it only fails when run after
//! some other test." We delta-debug the predecessor set at `-n 0` (serial, so
//! this isolates ORDERING, not concurrency): repeatedly run the victim preceded
//! by a subset of the earlier tests, minimizing (Zeller/Hildebrandt `ddmin`)
//! toward the 1-minimal set of predecessors that still reproduces the failure.
//!
//! Reuses the shared child-session runner ([`run_session`] at `-n 0`) and the
//! collection-order id list ([`collect_ids`]); the minimization and the
//! reproduce primitive are the new logic here.

use std::path::Path;

use anyhow::Result;

use super::{collect_ids, run_session, Phase};
use crate::reporting::sink::Sink;

/// Max child `-n 0` runs a bisect will spend before reporting the smallest
/// reproducing set found so far. ddmin is worst-case quadratic; this bounds a
/// pathological suite to a predictable wall-time.
const DEFAULT_BUDGET: u32 = 80;

/// Did `nodeid` fail (any phase) in this run's outcome map?
fn failed(out: &super::Outcomes, nodeid: &str) -> bool {
    matches!(out.get(nodeid).map(|r| r.phase), Some(Phase::Fail))
}

/// The reproduce oracle: run `-n 0` over `preds` followed by the victim (victim
/// LAST so every predecessor runs before it), and report whether the victim
/// failed. Each call spends one unit of `budget`.
fn reproduces(victim: &str, preds: &[String], budget: &mut u32, sink: &mut Sink) -> Result<bool> {
    if *budget == 0 {
        return Ok(false); // out of budget: treat as "not interesting", stops the search
    }
    *budget -= 1;
    let mut sel: Vec<String> = preds.to_vec();
    sel.push(victim.to_string());
    let out = run_session(&["-n", "0"], &sel)?;
    let _ = sink;
    Ok(failed(&out, victim))
}

/// Delta-debugging minimization (Zeller & Hildebrandt `ddmin`) of the
/// predecessor set: return a 1-minimal subset of `preds` that still reproduces
/// (`interesting`) the victim's failure. `interesting` is the reproduce oracle;
/// it consumes the shared run budget, so on exhaustion the loop converges on the
/// smallest set confirmed so far.
fn ddmin(
    preds: &[String],
    mut interesting: impl FnMut(&[String]) -> Result<bool>,
) -> Result<Vec<String>> {
    let mut circ: Vec<String> = preds.to_vec();
    let mut n = 2usize;
    while circ.len() >= 2 {
        let chunk = circ.len().div_ceil(n);
        let subsets: Vec<Vec<String>> = circ.chunks(chunk).map(|c| c.to_vec()).collect();

        // (1) any single subset reproduces alone -> narrow to it, reset n=2.
        let mut reduced = false;
        for s in &subsets {
            if interesting(s)? {
                circ = s.clone();
                n = 2;
                reduced = true;
                break;
            }
        }
        if reduced {
            continue;
        }

        // (2) any complement (all but one subset) reproduces -> drop that
        // subset, decrease granularity by one.
        for i in 0..subsets.len() {
            let complement: Vec<String> = subsets
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .flat_map(|(_, s)| s.iter().cloned())
                .collect();
            if !complement.is_empty() && interesting(&complement)? {
                circ = complement;
                n = (n - 1).max(2);
                reduced = true;
                break;
            }
        }
        if reduced {
            continue;
        }

        // (3) neither: increase granularity, or stop when already 1-per-chunk.
        if n >= circ.len() {
            break;
        }
        n = (2 * n).min(circ.len());
    }
    Ok(circ)
}

/// Run the order-dependency bisect. Exit code: 0 = order-dependent culprit(s)
/// found, 1 = not order-dependent (fails in isolation, or doesn't reproduce in
/// collection order), 2 = the nodeid isn't in the suite.
pub fn run_bisect(
    python: &Path,
    nodeid: &str,
    json_path: Option<&Path>,
    sink: &mut Sink,
) -> Result<i32> {
    sink.warn(&format!(
        "rstest bisect: collecting the suite to locate {nodeid}…"
    ));
    let ids = collect_ids(python, &[])?;
    let Some(idx) = ids.iter().position(|id| id == nodeid) else {
        sink.out_line(&format!(
            "rstest bisect: {nodeid} was not collected. Check the nodeid \
             (path::test[param]) and that it isn't deselected."
        ));
        return Ok(2);
    };

    let mut budget = DEFAULT_BUDGET;

    // Isolation check: a test that fails ALONE isn't order-dependent — it's a
    // plain failure, and no predecessor set is the culprit.
    let alone = run_session(&["-n", "0"], &[nodeid.to_string()])?;
    budget -= 1;
    if failed(&alone, nodeid) {
        sink.out_line(&format!(
            "\n{nodeid}\n  fails in isolation (`rstest -n 0 {nodeid}`) — this is a plain \
             failure, not an order dependency. Fix the test itself."
        ));
        write_json(json_path, nodeid, false, &[], sink)?;
        return Ok(1);
    }

    let preds: Vec<String> = ids[..idx].to_vec();
    if preds.is_empty() {
        sink.out_line(&format!(
            "\n{nodeid}\n  is first in collection order — nothing runs before it, so there is \
             no polluter to bisect. If it still flakes, it's parallel-only (try \
             `rstest migrate-check`)."
        ));
        write_json(json_path, nodeid, false, &[], sink)?;
        return Ok(1);
    }

    // Baseline: does the victim fail when the whole preceding suite runs before
    // it (collection order)? If not, there's nothing to minimize here.
    sink.warn(&format!(
        "rstest bisect: reproducing with all {} preceding test(s)…",
        preds.len()
    ));
    if !reproduces(nodeid, &preds, &mut budget, sink)? {
        sink.out_line(&format!(
            "\n{nodeid}\n  passes at -n 0 after all {} preceding tests — the failure does not \
             reproduce from collection order alone. It is likely parallel-only \
             (concurrency, not ordering); run `rstest migrate-check`.",
            preds.len()
        ));
        write_json(json_path, nodeid, false, &[], sink)?;
        return Ok(1);
    }

    sink.warn("rstest bisect: delta-debugging the predecessor set…");
    let victim = nodeid.to_string();
    let culprits = ddmin(&preds, |subset| {
        reproduces(&victim, subset, &mut budget, sink)
    })?;

    let capped = budget == 0;
    sink.out_line("\n================= rstest bisect =================");
    sink.out_line(&format!("  victim:   {nodeid}"));
    sink.out_line(&format!(
        "  culprit{}: {} predecessor test(s) reproduce the failure:",
        if culprits.len() == 1 { "" } else { "s" },
        culprits.len()
    ));
    for c in &culprits {
        sink.out_line(&format!("    {c}"));
    }
    if capped {
        sink.out_line(
            "  (run budget reached — this is the smallest reproducing set found, \
             may not be 1-minimal)",
        );
    }
    sink.out_line("\n  Minimal reproducing order (serial):");
    let mut repro: Vec<&str> = culprits.iter().map(String::as_str).collect();
    repro.push(nodeid);
    sink.out_line(&format!("    rstest -n 0 {}", repro.join(" ")));
    sink.out_line(
        "\n  Fix: the culprit leaks state the victim depends on being clean \
         (a module global, a monkeypatch not undone, a cached singleton, an \
         env var, a temp file). Reset it in the culprit's teardown, or make \
         the victim set up its own state.",
    );
    sink.out_line("================================================");

    write_json(json_path, nodeid, true, &culprits, sink)?;
    Ok(0)
}

/// Write the `--bisect-json` document, when a path was given.
fn write_json(
    json_path: Option<&Path>,
    nodeid: &str,
    order_dependent: bool,
    culprits: &[String],
    _sink: &mut Sink,
) -> Result<()> {
    let Some(path) = json_path else {
        return Ok(());
    };
    let mut repro: Vec<&str> = culprits.iter().map(String::as_str).collect();
    repro.push(nodeid);
    let doc = serde_json::json!({
        "meta": { "runner": "rstest", "kind": "bisect", "schema": 1 },
        "nodeid": nodeid,
        "order_dependent": order_dependent,
        "culprits": culprits,
        "reproduce_command": if order_dependent {
            Some(format!("rstest -n 0 {}", repro.join(" ")))
        } else {
            None
        },
    });
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ddmin_finds_the_single_polluter() {
        // Interesting iff the subset contains "poison". ddmin must shrink a
        // 6-element predecessor list to exactly ["poison"].
        let preds = ids(&["a", "b", "poison", "c", "d", "e"]);
        let out = ddmin(&preds, |s| Ok(s.iter().any(|x| x == "poison"))).unwrap();
        assert_eq!(out, ids(&["poison"]));
    }

    #[test]
    fn ddmin_keeps_an_interacting_pair() {
        // Interesting only when BOTH "p" and "q" are present (a two-test
        // interaction). ddmin can't drop either, so both survive.
        let preds = ids(&["p", "x", "y", "q", "z"]);
        let out = ddmin(&preds, |s| {
            Ok(s.iter().any(|v| v == "p") && s.iter().any(|v| v == "q"))
        })
        .unwrap();
        assert!(out.contains(&"p".to_string()) && out.contains(&"q".to_string()));
        // and it minimized away the irrelevant tests.
        assert!(!out.contains(&"x".to_string()));
    }

    #[test]
    fn ddmin_single_element_is_returned_as_is() {
        let preds = ids(&["only"]);
        let out = ddmin(&preds, |s| Ok(!s.is_empty())).unwrap();
        assert_eq!(out, ids(&["only"]));
    }

    #[test]
    fn ddmin_propagates_oracle_errors() {
        let preds = ids(&["a", "b"]);
        let r = ddmin(&preds, |_| Err(anyhow::anyhow!("boom")));
        assert!(r.is_err());
    }
}
