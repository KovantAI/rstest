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
    if let Ok(bytes) = serde_json::to_vec(snap) {
        let _ = cache::write_atomic(&cache::file(FILE), &bytes);
    }
}

/// The last green state, or `None` when no run has been green yet (or the
/// file is from another schema).
pub fn load() -> Option<Snapshot> {
    let bytes = std::fs::read(cache::file(FILE)).ok()?;
    serde_json::from_slice::<Snapshot>(&bytes)
        .ok()
        .filter(|s| s.schema == SCHEMA)
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
/// `PYTEST_ADDOPTS`? See [`narrows_suite`].
pub(crate) fn run_narrows_suite(args: &[String]) -> bool {
    narrows_suite(args)
        || std::env::var("PYTEST_ADDOPTS")
            .is_ok_and(|v| narrows_suite(&crate::config::shell_split(&v)))
}

/// Does the session argv select part of the suite? Positional nodeids,
/// `-k`/`-m`, `--deselect`, last-failed/stepwise, `--co`. A plain path
/// selection counts as the suite: CI commonly runs `rstest tests/`. Only
/// positionals are nodeids: `-W error::DeprecationWarning` is no selection.
fn narrows_suite(args: &[String]) -> bool {
    const FLAGS: [&str; 7] = [
        "--deselect",
        "--lf",
        "--last-failed",
        "--sw",
        "--stepwise",
        "--co",
        "--collect-only",
    ];
    args.iter()
        .zip(crate::cli::positional_mask(args))
        .any(|(a, pos)| {
            (pos && a.contains("::"))
                || FLAGS
                    .iter()
                    .any(|f| a == f || a.strip_prefix(f).is_some_and(|r| r.starts_with('=')))
                || crate::cli::has_short(a, 'k')
                || crate::cli::has_short(a, 'm')
        })
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
    fn narrows_suite_spots_subsets() {
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
        ] {
            assert!(narrows_suite(&v(a)), "{a:?}");
        }
        for a in [
            &[][..],
            &["tests/"],
            &["-v", "-x"],
            &["--maxfail=2"],
            &["-W", "error::DeprecationWarning"],
            &["-Wignore::UserWarning"],
            &["-o", "filterwarnings=error::DeprecationWarning"],
        ] {
            assert!(!narrows_suite(&v(a)), "{a:?}");
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
        let args: Vec<String> = Vec::new();
        {
            let _v = crate::test_env::set_var(&held, "PYTEST_ADDOPTS", "-k 'smoke or api'");
            assert!(run_narrows_suite(&args));
        }
        {
            let _v = crate::test_env::set_var(&held, "PYTEST_ADDOPTS", "-W error::UserWarning");
            assert!(!run_narrows_suite(&args));
        }
        let _v = crate::test_env::remove_var(&held, "PYTEST_ADDOPTS");
        assert!(!run_narrows_suite(&args));
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
