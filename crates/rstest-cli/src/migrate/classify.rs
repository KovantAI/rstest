//! The two migrate-check classifiers: unstable parametrize ids (collection
//! phase) and parallel-only failures (run phase). The core decisions
//! (`classify`, `decide`) are pure and unit-tested; the discriminator runs
//! (`classify_failures`, `bisect_polluter`) drive child sessions to reach them.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;

use super::bisect::{child_pins, pinned_inifile, workdir, MAXFAIL_LIFT};
use super::{file_of, run_session, Outcomes, Phase};
use crate::reporting::sink::Sink;

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
    Inconclusive,    // missing from a discriminator run - no evidence either way
}

impl Verdict {
    pub(super) fn title(&self) -> &'static str {
        match self {
            Verdict::NotParallel => "NOT PARALLEL-SPECIFIC",
            Verdict::IntrinsicFlake => "INTRINSIC FLAKE",
            Verdict::OrderDependency => "ORDER DEPENDENCY",
            Verdict::WallClock => "WALL-CLOCK / LOAD-SENSITIVE",
            Verdict::Isolation => "ISOLATION / CO-LOCATION",
            Verdict::Inconclusive => "INCONCLUSIVE",
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
            Verdict::Inconclusive => (
                "did not run in a -n 0 / loadfile follow-up run, so it can't be classified",
                "check the nodeid is stable across collections (`rstest migrate-check`) \
                 and that the follow-up runs collect it",
            ),
        }
    }
}

/// Classify the parallel-only failures. `par` = -n auto outcomes, pooled over
/// `repeat` parallel runs; the function runs the discriminators (serial
/// ×max(`repeat`, 2), loadfile ×max(`repeat`, 1)) and decides per failing test.
/// Matching the discriminator run counts to the parallel pass keeps the
/// evidence symmetric: an intermittent failure caught once in N parallel runs
/// gets N chances to show up serially and under loadfile too.
pub(super) fn classify_failures(
    python: &Path,
    args: &[String],
    par: &Outcomes,
    repeat: u32,
    sink: &mut Sink,
) -> Result<Vec<(String, Verdict)>> {
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
    // Nodeids (and so these file paths) are rootdir-relative, but a child
    // resolves paths from the cwd; from a subdirectory `tests/test_a.py` would
    // become `tests/tests/test_a.py` and collect nothing. Anchor them at the
    // rootdir, and pin the rootdir, config file and conftest cutoff the way
    // bisect does: absolute paths under a nested `pkg/pytest.ini` would
    // otherwise load that config instead of the one the -n auto pass used.
    // Best effort: without a rootdir the paths stay as-is, and any test the
    // children then miss comes back INCONCLUSIVE below rather than a pass.
    let collected = super::collect_session(python, args).ok();
    let rootdir = collected
        .as_ref()
        .and_then(|c| c.rootdir.as_deref())
        .map(PathBuf::from);
    // Holds the blank stand-in config (when none was loaded) until the
    // discriminators finish.
    let work = workdir()?;
    let mut scoped: Vec<String> = files
        .iter()
        .map(|f| match &rootdir {
            Some(root) => root.join(f).display().to_string(),
            None => f.to_string(),
        })
        .collect();
    if let (Some(root), Some(c)) = (&rootdir, &collected) {
        let ini = pinned_inifile(c.inifile.as_deref(), work.path())?;
        let cutoff = c
            .confcutdir
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| root.clone());
        scoped.extend(child_pins(root, &ini, &cutoff));
    }
    scoped.extend_from_slice(args);
    // After the user's args and addopts (the last --maxfail wins): an `-x`
    // run stopping at an earlier failure would leave the rest missing, and so
    // INCONCLUSIVE, for a reason that has nothing to do with them.
    scoped.push(MAXFAIL_LIFT.into());
    let (serial_runs, loadfile_runs) = (repeat.max(2), repeat.max(1));
    sink.warn(&format!(
        "  {} parallel failure(s) in {} file(s); running discriminators (serial ×{serial_runs}, \
         loadfile ×{loadfile_runs}, scoped to those files)…",
        failed.len(),
        files.len(),
    ));
    let serial: Vec<Outcomes> = (0..serial_runs)
        .map(|_| run_session(python, &["-n", "0"], &scoped))
        .collect::<Result<_>>()?;
    let loadfile: Vec<Outcomes> = (0..loadfile_runs)
        .map(|_| run_session(python, &["--dist", "loadfile"], &scoped))
        .collect::<Result<_>>()?;

    let fails = |o: &Outcomes, n: &str| matches!(o.get(n).map(|r| r.phase), Some(Phase::Fail));
    let wait_bound = |n: &str| par.get(n).map(|r| r.wait_bound()).unwrap_or(false);
    let mut out = Vec::new();
    for n in failed {
        // A test absent from any follow-up run (empty snapshot, unstable id,
        // collection error) has no evidence; reading absence as a pass would
        // turn a deterministic failure into ORDER DEPENDENCY.
        let ran = |o: &Outcomes| o.contains_key(n.as_str());
        if !serial.iter().chain(&loadfile).all(ran) {
            out.push((n.clone(), Verdict::Inconclusive));
            continue;
        }
        // decide() wants "failed every serial run" and "failed some serial run";
        // with exactly two runs these are s1 && s2 and s1 || s2 as before.
        let all = serial.iter().all(|o| fails(o, n));
        let any = serial.iter().any(|o| fails(o, n));
        // Any loadfile failure counts: an intermittent co-location race that
        // passed one loadfile run is still a co-location race, not ORDER.
        let lf = loadfile.iter().any(|o| fails(o, n));
        let v = decide(all, any, lf, wait_bound(n));
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
pub(super) fn bisect_polluter(
    python: &Path,
    args: &[String],
    victim: &str,
    all: &Outcomes,
) -> Result<Polluter> {
    let vfile = file_of(victim).to_string();

    // reproduce(subset): run `-n 0 <subset…> <vfile>` (vfile last so the
    // candidate files run first) - does the victim fail?
    let reproduce = |subset: &[String]| -> Result<bool> {
        let mut sel: Vec<String> = subset.to_vec();
        sel.push(vfile.clone());
        sel.extend_from_slice(args);
        // Order is the whole point here: keep pytest-randomly from shuffling
        // the candidates after the victim.
        let o = run_session(python, &["-n", "0", "-p", "no:randomly"], &sel)?;
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

    #[test]
    fn kind_label_and_fix_distinct_per_variant() {
        // Every Kind has its own label and a non-empty, address/uuid-specific fix.
        for k in [Kind::Address, Kind::Uuid, Kind::Time, Kind::Other] {
            assert!(!k.label().is_empty());
            assert!(!k.fix().is_empty());
        }
        assert_eq!(Kind::Address.label(), "address");
        assert_eq!(Kind::Uuid.label(), "uuid");
        assert_eq!(Kind::Time.label(), "time");
        assert_eq!(Kind::Other.label(), "other");
        // The address fix names repr()/address; the uuid fix names uuid.
        assert!(Kind::Address.fix().contains("repr"));
        assert!(Kind::Uuid.fix().contains("uuid"));
        assert!(Kind::Time.fix().contains("clock"));
        // Labels are all distinct.
        let labels = [
            Kind::Address.label(),
            Kind::Uuid.label(),
            Kind::Time.label(),
            Kind::Other.label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn verdict_title_and_advice_present_for_every_variant() {
        let all = [
            Verdict::NotParallel,
            Verdict::IntrinsicFlake,
            Verdict::OrderDependency,
            Verdict::WallClock,
            Verdict::Isolation,
        ];
        for v in all {
            assert!(!v.title().is_empty());
            let (why, fix) = v.advice();
            assert!(!why.is_empty());
            assert!(!fix.is_empty());
        }
        // Titles are all distinct (they key the by-verdict grouping in check.rs).
        let titles: std::collections::HashSet<_> = all.iter().map(|v| v.title()).collect();
        assert_eq!(titles.len(), 5);
        // Spot-check the semantics carried in the advice text.
        assert!(Verdict::NotParallel.advice().0.contains("-n 0"));
        assert!(Verdict::WallClock.advice().0.contains("wait-bound"));
    }

    #[test]
    fn split_param_ends_with_bracket_but_no_open_is_left_whole() {
        // Defensive: a trailing ']' with no '[' isn't a param site - return whole.
        let (site, param) = split_param("weird]");
        assert_eq!(site, "weird]");
        assert_eq!(param, "");
    }
}
