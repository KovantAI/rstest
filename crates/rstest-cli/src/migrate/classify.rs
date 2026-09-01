//! The two migrate-check classifiers: unstable parametrize ids (collection
//! phase) and parallel-only failures (run phase). The core decisions
//! (`classify`, `decide`) are pure and unit-tested; the discriminator runs
//! (`classify_failures`, `bisect_polluter`) drive child sessions to reach them.

use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;

use super::{file_of, run_session, Outcomes, Phase};

/// Why a parametrize id is unstable. `WILL` bail are per-process (differ in
/// every worker); `MAY` bail depend on collection timing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Address, // 0x... - repr() fallback id; differs every process
    Uuid,    // uuid4 in the id
    Time,    // timestamp / date in the id
    Other,   // unstable for an unrecognized reason
}

impl Kind {
    pub(super) fn will_bail(self) -> bool {
        matches!(self, Kind::Address | Kind::Uuid)
    }
    pub(super) fn label(self) -> &'static str {
        match self {
            Kind::Address => "address",
            Kind::Uuid => "uuid",
            Kind::Time => "time",
            Kind::Other => "other",
        }
    }
    pub(super) fn fix(self) -> &'static str {
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

pub(super) fn classify(param: &str) -> Kind {
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
pub(super) fn split_param(nodeid: &str) -> (&str, &str) {
    if nodeid.ends_with(']') {
        if let Some(open) = nodeid.find('[') {
            return (&nodeid[..open], &nodeid[open + 1..nodeid.len() - 1]);
        }
    }
    (nodeid, "")
}

/// The classifier verdict for one test that failed under `-n auto`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Verdict {
    NotParallel,     // fails at -n 0 too (deterministically) - a plain bug / env
    IntrinsicFlake,  // serial runs disagree - flaky under any runner
    OrderDependency, // passes serial + loadfile, fails under load
    WallClock,       // passes serial, fails parallel, wait-bound - load-sensitive timing
    Isolation,       // passes serial, fails under load AND loadfile - co-location
}

impl Verdict {
    pub(super) fn title(&self) -> &'static str {
        match self {
            Verdict::NotParallel => "NOT PARALLEL-SPECIFIC",
            Verdict::IntrinsicFlake => "INTRINSIC FLAKE",
            Verdict::OrderDependency => "ORDER DEPENDENCY",
            Verdict::WallClock => "WALL-CLOCK / LOAD-SENSITIVE",
            Verdict::Isolation => "ISOLATION / CO-LOCATION",
        }
    }
    pub(super) fn advice(&self) -> (&'static str, &'static str) {
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
pub(super) fn classify_failures(args: &[String], par: &Outcomes) -> Result<Vec<(String, Verdict)>> {
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
pub(super) fn decide(serial1: bool, serial2: bool, loadfile: bool, wait_bound: bool) -> Verdict {
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

/// Where a victim's pollution comes from.
pub(super) enum Polluter {
    SameFile(String),  // reproduces running the victim's own file alone
    OtherFile(String), // a different file, run before the victim, reproduces
    NotReproducible,   // no serial ordering reproduces - likely a concurrent race
}

/// Find the polluter: the file whose tests, run serially BEFORE the victim,
/// reproduce its failure. Checks the victim's own file first (same-file
/// co-location), then binary-searches the rest (polluter must precede victim).
pub(super) fn bisect_polluter(args: &[String], victim: &str, all: &Outcomes) -> Result<Polluter> {
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
}
