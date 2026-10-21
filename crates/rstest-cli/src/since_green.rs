//! "What changed since green?": every full run that exits 0 snapshots what git
//! can't see about it (interpreter version, installed packages, a few
//! environment variables) plus its commit, in `.rstest_cache/green_env.json`.
//! A red run diffs the current state against that snapshot, so a CI break
//! caused by a transitive dependency bump or an interpreter change names
//! itself; `rstest since-green` shows the same diff without running tests.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cache;
use crate::explain::SuspectCommit;
use crate::reporting::sink::Sink;

pub const FILE: &str = "green_env.json";
const SCHEMA: u32 = 1;

/// Environment variables that change how tests behave and carry no secrets.
const ENV_VARS: [&str; 8] = [
    "PYTEST_ADDOPTS",
    "PYTHONPATH",
    "PYTHONHASHSEED",
    "PYTHONWARNINGS",
    "PYTHONUTF8",
    "TZ",
    "LANG",
    "LC_ALL",
];

/// Commits listed in the human block (`--json` carries all).
const COMMITS_SHOWN: usize = 10;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    schema: u32,
    /// `HEAD` when the snapshot was taken; `None` outside a git checkout.
    pub commit: Option<String>,
    pub epoch: u64,
    /// Interpreter version from the venv's `pyvenv.cfg`.
    pub python: Option<String>,
    /// Installed third-party packages, normalized name -> version.
    pub packages: BTreeMap<String, String>,
    /// The [`ENV_VARS`] that were set, name -> value.
    pub env: BTreeMap<String, String>,
}

/// The current state, for `scope`'s interpreter `python`.
pub fn capture(scope: &Path, python: &Path, commit: Option<&str>, epoch: u64) -> Snapshot {
    Snapshot {
        schema: SCHEMA,
        commit: commit.map(str::to_string),
        epoch,
        python: crate::incremental::python_version(python),
        packages: crate::incremental::installed_packages(scope, python),
        env: ENV_VARS
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
            .collect(),
    }
}

/// Save `snap` as the last green state. Best-effort.
pub fn record(snap: &Snapshot) {
    record_to(&cache::file(FILE), snap);
}

/// [`record`] to an explicit path.
pub fn record_to(path: &Path, snap: &Snapshot) {
    if let Ok(bytes) = serde_json::to_vec(snap) {
        let _ = cache::write_atomic(path, &bytes);
    }
}

/// The last green state, or `None` when no run has been green yet (or the
/// file is from another schema).
pub fn load() -> Option<Snapshot> {
    load_from(&cache::file(FILE))
}

/// [`load`] from an explicit path.
pub fn load_from(path: &Path) -> Option<Snapshot> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<Snapshot>(&bytes)
        .ok()
        .filter(|s| s.schema == SCHEMA)
}

/// A green run's snapshot as it travels through the shared cache: a full
/// green run (`shard: None`), or one green shard `(k, n)` of a sharded run.
/// A sharded run counts as green once every shard `1..=n` of the same commit
/// has a part (see [`newest_complete`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GreenPart {
    pub snapshot: Snapshot,
    pub shard: Option<(usize, usize)>,
}

/// The newest green snapshot the parts prove: a full green run, or a sharded
/// run whose every shard (same commit, same shard count) went green, which
/// counts at its last shard's time. Returns it with the parts still waiting
/// for the rest of their run (the caller keeps those for a later merge):
/// shards of a newer run than the snapshot returned, at most `keep` of them.
pub fn newest_complete(parts: Vec<GreenPart>, keep: usize) -> (Option<Snapshot>, Vec<GreenPart>) {
    let mut best: Option<Snapshot> = None;
    let mut sets: BTreeMap<(String, usize), Vec<GreenPart>> = BTreeMap::new();
    for p in parts {
        match (p.shard, &p.snapshot.commit) {
            (None, _) => {
                if best.as_ref().is_none_or(|b| p.snapshot.epoch > b.epoch) {
                    best = Some(p.snapshot);
                }
            }
            // A shard without a commit can't be matched with its siblings.
            (Some(_), None) => {}
            (Some((_, n)), Some(c)) => sets.entry((c.clone(), n)).or_default().push(p),
        }
    }
    let mut pending: Vec<GreenPart> = Vec::new();
    for ((_, n), mut set) in sets {
        set.sort_by_key(|p| p.shard.map(|(k, _)| k));
        set.dedup_by_key(|p| p.shard);
        let complete = (1..=n).all(|k| set.iter().any(|p| p.shard == Some((k, n))));
        if complete {
            let last = set
                .into_iter()
                .max_by_key(|p| p.snapshot.epoch)
                .map(|p| p.snapshot);
            if let Some(l) = last {
                if best.as_ref().is_none_or(|b| l.epoch > b.epoch) {
                    best = Some(l);
                }
            }
        } else {
            pending.extend(set);
        }
    }
    // Shards older than the green we have can't improve on it.
    let since = best.as_ref().map_or(0, |b| b.epoch);
    pending.retain(|p| p.snapshot.epoch > since);
    pending.sort_by_key(|p| std::cmp::Reverse(p.snapshot.epoch));
    pending.truncate(keep);
    (best, pending)
}

/// One value that differs: `None` on a side means absent there.
#[derive(Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct Change {
    /// Package name, environment variable, or `python`.
    name: String,
    /// Value at the last green run; `null` when it was absent (added since).
    green: Option<String>,
    /// Value now; `null` when it is gone (removed since).
    now: Option<String>,
}

/// Envelope metadata for the since-green document.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct Meta {
    /// Constant producer tag: always `"rstest"`.
    runner: &'static str,
    /// Constant discriminator: always `"since-green"`.
    kind: &'static str,
    /// Document schema version.
    schema: u32,
    /// The rstest version that wrote the document.
    rstest_version: &'static str,
}

/// What differs between the last green run and now.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct SinceGreen {
    /// Envelope metadata.
    meta: Meta,
    /// Whether a green run was ever recorded here; every other field is
    /// empty or null when it wasn't.
    found: bool,
    /// Commit of the last green run.
    green_commit: Option<String>,
    /// Epoch seconds of the last green run.
    green_epoch: Option<u64>,
    /// Current `HEAD`.
    commit: Option<String>,
    /// Commits after the green commit up to `HEAD` (`git log green..HEAD`),
    /// newest first. Null when there is no commit on either side or git
    /// can't resolve the range here.
    commits: Option<Vec<SuspectCommit>>,
    /// Interpreter version change, if any.
    python: Option<Change>,
    /// Packages added, removed or at another version, by name.
    packages: Vec<Change>,
    /// Watched environment variables set, unset or changed.
    env: Vec<Change>,
}

impl SinceGreen {
    fn is_empty(&self) -> bool {
        // Unknown commits (git couldn't resolve the range) only count as "no
        // commits" when both sides are the same commit.
        let no_commits = match &self.commits {
            Some(cs) => cs.is_empty(),
            None => self.green_commit == self.commit,
        };
        no_commits && self.python.is_none() && self.packages.is_empty() && self.env.is_empty()
    }
}

fn diff_maps(green: &BTreeMap<String, String>, now: &BTreeMap<String, String>) -> Vec<Change> {
    let mut names: Vec<&String> = green.keys().chain(now.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|n| green.get(*n) != now.get(*n))
        .map(|n| Change {
            name: n.clone(),
            green: green.get(n).cloned(),
            now: now.get(n).cloned(),
        })
        .collect()
}

/// Diff `now` against the last green snapshot. `git` runs `git <args>` (the
/// commit range); injectable for tests.
fn compare(
    green: Option<&Snapshot>,
    now: &Snapshot,
    git: impl Fn(&[&str]) -> Result<String>,
) -> SinceGreen {
    let meta = Meta {
        runner: "rstest",
        kind: "since-green",
        schema: SCHEMA,
        rstest_version: env!("CARGO_PKG_VERSION"),
    };
    let Some(g) = green else {
        return SinceGreen {
            meta,
            found: false,
            green_commit: None,
            green_epoch: None,
            commit: now.commit.clone(),
            commits: None,
            python: None,
            packages: Vec::new(),
            env: Vec::new(),
        };
    };
    let commits = match (&g.commit, &now.commit) {
        (Some(a), Some(b)) => crate::explain::suspects(a, b, git),
        _ => None,
    };
    SinceGreen {
        meta,
        found: true,
        green_commit: g.commit.clone(),
        green_epoch: Some(g.epoch),
        commit: now.commit.clone(),
        commits,
        python: (g.python != now.python).then(|| Change {
            name: "python".into(),
            green: g.python.clone(),
            now: now.python.clone(),
        }),
        packages: diff_maps(&g.packages, &now.packages),
        env: diff_maps(&g.env, &now.env),
    }
}

fn change_line(c: &Change) -> String {
    match (&c.green, &c.now) {
        (Some(a), Some(b)) => format!("{} {a} -> {b}", c.name),
        (None, Some(b)) => format!("{} {b} (added)", c.name),
        (Some(a), None) => format!("{} {a} (removed)", c.name),
        (None, None) => c.name.clone(),
    }
}

/// The human block. `red` words it for a failing run.
fn render(sink: &mut Sink, d: &SinceGreen, now_epoch: u64, red: bool) {
    let out = |sink: &mut Sink, s: &str| {
        if red {
            sink.warn(s)
        } else {
            sink.out_line(s)
        }
    };
    if !d.found {
        out(
            sink,
            "rstest: since green: no green run recorded here yet (every full run that exits 0 records one)",
        );
        return;
    }
    let short = |c: &Option<String>| {
        c.as_deref()
            .map_or("no commit".to_string(), |c| c.chars().take(10).collect())
    };
    out(
        sink,
        &format!(
            "rstest: changed since the last green run ({}, {}):",
            short(&d.green_commit),
            crate::explain::ago(d.green_epoch.unwrap_or(0), now_epoch)
        ),
    );
    if d.is_empty() {
        out(
            sink,
            "  nothing: same commit, interpreter, packages and environment. \
             Suspect the tests themselves (`rstest flaky`, `rstest explain <nodeid>`).",
        );
        return;
    }
    match &d.commits {
        Some(cs) if !cs.is_empty() => {
            out(sink, &format!("  {} commit(s):", cs.len()));
            for c in cs.iter().take(COMMITS_SHOWN) {
                out(sink, &format!("    {} {} ({})", c.sha, c.subject, c.author));
            }
            if cs.len() > COMMITS_SHOWN {
                out(sink, &format!("    ... {} more", cs.len() - COMMITS_SHOWN));
            }
        }
        Some(_) => {}
        None if d.green_commit.is_some() && d.commit.is_some() => out(
            sink,
            "  commits: unknown here (not a git checkout, or the green commit isn't fetched)",
        ),
        None => {}
    }
    if let Some(p) = &d.python {
        out(sink, &format!("  interpreter: {}", change_line(p)));
    }
    if !d.packages.is_empty() {
        out(sink, &format!("  {} package(s):", d.packages.len()));
        for c in &d.packages {
            out(sink, &format!("    {}", change_line(c)));
        }
    }
    if !d.env.is_empty() {
        out(sink, "  environment:");
        for c in &d.env {
            out(sink, &format!("    {}", change_line(c)));
        }
    }
}

/// pytest flags that select a subset of the suite (with their `=`/attached
/// forms): a run using one says nothing about the whole suite being green.
const SELECTION_FLAGS: [&str; 11] = [
    "-k",
    "-m",
    "--lf",
    "--last-failed",
    "--deselect",
    "--ignore",
    "--ignore-glob",
    "--sw",
    "--stepwise",
    "--sw-skip",
    "--stepwise-skip",
];

/// Whether the session args select only part of the suite: a nodeid, a
/// selection flag, or a path that doesn't cover the suite. A path covers it
/// when it is the rootdir (or above it) or contains every `testpaths` entry,
/// so CI's `rstest tests/` still counts as a full run.
pub fn narrows_selection(args: &[String], project: &crate::config::ProjectConfig) -> bool {
    let flag = args.iter().any(|a| {
        SELECTION_FLAGS.iter().any(|f| {
            a == f
                || a.strip_prefix(f)
                    .is_some_and(|rest| rest.starts_with('=') || (f.len() == 2 && !rest.is_empty()))
        })
    });
    let nodeid = args
        .iter()
        .zip(crate::cli::positional_mask(args))
        .any(|(a, pos)| pos && a.contains("::"));
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let root = canon(&project.rootdir);
    let roots: Vec<std::path::PathBuf> = if project.testpaths.is_empty() {
        vec![root.clone()]
    } else {
        project
            .testpaths
            .iter()
            .map(|t| canon(&root.join(t)))
            .collect()
    };
    let partial_path = crate::cli::path_args(args).iter().any(|p| {
        let p = canon(Path::new(p.strip_prefix('@').unwrap_or(p)));
        !(root.starts_with(&p) || roots.iter().all(|r| r.starts_with(&p)))
    });
    flag || nodeid || partial_path
}

/// After a run: a green run (exit 0) over the whole suite records the
/// snapshot (`full` is false for a run that selected only part of it: the
/// caller's selection flags plus [`narrows_selection`]); a run with failed
/// tests (exit 1) prints what changed since the last green one, when anything
/// did.
pub fn after_run(sink: &mut Sink, exitstatus: i32, full: bool, now: &Snapshot) {
    match exitstatus {
        0 if full => record(now),
        1 => {
            if let Some(g) = load() {
                let d = compare(Some(&g), now, crate::select::git_stdout);
                // Silent when nothing differs: a red cycle in a local edit
                // loop (or `--watch`) shouldn't repeat "nothing changed".
                if !d.is_empty() {
                    render(sink, &d, now.epoch, true);
                }
            }
        }
        _ => {}
    }
}

/// `rstest since-green [--json]`: the diff against the last green run, no
/// tests run.
pub fn run_since_green(sink: &mut Sink, python: &Path, json: bool) -> Result<i32> {
    let scope = std::env::current_dir()?;
    let epoch = crate::time::now_epoch_secs();
    let now = capture(
        &scope,
        python,
        crate::incremental::head_sha().as_deref(),
        epoch,
    );
    let d = compare(load().as_ref(), &now, crate::select::git_stdout);
    if json {
        let mut bytes = serde_json::to_vec_pretty(&d)?;
        bytes.push(b'\n');
        std::io::Write::write_all(sink.out(), &bytes)?;
    } else {
        render(sink, &d, epoch, false);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(commit: &str, python: &str, pkgs: &[(&str, &str)], env: &[(&str, &str)]) -> Snapshot {
        Snapshot {
            schema: SCHEMA,
            commit: Some(commit.into()),
            epoch: 1000,
            python: Some(python.into()),
            packages: pkgs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn no_git(_: &[&str]) -> Result<String> {
        Ok(String::new())
    }

    #[test]
    fn compare_finds_package_interpreter_and_env_changes() {
        let green = snap(
            "aaa",
            "3.12.1",
            &[("httpx", "0.27.0"), ("gone", "1.0"), ("same", "2.0")],
            &[("TZ", "UTC"), ("LANG", "C")],
        );
        let now = snap(
            "bbb",
            "3.12.4",
            &[("httpx", "0.28.1"), ("new-dep", "0.1"), ("same", "2.0")],
            &[("TZ", "UTC"), ("PYTHONHASHSEED", "0")],
        );
        let git = |args: &[&str]| -> Result<String> {
            assert_eq!(args[2], "aaa..bbb");
            Ok("bbb\x1fAda\x1fbump httpx\n".into())
        };
        let d = compare(Some(&green), &now, git);
        assert!(d.found && !d.is_empty());
        assert_eq!(d.commits.as_ref().unwrap()[0].subject, "bump httpx");
        let p = d.python.as_ref().unwrap();
        assert_eq!(
            (p.green.as_deref(), p.now.as_deref()),
            (Some("3.12.1"), Some("3.12.4"))
        );
        let lines: Vec<String> = d.packages.iter().map(change_line).collect();
        assert_eq!(
            lines,
            [
                "gone 1.0 (removed)",
                "httpx 0.27.0 -> 0.28.1",
                "new-dep 0.1 (added)"
            ]
        );
        let env: Vec<String> = d.env.iter().map(change_line).collect();
        assert_eq!(env, ["LANG C (removed)", "PYTHONHASHSEED 0 (added)"]);
    }

    #[test]
    fn render_red_block_goes_to_stderr() {
        let green = snap("aaa", "3.12.1", &[("httpx", "0.27.0")], &[]);
        let now = snap("aaa", "3.12.1", &[("httpx", "0.28.1")], &[]);
        let d = compare(Some(&green), &now, no_git);
        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &d, 1000 + 7200, true);
        assert_eq!(cap.out(), "", "stdout stays a clean stream");
        let err = cap.err();
        assert!(
            err.contains("rstest: changed since the last green run (aaa, 2h ago):"),
            "{err}"
        );
        assert!(
            err.contains("  1 package(s):\n    httpx 0.27.0 -> 0.28.1\n"),
            "{err}"
        );
        // Same commit: an empty range, so no commit line at all.
        assert!(!err.contains("commit(s)"), "{err}");
    }

    #[test]
    fn render_nothing_changed_and_no_green_yet() {
        let s = snap("aaa", "3.12.1", &[("x", "1")], &[]);
        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &compare(Some(&s), &s, no_git), 1000, false);
        assert!(cap
            .out()
            .contains("nothing: same commit, interpreter, packages and environment"));

        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &compare(None, &s, no_git), 1000, false);
        assert!(cap.out().contains("no green run recorded here yet"));

        // git can't resolve the range: say so instead of "no commits".
        let other = snap("bbb", "3.12.1", &[("x", "1")], &[]);
        let d = compare(Some(&s), &other, |_| anyhow::bail!("bad revision"));
        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &d, 1000, false);
        assert!(cap.out().contains("commits: unknown here"), "{}", cap.out());
    }

    #[test]
    fn narrowed_selections_never_count_as_green() {
        let root = std::env::temp_dir().join(format!("rstest-sg-narrow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tests/unit")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("tests/unit/test_a.py"), "").unwrap();
        let mut project = crate::config::ProjectConfig {
            rootdir: root.clone(),
            testpaths: vec!["tests".into()],
            ..Default::default()
        };
        let p = |rel: &str| root.join(rel).display().to_string();
        let narrows = |args: &[String], project: &crate::config::ProjectConfig| {
            narrows_selection(args, project)
        };
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Full: no args, the testpaths dir itself, the rootdir, plain flags.
        assert!(!narrows(&v(&[]), &project));
        assert!(!narrows(&[p("tests")], &project));
        assert!(!narrows(&[p(".")], &project));
        assert!(!narrows(&v(&["-x", "-v", "--durations=5"]), &project));
        // Narrowed: a sub-dir, a file, a nodeid, selection flags in every form.
        assert!(narrows(&[p("tests/unit")], &project));
        assert!(narrows(&[p("tests/unit/test_a.py")], &project));
        assert!(narrows(&v(&["tests/unit/test_a.py::test_x"]), &project));
        for args in [
            v(&["-k", "slow"]),
            v(&["-kslow"]),
            v(&["-m=integration"]),
            v(&["--lf"]),
            v(&["--deselect", "t.py::x"]),
            v(&["--ignore=tests/unit"]),
            v(&["--sw"]),
        ] {
            assert!(narrows(&args, &project), "{args:?}");
        }
        // Without testpaths, only the rootdir (or above) is the whole suite.
        project.testpaths.clear();
        assert!(narrows(&[p("tests")], &project));
        assert!(!narrows(&[p(".")], &project));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn part(commit: Option<&str>, epoch: u64, shard: Option<(usize, usize)>) -> GreenPart {
        GreenPart {
            snapshot: Snapshot {
                commit: commit.map(String::from),
                epoch,
                ..snap("x", "3.12", &[], &[])
            },
            shard,
        }
    }

    #[test]
    fn a_sharded_run_is_green_once_every_shard_is() {
        // Two of three shards: not green yet; both wait for the third.
        let (best, pending) = newest_complete(
            vec![
                part(Some("c1"), 10, Some((1, 3))),
                part(Some("c1"), 12, Some((2, 3))),
            ],
            50,
        );
        assert_eq!(best, None);
        assert_eq!(pending.len(), 2);

        // All three (one pushed twice): green, at the last shard's time.
        let (best, pending) = newest_complete(
            vec![
                part(Some("c1"), 10, Some((1, 3))),
                part(Some("c1"), 12, Some((2, 3))),
                part(Some("c1"), 11, Some((3, 3))),
                part(Some("c1"), 10, Some((1, 3))),
            ],
            50,
        );
        assert_eq!(best.map(|b| b.epoch), Some(12));
        assert!(pending.is_empty());

        // Shards of different shard counts or commits never add up.
        let (best, _) = newest_complete(
            vec![
                part(Some("c1"), 10, Some((1, 2))),
                part(Some("c1"), 11, Some((2, 3))),
                part(Some("c2"), 12, Some((2, 2))),
                // No commit: can't be matched with its siblings.
                part(None, 13, Some((1, 1))),
            ],
            50,
        );
        assert_eq!(best, None);
    }

    #[test]
    fn newest_green_wins_and_stale_waiting_shards_go() {
        let complete = || {
            vec![
                part(Some("c2"), 50, Some((1, 2))),
                part(Some("c2"), 51, Some((2, 2))),
            ]
        };
        // A newer complete sharded run beats an older full run...
        let mut parts = complete();
        parts.push(part(Some("c1"), 40, None));
        assert_eq!(newest_complete(parts, 50).0.map(|b| b.epoch), Some(51));
        // ...and a newer full run beats it.
        let mut parts = complete();
        parts.push(part(Some("c3"), 60, None));
        assert_eq!(newest_complete(parts, 50).0.map(|b| b.epoch), Some(60));
        // Waiting shards older than the green can't improve on it; newer
        // ones keep waiting, capped, newest first.
        let mut parts = complete();
        parts.push(part(Some("c0"), 30, Some((1, 2))));
        for e in 70..75 {
            parts.push(part(Some(&format!("n{e}")), e, Some((1, 2))));
        }
        let (best, pending) = newest_complete(parts, 3);
        assert_eq!(best.map(|b| b.epoch), Some(51));
        let epochs: Vec<u64> = pending.iter().map(|p| p.snapshot.epoch).collect();
        assert_eq!(epochs, [74, 73, 72]);
    }

    #[test]
    fn json_shape() {
        let green = snap("aaa", "3.12.1", &[("httpx", "0.27.0")], &[]);
        let now = snap("aaa", "3.12.1", &[], &[]);
        let v = serde_json::to_value(compare(Some(&green), &now, no_git)).unwrap();
        assert_eq!(v["meta"]["kind"], "since-green");
        assert_eq!(v["found"], true);
        assert_eq!(v["green_commit"], "aaa");
        assert_eq!(v["packages"][0]["name"], "httpx");
        assert!(v["packages"][0]["now"].is_null());
        assert!(v["python"].is_null());
        let v = serde_json::to_value(compare(None, &now, no_git)).unwrap();
        assert_eq!(v["found"], false);
    }
}
