//! `--migrate-check`: the parallel-readiness preflight (M1) orchestrator.
//!
//! Collects the suite twice in fresh sessions and diffs the id sets; ids
//! present in only one are run-to-run unstable. Per-process-unstable ones
//! (memory address / uuid) force rstest to `-n 0`; we name them and the fix.
//! Then runs `-n auto` and classifies any parallel-only failures.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::Result;

use super::classify::{
    bisect_polluter, classify, classify_failures, split_param, Kind, Polluter, Verdict,
};
use super::{collect_ids, run_session};

/// Run the migration preflight. Exit code: 0 = ready, 1 = at least one blocker
/// (WILL-bail id or parallel-only failure). `json_path` writes findings as JSON.
/// `allow` holds accepted-finding substrings: reported but excluded from the gate.
pub fn run_migrate_check(
    python: &Path,
    args: &[String],
    json_path: Option<&Path>,
    allow: &[String],
) -> Result<i32> {
    let allowed = |s: &str| allow.iter().any(|p| s.contains(p.as_str()));
    eprintln!("rstest migrate-check: collecting twice to detect unstable test ids…");
    let run1 = collect_ids(python, args)?;
    let run2 = collect_ids(python, args)?;

    let set1: HashSet<&str> = run1.iter().map(String::as_str).collect();
    let set2: HashSet<&str> = run2.iter().map(String::as_str).collect();
    let union: HashSet<&str> = set1.union(&set2).copied().collect();
    let stable = set1.intersection(&set2).count();
    // Unstable = present in exactly one collection.
    let unstable: Vec<&str> = union
        .iter()
        .copied()
        .filter(|id| !(set1.contains(id) && set2.contains(id)))
        .collect();

    println!(
        "suite: {} tests collected, {stable} stable across both runs",
        union.len()
    );

    // Group by site; per site track the worst Kind and a sample param.
    struct Acc {
        counts: BTreeMap<&'static str, usize>,
        worst: Kind,
        sample: String,
    }
    let mut by_site: BTreeMap<&str, Acc> = BTreeMap::new();
    let mut will_bail_total = 0usize;
    for id in &unstable {
        let (site, param) = split_param(id);
        let kind = classify(param);
        if kind.will_bail() {
            will_bail_total += 1;
        }
        let acc = by_site.entry(site).or_insert_with(|| Acc {
            counts: BTreeMap::new(),
            worst: Kind::Other,
            sample: param.to_string(),
        });
        *acc.counts.entry(kind.label()).or_insert(0) += 1;
        // worst = a will-bail kind beats a may-bail one; remember its sample.
        if kind.will_bail() && !acc.worst.will_bail() {
            acc.worst = kind;
            acc.sample = param.to_string();
        }
    }

    // Structured form of the unstable-id findings (for --migrate-check-json).
    let json_unstable: Vec<serde_json::Value> = by_site
        .iter()
        .map(|(site, acc)| {
            let will = acc.counts.keys().any(|k| *k == "address" || *k == "uuid");
            serde_json::json!({
                "site": site,
                "kinds": acc.counts.iter().map(|(k, n)| (*k, *n)).collect::<BTreeMap<_, _>>(),
                "will_bail": will,
                "allowed": allowed(site),
                "sample": acc.sample,
                "fix": acc.worst.fix(),
            })
        })
        .collect();
    let tests_total = union.len();
    // Writes the JSON doc (if requested) and returns the exit code. `parallel`
    // is null when the parallel phase was skipped (WILL-bail) or didn't run.
    let finish = |ready: bool, parallel: serde_json::Value, exit: i32| -> Result<i32> {
        if let Some(path) = json_path {
            let doc = serde_json::json!({
                "meta": { "runner": "rstest", "kind": "migrate-check", "schema": 1 },
                "ready": ready,
                "tests_collected": tests_total,
                "will_bail_count": will_bail_total,
                "unstable_ids": json_unstable,
                "parallel": parallel,
            });
            std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        }
        Ok(exit)
    };

    if unstable.is_empty() {
        println!("  UNSTABLE NODEIDS: none — collection is reproducible.\n");
    } else {
        println!(
            "  UNSTABLE NODEIDS: {} across {} sites ({} per-process => WILL bail at -n auto)\n",
            unstable.len(),
            by_site.len(),
            will_bail_total
        );
        for (site, acc) in &by_site {
            let kinds: Vec<String> = acc.counts.iter().map(|(k, n)| format!("{k}:{n}")).collect();
            let will = acc.counts.keys().any(|k| *k == "address" || *k == "uuid");
            let verdict = if will {
                "WILL bail"
            } else {
                "may bail (timing)"
            };
            let mut sample = acc.sample.clone();
            if sample.len() > 90 {
                sample.truncate(90);
            }
            println!("  {site}");
            println!("    {}   -> {verdict}", kinds.join(", "));
            println!("    e.g. [{sample}]");
            println!("    FIX (upstream): {}", acc.worst.fix());
            if will {
                println!("    STOPGAP (rstest): -n 0\n");
            } else {
                println!(
                    "    STOPGAP (rstest): usually runs at -n auto (the id is stable enough \
                     within a run); -n 0 only if it bails\n"
                );
            }
        }
    }

    // A WILL-bail id means -n auto can't even dispatch - fix those first.
    if will_bail_total > 0 {
        // Allow-listed will-bail sites still force -n 0 mechanically, but don't
        // fail the gate (CI may have accepted them).
        let blocking = by_site
            .iter()
            .filter(|(site, acc)| {
                acc.counts.keys().any(|k| *k == "address" || *k == "uuid") && !allowed(site)
            })
            .count();
        println!(
            "==> {will_bail_total} per-process-unstable id(s) force -n 0. Fix these (stable ids=) \
             before parallel will run; skipping the parallel check."
        );
        if blocking == 0 {
            println!("    (all allow-listed — gate passes.)");
        }
        return finish(
            false,
            serde_json::Value::Null,
            if blocking > 0 { 1 } else { 0 },
        );
    }

    // Phase 2: run -n auto and classify any parallel-only failures.
    eprintln!("rstest migrate-check: running -n auto to check parallel behaviour…");
    let par = run_session(&[], args)?;
    if par.is_empty() {
        println!("PARALLEL: could not capture outcomes (no snapshot) — run `rstest` manually.");
        return finish(false, serde_json::json!({ "ran": false }), 1);
    }
    let verdicts = classify_failures(args, &par)?;
    if verdicts.is_empty() {
        println!("PARALLEL: ready — {} tests pass at -n auto.", par.len());
        return finish(
            true,
            serde_json::json!({ "ran": true, "ready": true, "findings": [], "preexisting": 0 }),
            0,
        );
    }

    // Pre-existing failures (fail at -n 0 too) aren't a migration concern;
    // summarize, don't drown the real parallelism findings in them.
    let preexisting = verdicts
        .iter()
        .filter(|(_, v)| matches!(v, Verdict::NotParallel))
        .count();
    let migration: Vec<&(String, Verdict)> = verdicts
        .iter()
        .filter(|(_, v)| !matches!(v, Verdict::NotParallel))
        .collect();

    if migration.is_empty() {
        println!("PARALLEL: ready — every test that passes at -n 0 also passes at -n auto.");
        if preexisting > 0 {
            println!(
                "  ({preexisting} test(s) already fail at -n 0 — pre-existing, not a parallelism \
                 issue; see `rstest -n 0`.)"
            );
        }
        return finish(
            true,
            serde_json::json!({ "ran": true, "ready": true, "findings": [], "preexisting": preexisting }),
            0,
        );
    }

    // Bisect the polluting file for order + isolation victims (both reproduce
    // by running the right file before the victim). ~log(#files) runs each.
    const BISECT_CAP: usize = 3;
    let mut polluter: BTreeMap<&str, Polluter> = BTreeMap::new();
    let victims: Vec<&str> = migration
        .iter()
        .filter(|(_, v)| matches!(v, Verdict::Isolation | Verdict::OrderDependency))
        .map(|(n, _)| n.as_str())
        .collect();
    if !victims.is_empty() {
        let n = victims.len().min(BISECT_CAP);
        eprintln!("  bisecting the polluting file for {n} victim(s)…");
        for victim in victims.iter().take(BISECT_CAP) {
            polluter.insert(victim, bisect_polluter(args, victim, &par)?);
        }
    }

    let mut by_verdict: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut advice: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for (nodeid, v) in &migration {
        by_verdict.entry(v.title()).or_default().push(nodeid);
        advice.entry(v.title()).or_insert_with(|| v.advice());
    }
    println!(
        "PARALLEL: {} test(s) fail only under parallelism, classified:\n",
        migration.len()
    );
    for (title, tests) in &by_verdict {
        let (why, fix) = advice[title];
        println!("  {title} ({} test(s))", tests.len());
        println!("    {why}");
        println!("    FIX: {fix}");
        for t in tests.iter().take(8) {
            let tag = if allowed(t) { "  (allowed)" } else { "" };
            match polluter.get(*t) {
                Some(Polluter::OtherFile(f)) => {
                    println!("      {t}{tag}\n        POLLUTED BY: {f}")
                }
                Some(Polluter::SameFile(f)) => {
                    println!("      {t}{tag}\n        SAME-FILE co-location (inspect {f})")
                }
                Some(Polluter::NotReproducible) => println!(
                    "      {t}{tag}\n        (not reproducible serially — likely a \
                     concurrent-resource race, not state pollution)"
                ),
                None => println!("      {t}{tag}"),
            }
        }
        if tests.len() > 8 {
            println!("      … and {} more", tests.len() - 8);
        }
        println!();
    }
    if preexisting > 0 {
        println!(
            "  (plus {preexisting} test(s) already failing at -n 0 — pre-existing, not shown.)"
        );
    }

    let json_findings: Vec<serde_json::Value> = migration
        .iter()
        .map(|(nodeid, v)| {
            let (why, fix) = v.advice();
            let pol = match polluter.get(nodeid.as_str()) {
                Some(Polluter::OtherFile(f)) => {
                    serde_json::json!({ "kind": "other_file", "file": f })
                }
                Some(Polluter::SameFile(f)) => {
                    serde_json::json!({ "kind": "same_file", "file": f })
                }
                Some(Polluter::NotReproducible) => {
                    serde_json::json!({ "kind": "not_reproducible" })
                }
                None => serde_json::Value::Null,
            };
            serde_json::json!({
                "nodeid": nodeid,
                "verdict": v.title(),
                "why": why,
                "fix": fix,
                "allowed": allowed(nodeid),
                "polluter": pol,
            })
        })
        .collect();
    // Gate: fail only on findings that aren't allow-listed.
    let blocking = migration.iter().filter(|(n, _)| !allowed(n)).count();
    if blocking == 0 {
        println!(
            "  (all {} finding(s) allow-listed — gate passes.)",
            migration.len()
        );
    }
    finish(
        false,
        serde_json::json!({
            "ran": true,
            "ready": false,
            "findings": json_findings,
            "preexisting": preexisting,
        }),
        if blocking > 0 { 1 } else { 0 },
    )
}
