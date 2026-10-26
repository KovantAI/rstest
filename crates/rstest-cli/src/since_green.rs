//! "What changed since green?": every full run that exits 0 on a clean tree
//! snapshots what git can't see about it (interpreter version, installed packages, a few
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
    /// The checkout had uncommitted changes. Never set on a recorded green
    /// snapshot: a green dirty tree proves nothing about `commit`.
    #[serde(default)]
    pub dirty: bool,
    pub epoch: u64,
    /// Interpreter version from the venv's `pyvenv.cfg`.
    pub python: Option<String>,
    /// Installed third-party packages, normalized name -> version.
    pub packages: BTreeMap<String, String>,
    /// The [`ENV_VARS`] that were set, name -> value.
    pub env: BTreeMap<String, String>,
}

/// The current state, for `scope`'s interpreter `python`.
pub fn capture(
    scope: &Path,
    python: &Path,
    commit: Option<&str>,
    dirty: bool,
    epoch: u64,
) -> Snapshot {
    Snapshot {
        schema: SCHEMA,
        commit: commit.map(str::to_string),
        dirty,
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
    /// Whether a green run was ever recorded here. When it wasn't, only
    /// `meta`, `commit` and `dirty` are filled; the rest are empty or null.
    found: bool,
    /// Commit of the last green run.
    green_commit: Option<String>,
    /// Epoch seconds of the last green run.
    green_epoch: Option<u64>,
    /// Current `HEAD`.
    commit: Option<String>,
    /// The checkout has uncommitted changes now.
    dirty: bool,
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
    /// Nothing but (maybe) uncommitted edits differs. The commit counts as
    /// unchanged only when it is the same sha: an empty `green..HEAD` range
    /// also means HEAD moved back behind the green commit.
    fn is_empty(&self) -> bool {
        self.green_commit == self.commit
            && self.python.is_none()
            && self.packages.is_empty()
            && self.env.is_empty()
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
            dirty: now.dirty,
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
        dirty: now.dirty,
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
            "rstest: since green: no green run recorded here yet (a whole-suite run that exits 0 on a clean tree records one)",
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
    if d.dirty {
        out(
            sink,
            "  uncommitted changes in the working tree (`git diff`)",
        );
    }
    if d.is_empty() {
        if !d.dirty {
            out(
                sink,
                "  nothing: same commit, interpreter, packages and environment. \
                 Suspect the tests themselves (`rstest flaky`, `rstest explain <nodeid>`).",
            );
        }
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
        // Empty range but another sha: HEAD is behind the green commit.
        Some(_) if d.green_commit != d.commit => out(
            sink,
            &format!(
                "  HEAD {} is behind the green commit (checked out or reset back; \
                 `git log HEAD..{}` lists what it lacks)",
                short(&d.commit),
                short(&d.green_commit)
            ),
        ),
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

/// pytest long flags that select a subset of the suite (with their `=`
/// forms): a run using one says nothing about the whole suite being green.
/// `-k`/`-m` are matched as short switches, clustered or attached. Setup and
/// fixture listings exit 0 without running a test body; `--pyargs` names
/// modules, not paths, so it is taken as a subset.
const SELECTION_FLAGS: [&str; 19] = [
    "--lf",
    "--last-failed",
    "--deselect",
    "--ignore",
    "--ignore-glob",
    "--sw",
    "--stepwise",
    "--sw-skip",
    "--stepwise-skip",
    "--co",
    "--collect-only",
    "--collectonly",
    "--setup-plan",
    "--setup-only",
    "--fixtures",
    "--fixtures-per-test",
    "--funcargs",
    "--markers",
    "--pyargs",
];

/// Whether the session args, run from `cwd`, select only part of the suite:
/// a nodeid, a selection flag, or paths that don't cover the suite. Paths
/// cover it when one is the rootdir (or above it) or together they contain
/// every `testpaths` match (globbed like pytest, else the rootdir), so CI's
/// `rstest tests/` still counts as a full run. No path below the rootdir
/// selects `cwd`, as pytest reads `testpaths` only from the rootdir itself.
/// Only positionals are nodeids: `-W error::DeprecationWarning` is no
/// selection.
pub fn narrows_selection(
    args: &[String],
    project: &crate::config::ProjectConfig,
    cwd: &Path,
) -> bool {
    let flag_or_nodeid = args
        .iter()
        .zip(crate::cli::positional_mask(args))
        .any(|(a, pos)| {
            (pos && a.contains("::"))
                || SELECTION_FLAGS
                    .iter()
                    .any(|f| a == f || a.strip_prefix(f).is_some_and(|r| r.starts_with('=')))
                || crate::cli::has_short(a, 'k')
                || crate::cli::has_short(a, 'm')
        });
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let root = canon(&project.rootdir);
    let mut paths: Vec<std::path::PathBuf> = crate::cli::path_args(args)
        .iter()
        .map(|p| canon(&cwd.join(p)))
        .collect();
    let cwd = canon(cwd);
    if paths.is_empty() && cwd != root {
        paths.push(cwd);
    }
    let mut roots: Vec<std::path::PathBuf> = project
        .testpaths
        .iter()
        .flat_map(|t| crate::collect::expand_glob(&project.rootdir, t))
        .map(|r| canon(&r))
        .collect();
    if roots.is_empty() {
        roots.push(root.clone());
    }
    let covered = paths.iter().any(|p| root.starts_with(p))
        || roots.iter().all(|r| paths.iter().any(|p| r.starts_with(p)));
    flag_or_nodeid || (!paths.is_empty() && !covered)
}

/// After a run: a green run (exit 0) over the whole suite (`full_suite`) on
/// a clean tree records the snapshot; a subset (`-k`, `--co`, a shard, ...)
/// passing says nothing about the suite. A run with failed tests (exit 1)
/// prints what changed since the last green one, when anything did.
pub fn after_run(sink: &mut Sink, exitstatus: i32, full_suite: bool, now: &Snapshot) {
    match exitstatus {
        0 if full_suite && !now.dirty => record(now),
        1 => {
            if let Some(g) = load() {
                let d = compare(Some(&g), now, crate::select::git_stdout);
                // Silent when nothing but local edits differs: a red cycle in
                // a local edit loop (or `--watch`) shouldn't repeat itself.
                if !d.is_empty() {
                    render(sink, &d, now.epoch, true);
                }
            }
        }
        _ => {}
    }
}

/// Does this run select part of the suite, from its argv or
/// `PYTEST_ADDOPTS` (which pytest puts before the argv)? The suite is that of
/// pytest's rootdir for these args, with `testpaths` only from a config file
/// in that dir. See [`narrows_selection`].
pub(crate) fn run_narrows_suite(args: &[String], cwd: &Path) -> bool {
    let mut all = std::env::var("PYTEST_ADDOPTS")
        .map(|v| crate::config::shell_split(&v))
        .unwrap_or_default();
    all.extend_from_slice(args);
    // pytest expands an `@argsfile` (one arg per line) before anything else,
    // the rootdir included.
    let all: Vec<String> = all
        .into_iter()
        .flat_map(|a| {
            match a
                .strip_prefix('@')
                .map(|f| std::fs::read_to_string(cwd.join(f)))
            {
                Some(Ok(body)) => body
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => vec![a],
            }
        })
        .collect();
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let root = crate::config::rootdir(cwd, &all);
    // ponytail: `-c other.ini` still reads testpaths from the rootdir's own
    // config; parse the named file if anyone runs since-green that way.
    let found = crate::config::discover(&root, &mut std::io::sink());
    let project = if canon(&found.rootdir) == canon(&root) {
        found
    } else {
        crate::config::ProjectConfig {
            rootdir: root,
            ..Default::default()
        }
    };
    narrows_selection(&all, &project, cwd)
}

/// Uncommitted edits to tracked files anywhere in the checkout. Untracked
/// files don't count: this command can't know which report files
/// (`--junitxml`, ...) the runs here write, and a leftover one would read as
/// a local edit. A git failure reads as dirty.
fn tracked_dirty(dir: &Path) -> bool {
    crate::select::git_stdout_in(dir, &["status", "--porcelain", "--untracked-files=no"])
        .map_or(true, |o| !o.trim().is_empty())
}

/// `rstest since-green [--json]`: the diff against the last green run, no
/// tests run.
pub fn run_since_green(sink: &mut Sink, python: &Path, json: bool) -> Result<i32> {
    let scope = std::env::current_dir()?;
    let epoch = crate::time::now_epoch_secs();
    let head = crate::incremental::head_sha();
    let dirty = head.is_some() && tracked_dirty(Path::new("."));
    let now = capture(&scope, python, head.as_deref(), dirty, epoch);
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
            dirty: false,
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
        std::fs::create_dir_all(root.join("pkgs/a/tests")).unwrap();
        std::fs::create_dir_all(root.join("pkgs/b/tests")).unwrap();
        std::fs::write(root.join("tests/unit/test_a.py"), "").unwrap();
        let mut project = crate::config::ProjectConfig {
            rootdir: root.clone(),
            testpaths: vec!["tests".into()],
            ..Default::default()
        };
        let p = |rel: &str| root.join(rel).display().to_string();
        let narrows = |args: &[String], project: &crate::config::ProjectConfig| {
            narrows_selection(args, project, &root)
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
        // No args below the rootdir: pytest collects the invocation dir.
        project.testpaths = vec!["tests".into()];
        assert!(narrows_selection(&[], &project, &root.join("tests/unit")));
        assert!(!narrows_selection(&[], &project, &root.join("tests")));
        // Globbed testpaths: the paths together must cover every match.
        project.testpaths = vec!["pkgs/*/tests".into()];
        assert!(!narrows(&[p("pkgs")], &project));
        assert!(!narrows(&[p("pkgs/a/tests"), p("pkgs/b/tests")], &project));
        assert!(narrows(&[p("pkgs/a/tests")], &project));
        // A testpaths glob matching nothing falls back to the rootdir.
        project.testpaths = vec!["nope/*".into()];
        assert!(narrows(&[p("tests")], &project));
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

    #[test]
    fn only_a_full_clean_green_run_records() {
        let held = crate::test_env::lock();
        let base = std::env::temp_dir().join(format!(
            "rstest-since-green-{}-{}",
            std::process::id(),
            crate::time::now_epoch_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        {
            let _cache = crate::test_env::set_var(&held, "RSTEST_CACHE", &base);
            let (mut sink, _cap) = Sink::captured();
            let green = snap("aaa", "3.12.1", &[("httpx", "0.27.0")], &[]);
            after_run(&mut sink, 0, true, &green);
            assert_eq!(load(), Some(green.clone()));
            // A passing subset (`--co`, `-k`, a shard) keeps the old baseline.
            let bumped = snap("aaa", "3.12.1", &[("httpx", "0.28.1")], &[]);
            after_run(&mut sink, 0, false, &bumped);
            assert_eq!(load(), Some(green.clone()));
            // So does a green dirty tree.
            after_run(
                &mut sink,
                0,
                true,
                &Snapshot {
                    dirty: true,
                    ..bumped
                },
            );
            assert_eq!(load(), Some(green));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn narrows_selection_spots_subsets() {
        let project = crate::config::ProjectConfig::default();
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for a in [
            &["-k", "smoke"][..],
            &["-ksmoke"],
            &["-m", "not slow"],
            &["--co"],
            &["--collect-only"],
            &["--lf"],
            &["--deselect=t.py::x"],
            &["tests/test_a.py::test_one"],
            &["--deselect", "t.py::x"],
            &["--setup-plan"],
            &["--setup-only"],
            &["--fixtures"],
            &["--fixtures-per-test"],
            &["--funcargs"],
            &["--markers"],
            &["--pyargs", "mypkg.tests.unit"],
        ] {
            assert!(
                narrows_selection(&v(a), &project, &project.rootdir),
                "{a:?}"
            );
        }
        for a in [
            &[][..],
            &["-v", "-x"],
            &["--maxfail=2"],
            &["-W", "error::DeprecationWarning"],
            &["-Wignore::UserWarning"],
            &["-o", "filterwarnings=error::DeprecationWarning"],
        ] {
            assert!(
                !narrows_selection(&v(a), &project, &project.rootdir),
                "{a:?}"
            );
        }
    }

    #[test]
    fn uncommitted_edits_are_named() {
        let green = snap("aaa", "3.12.1", &[], &[]);
        let now = Snapshot {
            dirty: true,
            ..green.clone()
        };
        let d = compare(Some(&green), &now, no_git);
        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &d, 1000, false);
        assert!(cap.out().contains("uncommitted changes"), "{}", cap.out());
        assert!(!cap.out().contains("nothing: same commit"), "{}", cap.out());
    }

    #[test]
    fn head_behind_green_is_a_change() {
        // `git log green..HEAD` is empty when HEAD is an ancestor of green.
        let green = snap("bbb", "3.12.1", &[], &[]);
        let now = snap("aaa", "3.12.1", &[], &[]);
        let d = compare(Some(&green), &now, no_git);
        assert!(!d.is_empty());
        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &d, 1000, false);
        assert!(
            cap.out().contains("HEAD aaa is behind the green commit"),
            "{}",
            cap.out()
        );
    }

    #[test]
    fn pytest_addopts_selection_narrows_the_run() {
        let held = crate::test_env::lock();
        let root = std::env::temp_dir().join(format!("rstest-sg-addopts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pytest.ini"), "[pytest]\n").unwrap();
        let args: Vec<String> = Vec::new();
        {
            let _v = crate::test_env::set_var(&held, "PYTEST_ADDOPTS", "-k 'smoke or api'");
            assert!(run_narrows_suite(&args, &root));
        }
        {
            let _v = crate::test_env::set_var(&held, "PYTEST_ADDOPTS", "-W error::UserWarning");
            assert!(!run_narrows_suite(&args, &root));
        }
        let _v = crate::test_env::remove_var(&held, "PYTEST_ADDOPTS");
        assert!(!run_narrows_suite(&args, &root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_suite_is_pytests_rootdir_for_the_args() {
        let held = crate::test_env::lock();
        let _v = crate::test_env::remove_var(&held, "PYTEST_ADDOPTS");
        let root = std::env::temp_dir().join(format!("rstest-sg-rootdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::create_dir_all(root.join("pkgs/a/tests")).unwrap();
        // A plain pyproject.toml (no pytest section) still anchors the
        // rootdir: running from `tests/` is a subset of it.
        std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert!(run_narrows_suite(&[], &root.join("tests")));
        assert!(!run_narrows_suite(&[], &root));
        // A sub-project with its own pytest config is its own suite.
        std::fs::write(
            root.join("pkgs/a/pyproject.toml"),
            "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
        )
        .unwrap();
        let a = root.join("pkgs/a").display().to_string();
        assert!(!run_narrows_suite(&[a], &root));
        let a_tests = root.join("pkgs/a/tests").display().to_string();
        assert!(!run_narrows_suite(&[a_tests], &root));
        // An @argsfile counts by what it lists, rootdir included: one naming
        // the sub-project is that project's whole suite, `-k` narrows.
        std::fs::write(
            root.join("full.txt"),
            format!("{}\n", root.join("pkgs/a").display()),
        )
        .unwrap();
        std::fs::write(root.join("sub.txt"), "-k\nslow\n").unwrap();
        assert!(!run_narrows_suite(&["@full.txt".into()], &root));
        assert!(run_narrows_suite(&["@sub.txt".into()], &root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tracked_dirty_ignores_untracked_report_files() {
        let repo = std::env::temp_dir().join(format!(
            "rstest-since-green-git-{}-{}",
            std::process::id(),
            crate::time::now_epoch_nanos()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        std::fs::write(repo.join("m.py"), "x = 1\n").unwrap();
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        assert!(!tracked_dirty(&repo));
        // A leftover `--junitxml` report is no local edit.
        std::fs::write(repo.join("report.xml"), "<testsuites/>").unwrap();
        assert!(!tracked_dirty(&repo));
        std::fs::write(repo.join("m.py"), "x = 2\n").unwrap();
        assert!(tracked_dirty(&repo));
        std::fs::remove_dir_all(&repo).ok();
    }
}
