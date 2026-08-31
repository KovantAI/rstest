//! `--migrate-check`: parallel-readiness preflight (M1).
//!
//! Collects the suite twice in fresh sessions and diffs the id sets; ids
//! present in only one are run-to-run unstable. Per-process-unstable ones
//! (memory address / uuid) force rstest to `-n 0`; we name them and the fix.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;

use crate::{proto, worker};

/// Per-test record from a run snapshot: pass/fail plus timing (for the
/// wait-bound / wall-clock signal). A test absent from a run isn't in the map.
type Outcomes = BTreeMap<String, Rec>;

#[derive(Clone, Copy)]
struct Rec {
    phase: Phase,
    wall: f64,        // call-phase wall seconds
    cpu: Option<f64>, // call-phase cpu seconds (only with doctor instrumentation)
}

impl Rec {
    /// Wait-bound: spent its time blocked, not computing - the signature of a
    /// wall-clock/timeout test. Needs cpu data (doctor) and a non-trivial wall.
    fn wait_bound(&self) -> bool {
        matches!(self.cpu, Some(c) if self.wall >= 0.05 && c < 0.5 * self.wall)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pass,
    Fail, // failed or errored in any phase
}

fn is_fail(entry: &serde_json::Value) -> bool {
    ["setup", "call", "teardown"].iter().any(|p| {
        matches!(
            entry.get(p).and_then(|v| v.as_str()),
            Some("failed") | Some("error")
        )
    })
}

/// Why a parametrize id is unstable. `WILL` bail are per-process (differ in
/// every worker); `MAY` bail depend on collection timing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Address, // 0x... - repr() fallback id; differs every process
    Uuid,    // uuid4 in the id
    Time,    // timestamp / date in the id
    Other,   // unstable for an unrecognized reason
}

impl Kind {
    fn will_bail(self) -> bool {
        matches!(self, Kind::Address | Kind::Uuid)
    }
    fn label(self) -> &'static str {
        match self {
            Kind::Address => "address",
            Kind::Uuid => "uuid",
            Kind::Time => "time",
            Kind::Other => "other",
        }
    }
    fn fix(self) -> &'static str {
        match self {
            Kind::Address => {
                "give this parametrize stable ids= (its default id falls back to \
                 repr(), hence the address)"
            }
            Kind::Uuid => "give this parametrize stable ids= (don't derive the id from a uuid)",
            Kind::Time => "freeze the clock for this parametrize source, or pass explicit ids=",
            Kind::Other => "pin this parametrize's ids= to stable labels",
        }
    }
}

struct Classifiers {
    address: Regex,
    uuid: Regex,
    time: Regex,
}

fn classifiers() -> &'static Classifiers {
    static C: OnceLock<Classifiers> = OnceLock::new();
    C.get_or_init(|| Classifiers {
        address: Regex::new(r"0x[0-9a-fA-F]{6,}").unwrap(),
        uuid: Regex::new(
            r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
        )
        .unwrap(),
        time: Regex::new(r"\b\d{4}-\d{2}-\d{2}\b|\b\d{2}:\d{2}:\d{2}\b|datetime|\d{10,}").unwrap(),
    })
}

fn classify(param: &str) -> Kind {
    let c = classifiers();
    if c.address.is_match(param) {
        Kind::Address
    } else if c.uuid.is_match(param) {
        Kind::Uuid
    } else if c.time.is_match(param) {
        Kind::Time
    } else {
        Kind::Other
    }
}

/// The parametrize site (nodeid with the trailing `[...]` stripped) and the
/// param segment. The param starts at the FIRST `[`: names/paths never contain
/// one, but a param's repr can (nested brackets), so `rfind` would split it.
fn split_param(nodeid: &str) -> (&str, &str) {
    if nodeid.ends_with(']') {
        if let Some(open) = nodeid.find('[') {
            return (&nodeid[..open], &nodeid[open + 1..nodeid.len() - 1]);
        }
    }
    (nodeid, "")
}

/// One fresh collect-only session -> the collected nodeids in session order.
fn collect_ids(python: &Path, args: &[String]) -> Result<Vec<String>> {
    // Full id+location payload from pytest_collection_finish (single session).
    std::env::set_var("RSTEST_SEND_IDS", "1");
    let mut collect_args = args.to_vec();
    if !collect_args
        .iter()
        .any(|a| a == "--collect-only" || a == "--co")
    {
        collect_args.push("--collect-only".into());
    }
    let mut w = worker::Worker::spawn_with_io(python, None, worker::Stdio::Null)?;
    w.send(&proto::Command::RunItemsSession { args: collect_args })?;
    let mut ids: Vec<String> = Vec::new();
    loop {
        match w.recv()? {
            proto::Event::CollectionDone { ids: Some(i), .. } => ids = i,
            proto::Event::Done { .. } => break,
            _ => {}
        }
    }
    w.shutdown()?;
    Ok(ids)
}

/// Run one full session in a child rstest process with the given config flags
/// (e.g. `["-n","0"]`), capture per-test pass/fail from its `--report-json`.
fn run_session(config: &[&str], args: &[String]) -> Result<Outcomes> {
    let exe = std::env::current_exe()?;
    let tmp = std::env::temp_dir().join(format!(
        "rstest-migrate-{}-{}.json",
        std::process::id(),
        run_session_seq()
    ));
    let mut cmd = std::process::Command::new(exe);
    cmd.args(config)
        .args(args)
        .arg("--report-json")
        .arg(&tmp)
        // worker-timeout: a fixed-port / deadlock test (httpx, werkzeug) would
        // otherwise hang the preflight; the stuck test becomes a failure.
        .args(["--worker-timeout", "120"])
        // dots off-tty keeps the child quiet & byte-stable; we discard stdout.
        .args(["-q", "--output", "dots"])
        // doctor instrumentation adds per-test cpu time (cheap) so the
        // classifier can tell a wait-bound (wall-clock) failure from a real
        // co-location/isolation one.
        .env("RSTEST_DOCTOR", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.status()?; // non-zero is expected when tests fail; the snapshot is truth
    let mut out = Outcomes::new();
    if let Ok(text) = std::fs::read_to_string(&tmp) {
        if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(tests) = doc.get("tests").and_then(|t| t.as_object()) {
                for (nodeid, entry) in tests {
                    out.insert(
                        nodeid.clone(),
                        Rec {
                            phase: if is_fail(entry) {
                                Phase::Fail
                            } else {
                                Phase::Pass
                            },
                            wall: entry
                                .get("duration")
                                .and_then(|v| v.as_f64())
                                .unwrap_or(0.0),
                            cpu: entry.get("cpu").and_then(|v| v.as_f64()),
                        },
                    );
                }
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Ok(out)
}

fn run_session_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// The classifier verdict for one test that failed under `-n auto`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    NotParallel,     // fails at -n 0 too (deterministically) - a plain bug / env
    IntrinsicFlake,  // serial runs disagree - flaky under any runner
    OrderDependency, // passes serial + loadfile, fails under load
    WallClock,       // passes serial, fails parallel, wait-bound - load-sensitive timing
    Isolation,       // passes serial, fails under load AND loadfile - co-location
}

impl Verdict {
    fn title(&self) -> &'static str {
        match self {
            Verdict::NotParallel => "NOT PARALLEL-SPECIFIC",
            Verdict::IntrinsicFlake => "INTRINSIC FLAKE",
            Verdict::OrderDependency => "ORDER DEPENDENCY",
            Verdict::WallClock => "WALL-CLOCK / LOAD-SENSITIVE",
            Verdict::Isolation => "ISOLATION / CO-LOCATION",
        }
    }
    fn advice(&self) -> (&'static str, &'static str) {
        match self {
            Verdict::NotParallel => (
                "fails at -n 0 too — not a parallelism issue; a plain bug or env gap",
                "fix the test (or its environment)",
            ),
            Verdict::IntrinsicFlake => (
                "serial repeats disagree — flaky under ANY runner, not parallelism",
                "fix the flake (mock the clock / remove the race); --reruns hides it",
            ),
            Verdict::OrderDependency => (
                "passes serial and under --dist loadfile, fails under -n auto (load)",
                "run this suite with --dist loadfile, or fix the in-file order coupling",
            ),
            Verdict::WallClock => (
                "passes serial, fails parallel, and is wait-bound (wall >> cpu) — \
                 a real-time deadline that misses when the machine is oversubscribed",
                "mock the clock / drop the tight upper bound; stopgap -n 4 or @pytest.mark.serial",
            ),
            Verdict::Isolation => (
                "passes serial, fails under both load and loadfile — co-located state leak",
                "reset the leaked global state per test; stopgap @pytest.mark.serial",
            ),
        }
    }
}

/// Classify the parallel-only failures. `par` = -n auto outcomes; the function
/// runs the discriminators (serial ×2, loadfile) and decides per failing test.
fn classify_failures(args: &[String], par: &Outcomes) -> Result<Vec<(String, Verdict)>> {
    let failed: Vec<&String> = par
        .iter()
        .filter(|(_, r)| r.phase == Phase::Fail)
        .map(|(n, _)| n)
        .collect();
    if failed.is_empty() {
        return Ok(Vec::new());
    }
    // M2: scope the discriminators to the FILES containing failures, not the
    // whole suite - cost ∝ failing files. A cross-file polluter in a
    // non-failing file may round ISOLATION down to ORDER-DEPENDENCY.
    let files: std::collections::BTreeSet<&str> = failed.iter().map(|n| file_of(n)).collect();
    let mut scoped: Vec<String> = files.iter().map(|s| s.to_string()).collect();
    scoped.extend_from_slice(args);
    eprintln!(
        "  {} parallel failure(s) in {} file(s); running discriminators (serial ×2, loadfile, \
         scoped to those files)…",
        failed.len(),
        files.len()
    );
    let s1 = run_session(&["-n", "0"], &scoped)?;
    let s2 = run_session(&["-n", "0"], &scoped)?;
    let lf = run_session(&["--dist", "loadfile"], &scoped)?;

    let fails = |o: &Outcomes, n: &str| matches!(o.get(n).map(|r| r.phase), Some(Phase::Fail));
    let wait_bound = |n: &str| par.get(n).map(|r| r.wait_bound()).unwrap_or(false);
    let mut out = Vec::new();
    for n in failed {
        let v = decide(fails(&s1, n), fails(&s2, n), fails(&lf, n), wait_bound(n));
        out.push((n.clone(), v));
    }
    Ok(out)
}

/// The pure classifier decision for a test that failed under `-n auto`, given
/// whether it also failed the two serial repeats, the loadfile run, and whether
/// its parallel run was wait-bound. Kept separate from the I/O so it's testable.
fn decide(serial1: bool, serial2: bool, loadfile: bool, wait_bound: bool) -> Verdict {
    if serial1 && serial2 {
        Verdict::NotParallel // fails deterministically even serially
    } else if serial1 || serial2 {
        Verdict::IntrinsicFlake // serial repeats disagree
    } else if !loadfile {
        Verdict::OrderDependency // passes serial + loadfile, fails only under load
    } else if wait_bound {
        // fails under load AND loadfile, passes serial - a co-location bug OR a
        // real-time deadline. Wait-bound (wall >> cpu) -> the latter.
        Verdict::WallClock
    } else {
        Verdict::Isolation
    }
}

/// The test file of a nodeid (everything before the first `::`).
fn file_of(nodeid: &str) -> &str {
    nodeid.split("::").next().unwrap_or(nodeid)
}

/// Where a victim's pollution comes from.
enum Polluter {
    SameFile(String),  // reproduces running the victim's own file alone
    OtherFile(String), // a different file, run before the victim, reproduces
    NotReproducible,   // no serial ordering reproduces - likely a concurrent race
}

/// Find the polluter: the file whose tests, run serially BEFORE the victim,
/// reproduce its failure. Checks the victim's own file first (same-file
/// co-location), then binary-searches the rest (polluter must precede victim).
fn bisect_polluter(args: &[String], victim: &str, all: &Outcomes) -> Result<Polluter> {
    let vfile = file_of(victim).to_string();

    // reproduce(subset): run `-n 0 <subset…> <vfile>` (vfile last so the
    // candidate files run first) - does the victim fail?
    let reproduce = |subset: &[String]| -> Result<bool> {
        let mut sel: Vec<String> = subset.to_vec();
        sel.push(vfile.clone());
        sel.extend_from_slice(args);
        let o = run_session(&["-n", "0"], &sel)?;
        Ok(matches!(o.get(victim).map(|r| r.phase), Some(Phase::Fail)))
    };

    // Same-file co-location: the victim's own file alone reproduces.
    if reproduce(&[])? {
        return Ok(Polluter::SameFile(vfile));
    }

    let mut files: Vec<String> = all
        .keys()
        .map(|n| file_of(n).to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|f| *f != vfile)
        .collect();
    if !reproduce(&files)? {
        return Ok(Polluter::NotReproducible);
    }
    let mut budget = 14; // ~2^14 files; bounds a pathological suite
    while files.len() > 1 && budget > 0 {
        let mid = files.len() / 2;
        let first: Vec<String> = files[..mid].to_vec();
        budget -= 1;
        if reproduce(&first)? {
            files = first;
            continue;
        }
        let second: Vec<String> = files[mid..].to_vec();
        budget -= 1;
        if reproduce(&second)? {
            files = second;
        } else {
            // Neither half alone reproduces - the polluter spans both
            // (interaction). Report the smallest confirmed set we have.
            break;
        }
    }
    Ok(files
        .into_iter()
        .next()
        .map(Polluter::OtherFile)
        .unwrap_or(Polluter::NotReproducible))
}

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

/// Run a command, return (parsed outcomes from its --report-json/recorder
/// snapshot, wall seconds, exit code). `record_path` is where the run wrote its
/// JSON.
fn time_run(mut cmd: std::process::Command, record_path: &Path) -> (Option<Outcomes>, f64, i32) {
    let t0 = std::time::Instant::now();
    let code = cmd.status().ok().and_then(|s| s.code()).unwrap_or(-1);
    let wall = t0.elapsed().as_secs_f64();
    let outcomes = std::fs::read_to_string(record_path).ok().and_then(|txt| {
        let doc: serde_json::Value = serde_json::from_str(&txt).ok()?;
        let tests = doc.get("tests")?.as_object()?;
        let mut out = Outcomes::new();
        for (nodeid, e) in tests {
            out.insert(
                nodeid.clone(),
                Rec {
                    phase: if is_fail(e) { Phase::Fail } else { Phase::Pass },
                    wall: e.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    cpu: None,
                },
            );
        }
        Some(out)
    });
    (outcomes, wall, code)
}

/// Estimate CI runs/day from git history: commits in the last 30 days ÷ 30
/// (CI typically runs once per push ≈ per commit). Returns (per_day, count) or
/// None outside a git repo / with no recent history.
fn commits_per_day() -> Option<(f64, u64)> {
    let out = std::process::Command::new("git")
        .args(["rev-list", "--count", "--since=30.days.ago", "HEAD"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let n: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (n > 0).then_some((n as f64 / 30.0, n))
}

fn fmt_secs(s: f64) -> String {
    if s >= 60.0 {
        format!("{}m{:02.0}s", (s / 60.0).floor(), s % 60.0)
    } else {
        format!("{s:.1}s")
    }
}

/// `rstest --try`: run the suite under plain pytest and under rstest (-n auto),
/// report whether outcomes are identical and the speedup. The 30-second
/// "should I switch?" proof.
pub fn run_try(python: &Path, args: &[String]) -> Result<i32> {
    let tmpdir = std::env::temp_dir();
    let pid = std::process::id();
    let py_json = tmpdir.join(format!("rstest-try-pytest-{pid}.json"));
    let rs_json = tmpdir.join(format!("rstest-try-rstest-{pid}.json"));

    eprintln!("rstest --try: running your suite under pytest…");
    let mut py = std::process::Command::new(python);
    py.args(["-m", "pytest", "-p", "rstest_worker.recorder", "-q"])
        .args(args)
        .env("RSTEST_RECORD", &py_json)
        .env("PYTHONPATH", worker::worker_pythonpath())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let (py_out, py_wall, py_code) = time_run(py, &py_json);
    let _ = std::fs::remove_file(&py_json);

    let Some(py_out) = py_out else {
        println!(
            "rstest --try: couldn't run pytest (is it installed and your suite collectable?).\n\
             Try `python -m pytest -q` yourself, then re-run `rstest --try`."
        );
        return Ok(2);
    };

    eprintln!("rstest --try: running it under rstest (-n auto)…");
    let exe = std::env::current_exe()?;
    let mut rs = std::process::Command::new(exe);
    rs.arg("-n")
        .arg("auto")
        .args(args)
        .arg("--report-json")
        .arg(&rs_json)
        .args(["-q", "--output", "dots"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let (rs_out, rs_wall, _rs_code) = time_run(rs, &rs_json);
    let _ = std::fs::remove_file(&rs_json);

    let Some(rs_out) = rs_out else {
        println!(
            "rstest --try: rstest produced no run (it may have refused to dispatch — \
             often an unstable parametrize id). Run `rstest --migrate-check` to see why."
        );
        return Ok(2);
    };

    // ---- parity ----
    let pk: std::collections::BTreeSet<&str> = py_out.keys().map(String::as_str).collect();
    let rk: std::collections::BTreeSet<&str> = rs_out.keys().map(String::as_str).collect();
    let only_py = pk.difference(&rk).count();
    let only_rs = rk.difference(&pk).count();
    let mut diffs = 0usize;
    for id in pk.intersection(&rk) {
        if py_out[*id].phase != rs_out[*id].phase {
            diffs += 1;
        }
    }
    let identical = only_py == 0 && only_rs == 0 && diffs == 0;
    let total = pk.union(&rk).count();

    println!("\n================= rstest --try =================");
    if identical {
        println!("  ✓ parity:  {total} tests — identical outcomes to pytest");
    } else {
        println!(
            "  ⚠ parity:  {} of {total} tests differ ({diffs} different outcome, \
             {only_py} only in pytest, {only_rs} only in rstest)",
            diffs + only_py + only_rs
        );
    }

    // ---- speed ----
    let speedup = if rs_wall > 0.0 {
        py_wall / rs_wall
    } else {
        0.0
    };
    println!(
        "  ⚡ speed:   pytest {}  →  rstest {}   ({speedup:.1}× at -n auto)",
        fmt_secs(py_wall),
        fmt_secs(rs_wall)
    );
    let saved = (py_wall - rs_wall).max(0.0);
    if saved >= 1.0 {
        match commits_per_day() {
            // Project over the repo's actual recent activity (commits ≈ CI
            // runs). Monthly total avoids rounding a low cadence to "0/day".
            Some((_, n)) => println!(
                "  💸 saves   {} per run — ≈ {} over your last 30 days ({n} commits ≈ CI runs)",
                fmt_secs(saved),
                fmt_secs(saved * n as f64),
            ),
            None => println!("  💸 saves   {} per run", fmt_secs(saved)),
        }
    }
    println!("================================================");

    if py_code != 0 {
        println!(
            "  note: your pytest run was already red ({} failing) — that's pre-existing, \
             not caused by rstest.",
            py_out.values().filter(|r| r.phase == Phase::Fail).count()
        );
    }
    if identical {
        println!("  → drop-in ready: `rstest` is `pytest`, in parallel. Switch with confidence.");
    } else {
        println!(
            "  → some tests differ. Could be a pytest-version difference or a real parallel-only\n\
             \x20   issue — run `rstest --migrate-check` to classify each and get the fix."
        );
    }
    Ok(if identical { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_by_pattern() {
        assert!(matches!(
            classify("<obj at 0x10ae4e660>-(10+0j)"),
            Kind::Address
        ));
        assert!(matches!(
            classify("efc8cccd-21d0-45ee-a84e-5b9e5f2ce0fd"),
            Kind::Uuid
        ));
        assert!(matches!(classify("06-14-2026 11:56:59"), Kind::Time));
        assert!(matches!(classify("plain-label-2"), Kind::Other));
    }

    #[test]
    fn will_bail_only_address_and_uuid() {
        assert!(Kind::Address.will_bail());
        assert!(Kind::Uuid.will_bail());
        assert!(!Kind::Time.will_bail()); // stable enough within a run
        assert!(!Kind::Other.will_bail());
    }

    #[test]
    fn split_param_uses_first_bracket_through_nested_repr() {
        // A param repr can contain brackets; the site is everything before the
        // FIRST '['. rfind would split inside the repr and lose the address.
        let nid = "test_any.py::test_x[MyModel({ class: Py(0x0000000a1e2c4010), defs: [] })]";
        let (site, param) = split_param(nid);
        assert_eq!(site, "test_any.py::test_x");
        assert!(param.contains("0x0000000a1e2c4010"));
        assert!(matches!(classify(param), Kind::Address));
    }

    #[test]
    fn split_param_no_brackets() {
        let (site, param) = split_param("a.py::test_plain");
        assert_eq!(site, "a.py::test_plain");
        assert_eq!(param, "");
    }

    #[test]
    fn file_of_strips_at_first_colons() {
        assert_eq!(
            file_of("tests/test_x.py::TestC::test_m[param]"),
            "tests/test_x.py"
        );
        assert_eq!(file_of("tests/test_x.py"), "tests/test_x.py");
    }

    #[test]
    fn decide_covers_every_branch() {
        // (serial1, serial2, loadfile, wait_bound) -> verdict
        // fails both serial repeats -> not a parallelism issue.
        assert_eq!(decide(true, true, true, false), Verdict::NotParallel);
        assert_eq!(decide(true, true, false, false), Verdict::NotParallel);
        // serial repeats disagree -> intrinsic flake (either order).
        assert_eq!(decide(true, false, true, false), Verdict::IntrinsicFlake);
        assert_eq!(decide(false, true, false, true), Verdict::IntrinsicFlake);
        // passes serial, passes loadfile -> order dependency (cross-file).
        assert_eq!(decide(false, false, false, false), Verdict::OrderDependency);
        // passes serial, fails loadfile, wait-bound -> wall-clock.
        assert_eq!(decide(false, false, true, true), Verdict::WallClock);
        // passes serial, fails loadfile, NOT wait-bound -> isolation.
        assert_eq!(decide(false, false, true, false), Verdict::Isolation);
    }

    #[test]
    fn decide_wait_bound_only_splits_the_loadfile_failure() {
        // wait_bound must not override the serial/order branches - it only
        // distinguishes WallClock from Isolation once load+loadfile both fail.
        assert_eq!(decide(true, true, true, true), Verdict::NotParallel);
        assert_eq!(decide(false, false, false, true), Verdict::OrderDependency);
    }

    #[test]
    fn wait_bound_signal() {
        // 1.0s wall, ~0 cpu -> waiting (wall-clock test).
        let waiting = Rec {
            phase: Phase::Fail,
            wall: 1.0,
            cpu: Some(0.01),
        };
        assert!(waiting.wait_bound());
        // cpu-bound: most of the wall is compute.
        let computing = Rec {
            phase: Phase::Fail,
            wall: 1.0,
            cpu: Some(0.9),
        };
        assert!(!computing.wait_bound());
        // no cpu data (doctor off) -> can't claim wait-bound.
        let no_cpu = Rec {
            phase: Phase::Fail,
            wall: 1.0,
            cpu: None,
        };
        assert!(!no_cpu.wait_bound());
        // trivially short -> not meaningful, don't flag.
        let quick = Rec {
            phase: Phase::Fail,
            wall: 0.001,
            cpu: Some(0.0),
        };
        assert!(!quick.wait_bound());
    }
}
