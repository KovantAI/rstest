//! Deterministic parallel replay: journal a run's exact per-worker schedule, and
//! re-run that same assignment on demand.
//!
//! rstest owns dispatch (worker assignment + order), so a parallel-only failure
//! that pytest/xdist would call irreproducible is, here, recordable. Every pool
//! run (`-n >= 2`) writes a journal to `.rstest_cache/replay/`: which worker ran
//! which tests, in what order. `rstest replay` re-pins that assignment.
//!
//! Primary flow is CI -> local: the failing CI run journals without foresight;
//! CI uploads `.rstest_cache/replay/latest.json` as an artifact; a developer runs
//! `rstest replay --journal latest.json` locally. Because indices are
//! machine-local (positions in pytest's collection order), the journal keys on
//! NODEID and the replay re-resolves nodeid -> local index after collecting, so
//! it survives the machine hop. The recorded collection hash lets replay warn
//! when the suite drifted since the journal was written.
//!
//! Determinism is per-worker: each worker runs exactly its recorded nodeids in
//! the recorded order, with work-stealing and reruns off. That reproduces
//! worker-local order and assignment (state-ordering flakes). Exact cross-worker
//! interleaving stays timing-dependent, as the design intends: best-effort for
//! genuinely time-dependent races, exact for state-ordering ones.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache;

/// Schema version of the on-disk journal. Bump on any incompatible change; an
/// older/newer journal is rejected with a clear message rather than misread.
const SCHEMA: u32 = 1;

/// Subdirectory of `.rstest_cache` holding replay journals.
pub const DIR: &str = "replay";

/// The pointer file always overwritten with the latest run's journal, so CI can
/// upload a stable path (`.rstest_cache/replay/latest.json`) without knowing the
/// run uid.
pub const LATEST: &str = "latest.json";

/// How many per-uid journals to keep; older ones are pruned on each write so the
/// directory stays bounded. `latest.json` is separate and never counts/prunes.
const KEEP: usize = 10;

/// A recorded run's schedule: enough to re-pin the exact per-worker assignment
/// on any machine. Portable by construction (nodeids, not indices; no absolute
/// paths).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Journal {
    pub schema: u32,
    pub rstest_version: String,
    /// The recording run's uid (xdist `testrun_uid`); also the file stem.
    pub run_uid: String,
    /// Unix epoch seconds when the journal was written.
    pub created_epoch: u64,
    /// Worker count of the recording run; replay forces `-n` to this.
    pub workers: usize,
    /// `--dist` mode of the recording run (informational; replay pins the exact
    /// per-worker lists, so the dist heuristic is not re-run).
    pub dist: String,
    /// `--shuffle` seed of the recording run, if any (informational; the pinned
    /// order already encodes the effect of the shuffle).
    pub shuffle_seed: Option<u64>,
    /// The pytest session args forwarded to the workers, so replay collects the
    /// identical suite (same `-k`/`-m`/paths/plugins).
    pub args: Vec<String>,
    /// sha256 of the ordered nodeid list the recording run agreed on, for a
    /// drift check at replay time. `None` if the run never surfaced one.
    pub collection_hash: Option<String>,
    /// Number of collected tests on the recording run.
    pub collection_size: u64,
    /// Per-worker ordered nodeids, outer index = worker slot (gw0, gw1, ...).
    /// This is the emergent assignment, captured from each `item_start`.
    pub assignment: Vec<Vec<String>>,
}

/// What `run_pool` needs to re-pin a schedule: the per-worker nodeid lists plus
/// the recorded collection identity for the drift check.
#[derive(Debug, Clone)]
pub struct PinnedSchedule {
    pub assignment: Vec<Vec<String>>,
    pub collection_hash: Option<String>,
    pub collection_size: u64,
}

impl Journal {
    /// Build a journal from a recording run's facts, stamping the current schema
    /// and rstest version. The heavy `assignment`/`args` are moved in.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        run_uid: String,
        workers: usize,
        dist: String,
        shuffle_seed: Option<u64>,
        args: Vec<String>,
        collection_hash: Option<String>,
        collection_size: u64,
        assignment: Vec<Vec<String>>,
    ) -> Self {
        Journal {
            schema: SCHEMA,
            rstest_version: env!("CARGO_PKG_VERSION").to_string(),
            run_uid,
            created_epoch: crate::time::now_epoch_secs(),
            workers,
            dist,
            shuffle_seed,
            args: std::env::current_dir()
                .map(|cwd| portable_args(args.clone(), &cwd))
                .unwrap_or(args),
            collection_hash,
            collection_size,
            assignment,
        }
    }

    fn pinned(&self) -> PinnedSchedule {
        PinnedSchedule {
            assignment: self.assignment.clone(),
            collection_hash: self.collection_hash.clone(),
            collection_size: self.collection_size,
        }
    }
}

/// True unless journaling is explicitly disabled. The journal is on by default
/// so a CI failure is replayable without having enabled anything first; opt out
/// with `RSTEST_NO_REPLAY_JOURNAL=1`. Empty, `0` and `false` leave it on, so
/// `RSTEST_NO_REPLAY_JOURNAL=0` means what it reads as.
pub fn journaling_enabled() -> bool {
    std::env::var("RSTEST_NO_REPLAY_JOURNAL")
        .map(|v| opt_out_off(&v))
        .unwrap_or(true)
}

/// True when an opt-out value does NOT opt out (empty / `0` / `false`).
fn opt_out_off(v: &str) -> bool {
    let v = v.trim();
    v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false")
}

/// Write `journal` to `<cache>/replay/<uid>.json`, refresh `latest.json`, and
/// prune older per-uid files to [`KEEP`]. Best-effort: journaling never fails a
/// run, so all IO errors are swallowed. An empty assignment (nothing dispatched)
/// is not journaled.
pub fn write(journal: &Journal) {
    if journal.assignment.iter().all(|w| w.is_empty()) {
        return;
    }
    let Ok(bytes) = serde_json::to_vec_pretty(journal) else {
        return;
    };
    let dir = cache::dir().join(DIR);
    let file = dir.join(format!("{}.json", sanitize(&journal.run_uid)));
    if cache::write_atomic(&file, &bytes).is_err() {
        return;
    }
    let _ = cache::write_atomic(&dir.join(LATEST), &bytes);
    prune(&dir);
}

/// Keep only the newest [`KEEP`] per-uid journals (by modified time), deleting
/// older ones. `latest.json` is skipped (it is the stable pointer, not a
/// per-uid file). Best-effort.
fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            if name == LATEST || !name.ends_with(".json") {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, path))
        })
        .collect();
    if files.len() <= KEEP {
        return;
    }
    // Newest first, then drop everything past the retention window.
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    for (_, path) in files.into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

/// Resolve which journal to replay, from the subcommand inputs:
/// - `path` (from `--journal`): read that file verbatim (the CI-artifact case);
/// - else `run_id`: `<cache>/replay/<run_id>.json`;
/// - else the latest: `<cache>/replay/latest.json`.
///
/// A hard error (missing file / bad JSON / schema mismatch) is returned so the
/// user learns why replay can't proceed, with a hint at the fix.
pub fn load(run_id: Option<&str>, path: Option<&Path>) -> Result<Journal> {
    let file = match (path, run_id) {
        (Some(p), _) => p.to_path_buf(),
        (None, Some(id)) => cache::dir()
            .join(DIR)
            .join(format!("{}.json", sanitize(id))),
        (None, None) => cache::dir().join(DIR).join(LATEST),
    };
    if !file.exists() {
        anyhow::bail!(
            "no replay journal at {}. Only parallel runs record one (-n >= 2, not \
             --dist each / --shard / --collect lazy); run the suite that way once, or \
             pass --journal <file> for a downloaded CI artifact",
            file.display()
        );
    }
    let bytes = std::fs::read(&file)
        .with_context(|| format!("reading replay journal {}", file.display()))?;
    let journal: Journal = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {} as a replay journal", file.display()))?;
    if journal.schema != SCHEMA {
        anyhow::bail!(
            "replay journal {} has schema {} but this rstest expects {SCHEMA}; \
             re-record it with this version",
            file.display(),
            journal.schema
        );
    }
    validate(&journal).with_context(|| format!("invalid replay journal {}", file.display()))?;
    Ok(journal)
}

/// Shape checks `serde` can't express. Each worker slot must have exactly one
/// list: extra lists would never run (and nothing would say so), missing ones
/// would silently idle a worker.
fn validate(journal: &Journal) -> Result<()> {
    if journal.workers < 2 {
        anyhow::bail!(
            "records {} worker(s); replay needs a parallel recording (-n >= 2)",
            journal.workers
        );
    }
    if journal.assignment.len() != journal.workers {
        anyhow::bail!(
            "records {} worker(s) but {} per-worker test list(s)",
            journal.workers,
            journal.assignment.len()
        );
    }
    Ok(())
}

/// Render recorded args for display, quoting a token that has whitespace (or
/// is empty) so `-k 'not slow'` reads as the two tokens it is.
fn display_args(args: &[String]) -> String {
    if args.is_empty() {
        return "(none)".into();
    }
    args.iter()
        .map(|a| {
            if a.is_empty() || a.chars().any(char::is_whitespace) {
                format!("'{a}'")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Convenience for the command layer: load and hand back the [`PinnedSchedule`]
/// plus the whole journal (the caller needs `args`/`workers`/`dist` to rebuild
/// the run).
pub fn load_pinned(run_id: Option<&str>, path: Option<&Path>) -> Result<(Journal, PinnedSchedule)> {
    let journal = load(run_id, path)?;
    let pinned = journal.pinned();
    Ok((journal, pinned))
}

/// `rstest replay` entry point: load the journal, rebuild the run from it (force
/// `-n` to the recorded worker count, `--collect full`, no reruns/shuffle/shard/
/// incremental), and re-run it with the recorded per-worker schedule pinned.
/// Returns the run's exit status.
pub fn run_replay(
    cli: &crate::Cli,
    run_id: Option<&str>,
    journal_path: Option<&Path>,
    sink: &mut crate::reporting::sink::Sink,
) -> Result<i32> {
    let (journal, pinned) = load_pinned(run_id, journal_path)?;
    sink.warn(&format!(
        "rstest: replay: run {} ({} recorded), {} worker(s), {} test(s) across {} slot(s)",
        journal.run_uid,
        journal.rstest_version,
        journal.workers,
        journal.collection_size,
        journal.assignment.len(),
    ));
    // Always shown: these are handed to pytest verbatim, and a journal is often
    // a downloaded artifact (possibly from a fork's CI) the user never opened.
    sink.warn(&format!(
        "rstest: replay: args: {}",
        display_args(&journal.args)
    ));
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    if journal.workers > cores.saturating_mul(4) {
        sink.warn(&format!(
            "rstest: replay: the journal records {} workers and this machine has {cores} \
             core(s); replay starts all {} to keep the recorded assignment",
            journal.workers, journal.workers
        ));
    }
    for arg in missing_absolute_paths(&journal.args) {
        sink.warn(&format!(
            "rstest: replay: recorded arg {arg} is an absolute path that does not exist on \
             this machine; re-record with a path relative to the project"
        ));
    }
    let (args, dropped) = strip_cache_selection(&journal.args);
    if !dropped.is_empty() {
        sink.warn(&format!(
            "rstest: replay: ignoring recorded {} (they select tests from this machine's \
             pytest cache, not the recording's; the journal already pins what ran)",
            dropped.join(" ")
        ));
    }
    // Rebuild the CLI from the recording run's shape, neutralizing every mode
    // that would perturb the pinned schedule. The session args (paths/-k/-m/
    // plugins) come from the journal, not this invocation's argv.
    let mut c = cli.clone();
    c.command = None;
    c.numprocesses = Some(journal.workers.to_string());
    c.collect = Some("full".into());
    c.dist = Some(journal.dist.clone());
    // Some(0), not None: None falls back to `[tool.rstest] reruns`, and a
    // retried attempt has no queue to go to under replay, so the failure being
    // reproduced would be dropped.
    c.reruns = Some(0);
    c.shuffle = None;
    c.shard = None;
    c.incremental = false;
    c.since_green = false;
    c.changed = None;
    c.changed_strict = false;
    c.watch = false;
    c.cache_pull = false;
    c.cache_push = false;
    crate::run::execute_inner(&c, &args, Some(&pinned))
}

/// pytest cacheprovider flags that FILTER the collection by the local cache
/// (`--lf` keeps last-failed, `--sw` skips up to the last stepwise failure).
/// Replayed from a CI journal they'd select by the developer's cache and drop
/// the very tests being reproduced. Reorder-only flags (`--ff`, `--nf`) are
/// harmless (replay maps by nodeid) and stay.
const CACHE_SELECTION_FLAGS: &[&str] = &[
    "--lf",
    "--last-failed",
    "--sw",
    "--stepwise",
    "--sw-skip",
    "--stepwise-skip",
    "--sw-reset",
    "--stepwise-reset",
];
/// Same family, taking a value (`--lfnf all` or `--lfnf=all`).
const CACHE_SELECTION_VALUED: &[&str] = &["--lfnf", "--last-failed-no-failures"];

/// `args` without the cache-driven selection flags, plus the dropped tokens
/// (for one warning).
fn strip_cache_selection(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut kept = Vec::with_capacity(args.len());
    let mut dropped = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let name = a.split_once('=').map_or(a.as_str(), |(f, _)| f);
        if CACHE_SELECTION_FLAGS.contains(&a.as_str()) {
            dropped.push(a.clone());
        } else if CACHE_SELECTION_VALUED.contains(&name) {
            dropped.push(a.clone());
            if !a.contains('=') {
                if let Some(v) = it.next() {
                    dropped.push(v.clone());
                }
            }
        } else {
            kept.push(a.clone());
        }
    }
    (kept, dropped)
}

/// Rewrite absolute paths under `cwd` in the session args to cwd-relative ones,
/// so a journal recorded on CI (`rstest -n auto $GITHUB_WORKSPACE/tests`)
/// replays from the same project directory on another machine. Covers bare
/// paths, nodeids (`/abs/test_x.py::test`) and `--flag=/abs/path`. Paths
/// outside `cwd`, and everything that isn't an absolute path, pass through.
fn portable_args(args: Vec<String>, cwd: &Path) -> Vec<String> {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    args.into_iter()
        .map(|a| match a.split_once('=') {
            Some((flag, val)) if flag.starts_with("--") => {
                format!("{flag}={}", relativize(val, &cwd))
            }
            _ if a.starts_with('-') => a,
            _ => relativize(&a, &cwd),
        })
        .collect()
}

/// `token` made relative to `cwd` if its path part (before any `::`) is
/// absolute and under `cwd`; else unchanged. Joined with `/`, which pytest
/// accepts on every platform, so a Windows-recorded journal replays on unix.
fn relativize(token: &str, cwd: &Path) -> String {
    let (path, rest) = token.find("::").map_or((token, ""), |i| token.split_at(i));
    let p = Path::new(path);
    if !p.is_absolute() {
        return token.to_string();
    }
    let p = canonical_lenient(p);
    let Ok(rel) = p.strip_prefix(cwd) else {
        return token.to_string();
    };
    let rel: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let rel = if rel.is_empty() {
        ".".to_string()
    } else {
        rel.join("/")
    };
    format!("{rel}{rest}")
}

/// Canonicalize so a symlinked spelling (macOS `/tmp` -> `/private/tmp`) still
/// matches the canonical cwd. A path that doesn't exist (yet) canonicalizes its
/// longest existing ancestor and keeps the rest verbatim.
fn canonical_lenient(p: &Path) -> PathBuf {
    for anc in p.ancestors() {
        if let Ok(c) = std::fs::canonicalize(anc) {
            return match p.strip_prefix(anc) {
                Ok(rest) if !rest.as_os_str().is_empty() => c.join(rest),
                _ => c,
            };
        }
    }
    p.to_path_buf()
}

/// Recorded args whose path part is absolute and missing here: the usual cause
/// of a replay that collects nothing (a journal from an older rstest, or a path
/// outside the project, still carries the CI machine's path).
fn missing_absolute_paths(args: &[String]) -> Vec<&str> {
    args.iter()
        .map(String::as_str)
        .filter(|a| {
            let val = match a.split_once('=') {
                Some((flag, val)) if flag.starts_with("--") => val,
                _ if a.starts_with('-') => return false,
                _ => a,
            };
            let path = val.split("::").next().unwrap_or(val);
            let p = Path::new(path);
            p.is_absolute() && !p.exists()
        })
        .collect()
}

/// Sanitize a run uid / user-supplied id into a safe single-path-component file
/// stem: keep `[A-Za-z0-9._-]`, replace the rest with `_`. Guards against a
/// `--journal ../../etc` style id turning into a path traversal, and against a
/// uid that ever grows exotic characters.
fn sanitize(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // A leading run of dots (`.`, `..`) would still be a traversal component.
    if s.chars().all(|c| c == '.') {
        "_".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(uid: &str, assignment: Vec<Vec<&str>>) -> Journal {
        Journal {
            schema: SCHEMA,
            rstest_version: "test".into(),
            run_uid: uid.into(),
            created_epoch: 0,
            workers: assignment.len(),
            dist: "load".into(),
            shuffle_seed: None,
            args: vec!["tests/".into()],
            collection_hash: Some("abc".into()),
            collection_size: assignment.iter().map(|w| w.len() as u64).sum(),
            assignment: assignment
                .into_iter()
                .map(|w| w.into_iter().map(String::from).collect())
                .collect(),
        }
    }

    /// Serializes every test that mutates the process-global `RSTEST_CACHE`, so
    /// two `with_cache_dir` closures never clobber each other's env/dir.
    fn with_cache_dir<T>(f: impl FnOnce(&Path) -> T) -> T {
        // Held for the whole closure: RSTEST_CACHE is process-global, so these
        // tests must not overlap with any other env-touching test.
        let held = crate::test_env::lock();
        let base = std::env::temp_dir().join(format!(
            "rstest-replay-{}-{}",
            std::process::id(),
            crate::time::now_epoch_nanos()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let out = {
            let _cache = crate::test_env::set_var(&held, "RSTEST_CACHE", &base);
            f(&base)
        };
        let _ = std::fs::remove_dir_all(&base);
        out
    }

    #[test]
    fn write_then_load_roundtrips_by_uid_and_latest() {
        with_cache_dir(|_| {
            let j = journal("run1", vec![vec!["a::t1", "a::t3"], vec!["a::t2"]]);
            write(&j);
            // By uid.
            assert_eq!(load(Some("run1"), None).unwrap(), j);
            // Via latest pointer.
            assert_eq!(load(None, None).unwrap(), j);
        });
    }

    #[test]
    fn load_missing_is_a_helpful_error() {
        with_cache_dir(|_| {
            let err = load(Some("nope"), None).unwrap_err().to_string();
            assert!(err.contains("no replay journal at"), "{err}");
            assert!(err.contains("--journal"), "hint at the CI path: {err}");
        });
    }

    #[test]
    fn schema_mismatch_is_rejected() {
        with_cache_dir(|dir| {
            let mut j = journal("bad", vec![vec!["a::t"]]);
            j.schema = 999;
            let path = dir.join(DIR).join("bad.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_vec(&j).unwrap()).unwrap();
            let err = load(Some("bad"), None).unwrap_err().to_string();
            assert!(err.contains("schema 999"), "{err}");
        });
    }

    #[test]
    fn empty_assignment_is_not_written() {
        with_cache_dir(|dir| {
            write(&journal("empty", vec![vec![], vec![]]));
            assert!(!dir.join(DIR).join("empty.json").exists());
        });
    }

    #[test]
    fn explicit_journal_path_wins_over_run_id() {
        with_cache_dir(|dir| {
            let j = journal("artifact", vec![vec!["x::t"], vec![]]);
            let p = dir.join("downloaded.json");
            std::fs::write(&p, serde_json::to_vec(&j).unwrap()).unwrap();
            assert_eq!(load(Some("ignored"), Some(&p)).unwrap(), j);
        });
    }

    #[test]
    fn prune_keeps_newest_and_latest() {
        with_cache_dir(|dir| {
            let rdir = dir.join(DIR);
            for i in 0..(KEEP + 5) {
                // Distinct uids; write() refreshes latest and prunes each time.
                write(&journal(&format!("r{i:02}"), vec![vec!["a::t"]]));
            }
            let jsons: Vec<String> = std::fs::read_dir(&rdir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".json"))
                .collect();
            // KEEP per-uid files + latest.json.
            assert!(
                jsons.contains(&LATEST.to_string()),
                "latest kept: {jsons:?}"
            );
            let per_uid = jsons.iter().filter(|n| *n != LATEST).count();
            assert_eq!(per_uid, KEEP, "pruned to KEEP: {jsons:?}");
        });
    }

    // Unix path spellings (`/definitely/...` is not absolute on Windows).
    #[cfg(unix)]
    #[test]
    fn portable_args_relativizes_paths_under_cwd() {
        let base = std::env::temp_dir().join(format!(
            "rstest-replay-args-{}-{}",
            std::process::id(),
            crate::time::now_epoch_nanos()
        ));
        let tests = base.join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        let t = tests.display().to_string();
        let b = base.display().to_string();
        let got = portable_args(
            vec![
                t.clone(),
                format!("{t}/test_x.py::test_y"),
                format!("--rootdir={b}"),
                b.clone(),
                "-k".into(),
                "slow".into(),
                "/definitely/not/under/cwd".into(),
                "rel/path".into(),
            ],
            &base,
        );
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(
            got,
            vec![
                "tests",
                "tests/test_x.py::test_y",
                "--rootdir=.",
                ".",
                "-k",
                "slow",
                "/definitely/not/under/cwd",
                "rel/path",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_absolute_paths_flags_only_absent_absolute_paths() {
        let here = std::env::temp_dir().display().to_string();
        let args: Vec<String> = vec![
            "/no/such/dir/tests".into(),
            "/no/such/t.py::test".into(),
            "--basetemp=/no/such/tmp".into(),
            here,
            "tests".into(),
            "-q".into(),
        ];
        assert_eq!(
            missing_absolute_paths(&args),
            vec![
                "/no/such/dir/tests",
                "/no/such/t.py::test",
                "--basetemp=/no/such/tmp"
            ]
        );
    }

    #[test]
    fn load_rejects_worker_list_mismatch() {
        with_cache_dir(|dir| {
            let mut j = journal("mismatch", vec![vec!["a::t1"], vec!["a::t2"]]);
            j.workers = 3;
            let p = dir.join("m.json");
            std::fs::write(&p, serde_json::to_vec(&j).unwrap()).unwrap();
            let err = format!("{:#}", load(None, Some(&p)).unwrap_err());
            assert!(err.contains("3 worker(s) but 2"), "{err}");

            j.workers = 1;
            j.assignment.truncate(1);
            std::fs::write(&p, serde_json::to_vec(&j).unwrap()).unwrap();
            let err = format!("{:#}", load(None, Some(&p)).unwrap_err());
            assert!(err.contains("-n >= 2"), "{err}");
        });
    }

    #[test]
    fn opt_out_value_parsing() {
        for on in ["", "0", "false", "FALSE", " 0 "] {
            assert!(opt_out_off(on), "{on:?} keeps journaling on");
        }
        for off in ["1", "true", "yes"] {
            assert!(!opt_out_off(off), "{off:?} turns journaling off");
        }
    }

    #[test]
    fn display_args_quotes_whitespace_tokens() {
        let a: Vec<String> = ["tests", "-k", "not slow", ""].map(String::from).to_vec();
        assert_eq!(display_args(&a), "tests -k 'not slow' ''");
        assert_eq!(display_args(&[]), "(none)");
    }

    #[test]
    fn strip_cache_selection_drops_filtering_flags_only() {
        let args: Vec<String> = [
            "tests",
            "--lf",
            "--ff",
            "--lfnf",
            "all",
            "--sw",
            "-k",
            "slow",
            "--last-failed-no-failures=none",
            "--stepwise-skip",
            "--nf",
        ]
        .map(String::from)
        .to_vec();
        let (kept, dropped) = strip_cache_selection(&args);
        assert_eq!(kept, ["tests", "--ff", "-k", "slow", "--nf"]);
        assert_eq!(
            dropped,
            [
                "--lf",
                "--lfnf",
                "all",
                "--sw",
                "--last-failed-no-failures=none",
                "--stepwise-skip"
            ]
        );
    }

    #[test]
    fn sanitize_blocks_traversal_and_exotic_chars() {
        assert_eq!(sanitize("a1b2._-"), "a1b2._-");
        // Slashes/backslashes become underscores, so the result is always a
        // single path component (no traversal); embedded dots are harmless once
        // no separators remain.
        assert_eq!(sanitize("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize("."), "_");
        assert_eq!(sanitize("a/b\\c"), "a_b_c");
    }
}
