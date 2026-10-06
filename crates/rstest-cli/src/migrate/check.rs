//! `migrate-check`: the parallel-readiness preflight (M1) orchestrator.
//!
//! Collects the suite twice in fresh sessions and diffs the id sets; ids
//! present in only one are run-to-run unstable. Per-process-unstable ones
//! (memory address / uuid) force rstest to `-n 0`; we name them and the fix.
//! The same ids in a different order (parametrize over a set, hash-ordered
//! iteration) bail too: the pool needs every worker to collect the identical
//! ordered list. Each session is a fresh interpreter, so an unset
//! `PYTHONHASHSEED` gives each its own random seed, exactly like the pool's
//! workers.
//! Then runs the suite in parallel (at least two workers) and classifies any
//! parallel-only failures.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use super::bisect::MAXFAIL_LIFT;
use super::classify::{
    bisect_polluter, classify, classify_failures, split_param, Kind, Polluter, Verdict,
};
use super::{collect_ids, run_session};
use crate::reporting::sink::Sink;

/// The `--migrate-check-json` document (schema 1). Field order is alphabetical
/// to match the historical `serde_json` map output (no `preserve_order`), so
/// the emitted bytes are unchanged by the move to typed structs.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct MigrateCheckDoc {
    /// Envelope: `kind` is `"migrate-check"`.
    pub meta: MigrateMeta,
    /// Parallel-phase result: `null` when the phase was skipped (WILL-bail ids
    /// force `-n 0`); `{"ran": false}` when it started but captured no outcomes.
    pub parallel: Option<ParallelReport>,
    /// Whether the suite is parallel-ready (no blocking findings).
    pub ready: bool,
    /// Tests collected (union across the two collection runs).
    pub tests_collected: usize,
    /// Unstable-nodeid findings, grouped by test site.
    pub unstable_ids: Vec<UnstableSite>,
    /// Count of ids that force `-n 0`: per-process-unstable ids plus ids at
    /// order-unstable sites (`order` kind).
    pub will_bail_count: usize,
}

/// Envelope metadata shared by the migrate documents.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct MigrateMeta {
    /// Constant discriminator: the producing subcommand (`"migrate-check"`,
    /// `"xdist-removal-check"`).
    pub kind: String,
    /// Constant producer tag: always `"rstest"`.
    pub runner: String,
    /// Document schema version.
    pub schema: u32,
}

/// Result of the parallel phase. Fields other than `ran` are absent
/// when the phase did not actually run to completion.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ParallelReport {
    /// Per-test parallel-only findings (empty when ready).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, schemars(with = "Vec<Finding>"))]
    pub findings: Option<Vec<Finding>>,
    /// Tests that already fail at `-n 0` (pre-existing, not a parallelism bug).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, schemars(with = "usize"))]
    pub preexisting: Option<usize>,
    /// Whether the parallel phase actually ran.
    pub ran: bool,
    /// Whether it passed (present only once the phase ran).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, schemars(with = "bool"))]
    pub ready: Option<bool>,
}

impl ParallelReport {
    /// The phase started but captured no outcomes (no snapshot).
    fn not_run() -> Self {
        Self {
            findings: None,
            preexisting: None,
            ran: false,
            ready: None,
        }
    }

    /// The phase ran and found no parallel-only failures.
    fn ready(preexisting: usize) -> Self {
        Self {
            findings: Some(vec![]),
            preexisting: Some(preexisting),
            ran: true,
            ready: Some(true),
        }
    }

    /// The phase ran and found parallel-only failures.
    fn blocked(findings: Vec<Finding>, preexisting: usize) -> Self {
        Self {
            findings: Some(findings),
            preexisting: Some(preexisting),
            ran: true,
            ready: Some(false),
        }
    }
}

/// One unstable-nodeid finding, grouped by test site (`file::test`).
#[derive(Serialize, Clone)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct UnstableSite {
    /// Whether this site matches a `--migrate-allow` entry.
    pub allowed: bool,
    /// The upstream fix for the worst instability kind here.
    pub fix: String,
    /// Count of unstable ids at this site, keyed by instability kind.
    pub kinds: BTreeMap<String, usize>,
    /// A sample parametrize id from this site.
    pub sample: String,
    /// The test site (`file::test`).
    pub site: String,
    /// Whether the site WILL bail at `-n auto` (per-process-unstable id).
    pub will_bail: bool,
}

/// One parallel-only failure finding.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Finding {
    /// Whether this nodeid is on the allow list.
    pub allowed: bool,
    /// The suggested fix.
    pub fix: String,
    /// The failing test's node id.
    pub nodeid: String,
    /// The bisected polluter, or `null` when none was found.
    pub polluter: Option<PolluterJson>,
    /// The classification verdict title.
    pub verdict: String,
    /// Why it fails only under parallelism.
    pub why: String,
}

/// A finding's polluter: the file that, run first, reproduces the failure.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PolluterJson {
    /// The polluting file (absent for `not_reproducible`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, schemars(with = "String"))]
    pub file: Option<String>,
    /// Polluter kind: `other_file`, `same_file`, or `not_reproducible`.
    pub kind: String,
}

/// Run the migration preflight. Exit code: 0 = ready, 1 = at least one blocker
/// (WILL-bail id or parallel-only failure), 2 = the parallel pass produced no
/// outcomes to judge. `json_path` writes findings as JSON.
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
    let (by_site, id_will_bail) = accumulate_unstable(&unstable);
    // Same ids, different order: the pool compares ordered lists, so it bails.
    let order_sites = order_unstable(&run1, &run2);
    let will_bail_total = id_will_bail + order_sites.iter().map(|o| o.ids).sum::<usize>();
    let mut json_unstable = unstable_json(&by_site, allowed);
    json_unstable.extend(order_json(&order_sites, allowed));
    let tests_total = union.len();
    // Writes the JSON doc (if requested) and returns the exit code. `parallel`
    // is null when the parallel phase was skipped (WILL-bail) or didn't run.
    let finish = |ready: bool, parallel: Option<ParallelReport>, exit: i32| -> Result<i32> {
        if let Some(path) = json_path {
            let doc = check_doc(
                ready,
                tests_total,
                will_bail_total,
                &json_unstable,
                parallel,
            );
            crate::reporting::write_output(path, serde_json::to_string_pretty(&doc)?)?;
        }
        Ok(exit)
    };

    if unstable.is_empty() && order_sites.is_empty() {
        sink.out_line("  UNSTABLE NODEIDS: none — collection is reproducible.\n");
    } else if !unstable.is_empty() {
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
    if !order_sites.is_empty() {
        sink.out_line(&format!(
            "  UNSTABLE ORDER: {} site(s) collected the same ids in a different order \
             (=> WILL bail at -n auto)\n",
            order_sites.len()
        ));
        for o in &order_sites {
            let mut sample = o.sample.to_string();
            crate::text::truncate_on_boundary(&mut sample, 90);
            sink.out_line(&format!("  {}", o.site));
            sink.out_line(&format!("    order:{}   -> WILL bail", o.ids));
            if !sample.is_empty() {
                sink.out_line(&format!("    e.g. [{sample}]"));
            }
            sink.out_line(&format!("    FIX (upstream): {ORDER_FIX}"));
            sink.out_line(&format!("    STOPGAP (rstest): {ORDER_STOPGAP}\n"));
        }
    }

    // A WILL-bail id means -n auto can't even dispatch - fix those first.
    if will_bail_total > 0 {
        // Allow-listed will-bail sites still force -n 0 mechanically, but don't
        // fail the gate (CI may have accepted them).
        let blocking = by_site
            .iter()
            .filter(|(site, acc)| acc.will_bail() && !allowed(site))
            .count()
            + order_sites.iter().filter(|o| !allowed(o.site)).count();
        if id_will_bail > 0 {
            sink.out_line(&format!(
                "==> {id_will_bail} per-process-unstable id(s) force -n 0. Fix these (stable \
                 ids=) before parallel will run; skipping the parallel check."
            ));
        }
        if !order_sites.is_empty() {
            sink.out_line(&format!(
                "==> {} site(s) collect in a run-to-run order: workers would disagree and the \
                 pool refuses to dispatch. Fix the order (or pin PYTHONHASHSEED) before \
                 parallel will run; skipping the parallel check.",
                order_sites.len()
            ));
        }
        if blocking == 0 {
            sink.out_line("    (all allow-listed — gate passes.)");
        }
        return finish(false, None, if blocking > 0 { 1 } else { 0 });
    }

    // Phase 2: run in parallel and classify any parallel-only failures.
    let n = super::parallel_n();
    sink.warn(&format!(
        "rstest migrate-check: running -n {n} to check parallel behaviour…"
    ));
    // Lift -x/--maxfail so the parallel pass covers the whole suite (see audit).
    let par_args: Vec<String> = args
        .iter()
        .cloned()
        .chain([MAXFAIL_LIFT.to_string()])
        .collect();
    let par = run_session(python, &["-n", &n], &par_args)?;
    if par.is_empty() {
        sink.out_line(
            "PARALLEL: could not capture outcomes (no snapshot) — run `rstest` manually.",
        );
        return finish(false, Some(ParallelReport::not_run()), 2);
    }
    let verdicts = classify_failures(python, args, &par, 1, sink)?;
    if verdicts.is_empty() {
        sink.out_line(&format!(
            "PARALLEL: ready — {} tests pass in parallel.",
            par.len()
        ));
        return finish(true, Some(ParallelReport::ready(0)), 0);
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
        sink.out_line("PARALLEL: ready — every test that passes at -n 0 also passes in parallel.");
        if preexisting > 0 {
            sink.out_line(&format!(
                "  ({preexisting} test(s) already fail at -n 0 — pre-existing, not a parallelism \
                 issue; see `rstest -n 0`.)"
            ));
        }
        return finish(true, Some(ParallelReport::ready(preexisting)), 0);
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
            polluter.insert(victim, bisect_polluter(python, args, victim, &par)?);
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
        Some(ParallelReport::blocked(json_findings, preexisting)),
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

/// Upstream fix for an order-unstable site.
const ORDER_FIX: &str = "parametrize over an ordered sequence (a list/tuple, or sorted(...)), \
     not a set or other hash-ordered iteration: set order of str/bytes values changes with \
     PYTHONHASHSEED, which is random per process";
/// Stopgap for an order-unstable site.
const ORDER_STOPGAP: &str = "pin one seed for the whole run (e.g. PYTHONHASHSEED=0), or -n 0";

/// One site whose ids came back in a different order across the two runs.
struct OrderSite<'a> {
    site: &'a str,
    /// Ids at this site (all of them ride the unstable order).
    ids: usize,
    /// The param of the first id that moved.
    sample: &'a str,
}

/// Sites whose ids (those present in both runs) came back in a different
/// order. The pool needs the identical ordered list on every worker, so each
/// of these WILL bail. When only the order across sites changed (whole files
/// or tests reordered), the site at the first divergence stands in.
fn order_unstable<'a>(run1: &'a [String], run2: &'a [String]) -> Vec<OrderSite<'a>> {
    let set1: HashSet<&str> = run1.iter().map(String::as_str).collect();
    let set2: HashSet<&str> = run2.iter().map(String::as_str).collect();
    let seq1: Vec<&str> = run1
        .iter()
        .map(String::as_str)
        .filter(|id| set2.contains(id))
        .collect();
    let seq2: Vec<&str> = run2
        .iter()
        .map(String::as_str)
        .filter(|id| set1.contains(id))
        .collect();
    if seq1 == seq2 {
        return Vec::new();
    }
    fn first_moved<'a>(a: &[&'a str], b: &[&'a str]) -> Option<&'a str> {
        a.iter().zip(b).find(|(x, y)| x != y).map(|(x, _)| *x)
    }
    let mut per_site: BTreeMap<&str, (Vec<&str>, Vec<&str>)> = BTreeMap::new();
    for id in &seq1 {
        per_site.entry(split_param(id).0).or_default().0.push(id);
    }
    for id in &seq2 {
        per_site.entry(split_param(id).0).or_default().1.push(id);
    }
    let mut out: Vec<OrderSite> = per_site
        .iter()
        .filter_map(|(site, (a, b))| {
            first_moved(a, b).map(|id| OrderSite {
                site,
                ids: a.len(),
                sample: split_param(id).1,
            })
        })
        .collect();
    if out.is_empty() {
        if let Some(id) = first_moved(&seq1, &seq2) {
            let (site, sample) = split_param(id);
            out.push(OrderSite {
                site,
                ids: 1,
                sample,
            });
        }
    }
    out
}

/// The structured form of the order-unstable sites: ordinary unstable-id
/// entries with the `order` kind (always will-bail).
fn order_json(sites: &[OrderSite], allowed: impl Fn(&str) -> bool) -> Vec<UnstableSite> {
    sites
        .iter()
        .map(|o| UnstableSite {
            allowed: allowed(o.site),
            fix: ORDER_FIX.to_string(),
            kinds: BTreeMap::from([("order".to_string(), o.ids)]),
            sample: o.sample.to_string(),
            site: o.site.to_string(),
            will_bail: true,
        })
        .collect()
}

/// The structured (`--migrate-check-json`) form of the unstable-id findings.
fn unstable_json(
    by_site: &BTreeMap<&str, Acc>,
    allowed: impl Fn(&str) -> bool,
) -> Vec<UnstableSite> {
    by_site
        .iter()
        .map(|(site, acc)| UnstableSite {
            allowed: allowed(site),
            fix: acc.worst.fix().to_string(),
            kinds: acc
                .counts
                .iter()
                .map(|(k, n)| ((*k).to_string(), *n))
                .collect(),
            sample: acc.sample.clone(),
            site: (*site).to_string(),
            will_bail: acc.will_bail(),
        })
        .collect()
}

/// The `--migrate-check-json` envelope. `parallel` is `None` (serialized null)
/// when the parallel phase was skipped or didn't run.
fn check_doc(
    ready: bool,
    tests: usize,
    will_bail: usize,
    unstable: &[UnstableSite],
    parallel: Option<ParallelReport>,
) -> MigrateCheckDoc {
    MigrateCheckDoc {
        meta: MigrateMeta {
            kind: "migrate-check".to_string(),
            runner: "rstest".to_string(),
            schema: 1,
        },
        parallel,
        ready,
        tests_collected: tests,
        unstable_ids: unstable.to_vec(),
        will_bail_count: will_bail,
    }
}

/// The JSON shape of a victim's polluter (`None` -> null when no bisect hit).
fn polluter_json(p: Option<&Polluter>) -> Option<PolluterJson> {
    match p {
        Some(Polluter::OtherFile(f)) => Some(PolluterJson {
            file: Some(f.clone()),
            kind: "other_file".to_string(),
        }),
        Some(Polluter::SameFile(f)) => Some(PolluterJson {
            file: Some(f.clone()),
            kind: "same_file".to_string(),
        }),
        Some(Polluter::NotReproducible) => Some(PolluterJson {
            file: None,
            kind: "not_reproducible".to_string(),
        }),
        None => None,
    }
}

/// The structured form of the parallel-only findings (verdict + advice + the
/// bisected polluter per test).
fn findings_json(
    migration: &[&(String, Verdict)],
    polluter: &BTreeMap<&str, Polluter>,
    allowed: impl Fn(&str) -> bool,
) -> Vec<Finding> {
    migration
        .iter()
        .map(|(nodeid, v)| {
            let (why, fix) = v.advice();
            Finding {
                allowed: allowed(nodeid),
                fix: fix.to_string(),
                nodeid: nodeid.clone(),
                polluter: polluter_json(polluter.get(nodeid.as_str())),
                verdict: v.title().to_string(),
                why: why.to_string(),
            }
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

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn order_unstable_flags_site_with_same_ids_in_another_order() {
        let r1 = ids(&["a.py::t[x]", "a.py::t[y]", "a.py::u", "b.py::v[1]"]);
        let r2 = ids(&["a.py::t[y]", "a.py::t[x]", "a.py::u", "b.py::v[1]"]);
        let got = order_unstable(&r1, &r2);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].site, "a.py::t");
        assert_eq!(got[0].ids, 2);
        assert_eq!(got[0].sample, "x");
        let doc = serde_json::to_value(order_json(&got, |_| false)).unwrap();
        assert_eq!(doc[0]["kinds"]["order"], 2);
        assert_eq!(doc[0]["will_bail"], true);
        assert!(doc[0]["fix"].as_str().unwrap().contains("PYTHONHASHSEED"));
    }

    #[test]
    fn order_unstable_ignores_identical_order_and_set_only_differences() {
        let r1 = ids(&["a.py::t[x]", "a.py::t[y]"]);
        assert!(order_unstable(&r1, &r1).is_empty());
        // An id present in only one run is the id check's job, not order's.
        let r2 = ids(&["a.py::t[x]", "a.py::t[z]", "a.py::t[y]"]);
        assert!(order_unstable(&r1, &r2).is_empty());
    }

    #[test]
    fn order_unstable_cross_site_reorder_names_first_divergence() {
        let r1 = ids(&["a.py::t", "b.py::u"]);
        let r2 = ids(&["b.py::u", "a.py::t"]);
        let got = order_unstable(&r1, &r2);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].site, "a.py::t");
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
        let docs = serde_json::to_value(unstable_json(&by_site, |s| s == "b.py::u")).unwrap();
        let docs = docs.as_array().unwrap();
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
        let (by_site, _) = accumulate_unstable(&["a.py::t[<obj at 0x1>]"]);
        let unstable = unstable_json(&by_site, |_| false);
        let doc = serde_json::to_value(check_doc(false, 12, 3, &unstable, None)).unwrap();
        assert_eq!(doc["meta"]["schema"], 1);
        assert_eq!(doc["meta"]["runner"], "rstest");
        assert_eq!(doc["meta"]["kind"], "migrate-check");
        assert_eq!(doc["ready"], false);
        assert_eq!(doc["tests_collected"], 12);
        assert_eq!(doc["will_bail_count"], 3);
        assert_eq!(doc["unstable_ids"][0]["site"], "a.py::t");
        assert!(doc["parallel"].is_null());
    }

    #[test]
    fn parallel_report_shapes_per_outcome() {
        // Not run: only `ran` is emitted; the rest are skipped, not null.
        assert_eq!(
            serde_json::to_value(ParallelReport::not_run()).unwrap(),
            serde_json::json!({ "ran": false })
        );
        assert_eq!(
            serde_json::to_value(ParallelReport::ready(2)).unwrap(),
            serde_json::json!({ "ran": true, "ready": true, "findings": [], "preexisting": 2 })
        );
        let finding = Finding {
            allowed: false,
            fix: "f".into(),
            nodeid: "a.py::t".into(),
            polluter: None,
            verdict: "v".into(),
            why: "w".into(),
        };
        let blocked = serde_json::to_value(ParallelReport::blocked(vec![finding], 1)).unwrap();
        assert_eq!(blocked["ran"], true);
        assert_eq!(blocked["ready"], false);
        assert_eq!(blocked["preexisting"], 1);
        assert_eq!(blocked["findings"][0]["nodeid"], "a.py::t");
        assert!(blocked["findings"][0]["polluter"].is_null());
    }

    #[test]
    fn polluter_json_maps_each_variant() {
        fn to_value(p: Option<&Polluter>) -> serde_json::Value {
            serde_json::to_value(polluter_json(p)).unwrap()
        }
        assert_eq!(
            to_value(Some(&Polluter::OtherFile("x.py".into()))),
            serde_json::json!({ "kind": "other_file", "file": "x.py" })
        );
        assert_eq!(
            to_value(Some(&Polluter::SameFile("y.py".into()))),
            serde_json::json!({ "kind": "same_file", "file": "y.py" })
        );
        assert_eq!(
            to_value(Some(&Polluter::NotReproducible)),
            serde_json::json!({ "kind": "not_reproducible" })
        );
        assert!(to_value(None).is_null());
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
        let docs =
            serde_json::to_value(findings_json(&migration, &polluter, |n| n == "b.py::order"))
                .unwrap();
        let docs = docs.as_array().unwrap();

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
