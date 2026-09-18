//! `migrate-check`: the parallel-readiness preflight (M1) orchestrator.
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
use crate::reporting::sink::Sink;

/// Run the migration preflight. Exit code: 0 = ready, 1 = at least one blocker
/// (WILL-bail id or parallel-only failure). `json_path` writes findings as JSON.
/// `allow` holds accepted-finding substrings: reported but excluded from the gate.
pub fn run_migrate_check(
    python: &Path,
    args: &[String],
    json_path: Option<&Path>,
    allow: &[String],
    sink: &mut Sink,
) -> Result<i32> {
    let allowed = |s: &str| allow.iter().any(|p| s.contains(p.as_str()));
    sink.warn("rstest migrate-check: collecting twice to detect unstable test ids…");
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

    sink.out_line(&format!(
        "suite: {} tests collected, {stable} stable across both runs",
        union.len()
    ));

    // Group by site (worst Kind + a sample param each) and its structured form.
    let (by_site, will_bail_total) = accumulate_unstable(&unstable);
    let json_unstable = unstable_json(&by_site, allowed);
    let tests_total = union.len();
    // Writes the JSON doc (if requested) and returns the exit code. `parallel`
    // is null when the parallel phase was skipped (WILL-bail) or didn't run.
    let finish = |ready: bool, parallel: serde_json::Value, exit: i32| -> Result<i32> {
        if let Some(path) = json_path {
            let doc = check_doc(
                ready,
                tests_total,
                will_bail_total,
                &json_unstable,
                parallel,
            );
            std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        }
        Ok(exit)
    };

    if unstable.is_empty() {
        sink.out_line("  UNSTABLE NODEIDS: none — collection is reproducible.\n");
    } else {
        sink.out_line(&format!(
            "  UNSTABLE NODEIDS: {} across {} sites ({} per-process => WILL bail at -n auto)\n",
            unstable.len(),
            by_site.len(),
            will_bail_total
        ));
        for (site, acc) in &by_site {
            let kinds: Vec<String> = acc.counts.iter().map(|(k, n)| format!("{k}:{n}")).collect();
            let will = acc.will_bail();
            let verdict = if will {
                "WILL bail"
            } else {
                "may bail (timing)"
            };
            let mut sample = acc.sample.clone();
            crate::text::truncate_on_boundary(&mut sample, 90);
            sink.out_line(&format!("  {site}"));
            sink.out_line(&format!("    {}   -> {verdict}", kinds.join(", ")));
            sink.out_line(&format!("    e.g. [{sample}]"));
            sink.out_line(&format!("    FIX (upstream): {}", acc.worst.fix()));
            if will {
                sink.out_line("    STOPGAP (rstest): -n 0\n");
            } else {
                sink.out_line(
                    "    STOPGAP (rstest): usually runs at -n auto (the id is stable enough \
                     within a run); -n 0 only if it bails\n",
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
            .filter(|(site, acc)| acc.will_bail() && !allowed(site))
            .count();
        sink.out_line(&format!(
            "==> {will_bail_total} per-process-unstable id(s) force -n 0. Fix these (stable ids=) \
             before parallel will run; skipping the parallel check."
        ));
        if blocking == 0 {
            sink.out_line("    (all allow-listed — gate passes.)");
        }
        return finish(
            false,
            serde_json::Value::Null,
            if blocking > 0 { 1 } else { 0 },
        );
    }

    // Phase 2: run -n auto and classify any parallel-only failures.
    sink.warn("rstest migrate-check: running -n auto to check parallel behaviour…");
    let par = run_session(&[], args)?;
    if par.is_empty() {
        sink.out_line(
            "PARALLEL: could not capture outcomes (no snapshot) — run `rstest` manually.",
        );
        return finish(false, serde_json::json!({ "ran": false }), 1);
    }
    let verdicts = classify_failures(args, &par, sink)?;
    if verdicts.is_empty() {
        sink.out_line(&format!(
            "PARALLEL: ready — {} tests pass at -n auto.",
            par.len()
        ));
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
        sink.out_line("PARALLEL: ready — every test that passes at -n 0 also passes at -n auto.");
        if preexisting > 0 {
            sink.out_line(&format!(
                "  ({preexisting} test(s) already fail at -n 0 — pre-existing, not a parallelism \
                 issue; see `rstest -n 0`.)"
            ));
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
        sink.warn(&format!(
            "  bisecting the polluting file for {n} victim(s)…"
        ));
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
    sink.out_line(&format!(
        "PARALLEL: {} test(s) fail only under parallelism, classified:\n",
        migration.len()
    ));
    for (title, tests) in &by_verdict {
        let (why, fix) = advice[title];
        sink.out_line(&format!("  {title} ({} test(s))", tests.len()));
        sink.out_line(&format!("    {why}"));
        sink.out_line(&format!("    FIX: {fix}"));
        for t in tests.iter().take(8) {
            let tag = if allowed(t) { "  (allowed)" } else { "" };
            match polluter.get(*t) {
                Some(Polluter::OtherFile(f)) => {
                    sink.out_line(&format!("      {t}{tag}\n        POLLUTED BY: {f}"))
                }
                Some(Polluter::SameFile(f)) => sink.out_line(&format!(
                    "      {t}{tag}\n        SAME-FILE co-location (inspect {f})"
                )),
                Some(Polluter::NotReproducible) => sink.out_line(&format!(
                    "      {t}{tag}\n        (not reproducible serially — likely a \
                     concurrent-resource race, not state pollution)"
                )),
                None => sink.out_line(&format!("      {t}{tag}")),
            }
        }
        if tests.len() > 8 {
            sink.out_line(&format!("      … and {} more", tests.len() - 8));
        }
        sink.out_line("");
    }
    if preexisting > 0 {
        sink.out_line(&format!(
            "  (plus {preexisting} test(s) already failing at -n 0 — pre-existing, not shown.)"
        ));
    }

    let json_findings = findings_json(&migration, &polluter, allowed);
    // Gate: fail only on findings that aren't allow-listed.
    let blocking = migration.iter().filter(|(n, _)| !allowed(n)).count();
    if blocking == 0 {
        sink.out_line(&format!(
            "  (all {} finding(s) allow-listed — gate passes.)",
            migration.len()
        ));
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

/// Per-site accumulation of unstable ids: how many of each kind, the worst
/// (a will-bail kind beats a may-bail one) kind, and a sample param for it.
struct Acc {
    counts: BTreeMap<&'static str, usize>,
    worst: Kind,
    sample: String,
}

impl Acc {
    /// A site is a WILL-bail (per-process) blocker if any of its ids is an
    /// address or uuid; those force rstest to `-n 0`.
    fn will_bail(&self) -> bool {
        self.counts.keys().any(|k| *k == "address" || *k == "uuid")
    }
}

/// Group unstable nodeids by their parametrize site, returning the per-site
/// accumulation and the total count of will-bail (per-process) ids across all
/// sites. Pure: the classifier decides each id's kind from its param text.
fn accumulate_unstable<'a>(unstable: &[&'a str]) -> (BTreeMap<&'a str, Acc>, usize) {
    let mut by_site: BTreeMap<&str, Acc> = BTreeMap::new();
    let mut will_bail_total = 0usize;
    for id in unstable {
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
    (by_site, will_bail_total)
}

/// The structured (`--migrate-check-json`) form of the unstable-id findings.
fn unstable_json(
    by_site: &BTreeMap<&str, Acc>,
    allowed: impl Fn(&str) -> bool,
) -> Vec<serde_json::Value> {
    by_site
        .iter()
        .map(|(site, acc)| {
            serde_json::json!({
                "site": site,
                "kinds": acc.counts.iter().map(|(k, n)| (*k, *n)).collect::<BTreeMap<_, _>>(),
                "will_bail": acc.will_bail(),
                "allowed": allowed(site),
                "sample": acc.sample,
                "fix": acc.worst.fix(),
            })
        })
        .collect()
}

/// The `--migrate-check-json` envelope (schema 5's migrate-check variant).
/// `parallel` is null when the parallel phase was skipped or didn't run.
fn check_doc(
    ready: bool,
    tests: usize,
    will_bail: usize,
    unstable: &[serde_json::Value],
    parallel: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "meta": { "runner": "rstest", "kind": "migrate-check", "schema": 1 },
        "ready": ready,
        "tests_collected": tests,
        "will_bail_count": will_bail,
        "unstable_ids": unstable,
        "parallel": parallel,
    })
}

/// The JSON shape of a victim's polluter (null when the bisect found none).
fn polluter_json(p: Option<&Polluter>) -> serde_json::Value {
    match p {
        Some(Polluter::OtherFile(f)) => serde_json::json!({ "kind": "other_file", "file": f }),
        Some(Polluter::SameFile(f)) => serde_json::json!({ "kind": "same_file", "file": f }),
        Some(Polluter::NotReproducible) => serde_json::json!({ "kind": "not_reproducible" }),
        None => serde_json::Value::Null,
    }
}

/// The structured form of the parallel-only findings (verdict + advice + the
/// bisected polluter per test).
fn findings_json(
    migration: &[&(String, Verdict)],
    polluter: &BTreeMap<&str, Polluter>,
    allowed: impl Fn(&str) -> bool,
) -> Vec<serde_json::Value> {
    migration
        .iter()
        .map(|(nodeid, v)| {
            let (why, fix) = v.advice();
            serde_json::json!({
                "nodeid": nodeid,
                "verdict": v.title(),
                "why": why,
                "fix": fix,
                "allowed": allowed(nodeid),
                "polluter": polluter_json(polluter.get(nodeid.as_str())),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulate_unstable_groups_by_site_and_counts_will_bail() {
        // Two ids on one site (an address => will-bail, and a plain label), plus
        // a uuid id on a second site. will_bail_total counts address + uuid.
        let ids = [
            "a.py::t[<obj at 0x10ae4e660>]",
            "a.py::t[plain-label]",
            "b.py::u[efc8cccd-21d0-45ee-a84e-5b9e5f2ce0fd]",
        ];
        let (by_site, will_bail) = accumulate_unstable(&ids);
        assert_eq!(will_bail, 2, "address + uuid are per-process unstable");
        assert_eq!(by_site.len(), 2, "grouped into two sites");
        let a = &by_site["a.py::t"];
        assert!(a.will_bail(), "the address id makes the site will-bail");
        assert_eq!(a.counts["address"], 1);
        assert_eq!(a.counts["other"], 1);
        // worst tracks the will-bail kind and keeps ITS sample (the address).
        assert!(a.sample.contains("0x10ae4e660"));
        let b = &by_site["b.py::u"];
        assert!(b.will_bail());
        assert_eq!(b.counts["uuid"], 1);
    }

    #[test]
    fn accumulate_unstable_may_bail_site_is_not_will_bail() {
        let ids = ["a.py::t[2026-09-08]"]; // time -> may bail, not will
        let (by_site, will_bail) = accumulate_unstable(&ids);
        assert_eq!(will_bail, 0);
        assert!(!by_site["a.py::t"].will_bail());
        assert_eq!(by_site["a.py::t"].counts["time"], 1);
    }

    #[test]
    fn unstable_json_carries_site_kinds_and_allow_flag() {
        let ids = ["a.py::t[<obj at 0x10ae4e660>]", "b.py::u[plain]"];
        let (by_site, _) = accumulate_unstable(&ids);
        let docs = unstable_json(&by_site, |s| s == "b.py::u");
        assert_eq!(docs.len(), 2);
        let a = docs.iter().find(|d| d["site"] == "a.py::t").unwrap();
        assert_eq!(a["will_bail"], true);
        assert_eq!(a["allowed"], false);
        assert_eq!(a["kinds"]["address"], 1);
        assert!(a["fix"].as_str().unwrap().contains("repr"));
        let b = docs.iter().find(|d| d["site"] == "b.py::u").unwrap();
        assert_eq!(b["will_bail"], false);
        assert_eq!(b["allowed"], true, "the allow predicate marks this site");
    }

    #[test]
    fn check_doc_has_versioned_envelope() {
        let unstable = vec![serde_json::json!({ "site": "a.py::t" })];
        let doc = check_doc(false, 12, 3, &unstable, serde_json::Value::Null);
        assert_eq!(doc["meta"]["schema"], 1);
        assert_eq!(doc["meta"]["runner"], "rstest");
        assert_eq!(doc["ready"], false);
        assert_eq!(doc["tests_collected"], 12);
        assert_eq!(doc["will_bail_count"], 3);
        assert_eq!(doc["unstable_ids"][0]["site"], "a.py::t");
        assert!(doc["parallel"].is_null());
    }

    #[test]
    fn polluter_json_maps_each_variant() {
        assert_eq!(
            polluter_json(Some(&Polluter::OtherFile("x.py".into()))),
            serde_json::json!({ "kind": "other_file", "file": "x.py" })
        );
        assert_eq!(
            polluter_json(Some(&Polluter::SameFile("y.py".into()))),
            serde_json::json!({ "kind": "same_file", "file": "y.py" })
        );
        assert_eq!(
            polluter_json(Some(&Polluter::NotReproducible)),
            serde_json::json!({ "kind": "not_reproducible" })
        );
        assert!(polluter_json(None).is_null());
    }

    #[test]
    fn findings_json_joins_verdict_advice_and_polluter() {
        let migration_owned = [
            ("a.py::victim".to_string(), Verdict::Isolation),
            ("b.py::order".to_string(), Verdict::OrderDependency),
        ];
        let migration: Vec<&(String, Verdict)> = migration_owned.iter().collect();
        let mut polluter: BTreeMap<&str, Polluter> = BTreeMap::new();
        polluter.insert("a.py::victim", Polluter::OtherFile("c.py".into()));
        let docs = findings_json(&migration, &polluter, |n| n == "b.py::order");

        let v = &docs[0];
        assert_eq!(v["nodeid"], "a.py::victim");
        assert_eq!(v["verdict"], Verdict::Isolation.title());
        assert_eq!(v["allowed"], false);
        assert_eq!(v["polluter"]["kind"], "other_file");
        assert_eq!(v["polluter"]["file"], "c.py");
        let (why, fix) = Verdict::Isolation.advice();
        assert_eq!(v["why"], why);
        assert_eq!(v["fix"], fix);

        let o = &docs[1];
        assert_eq!(o["allowed"], true, "the allow predicate marks this finding");
        assert!(o["polluter"].is_null(), "no bisect entry -> null polluter");
    }
}
