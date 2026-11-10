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
             --dist each / --shard); run the suite that way once, or \
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
            // A journal may come from an untrusted run: print control characters
            // (ANSI escapes, carriage returns) escaped, so none can rewrite the
            // line the user is told to read before replaying.
            if a.chars().any(char::is_control) {
                format!("{a:?}")
            } else if a.is_empty() || a.chars().any(char::is_whitespace) {
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

    #[test]
    fn replay_guide_says_the_quarantine_file_still_applies() {
        // run_replay neutralizes reruns and auto-quarantine but keeps
        // `--quarantine` / `[tool.rstest] quarantine`, so a listed victim
        // replays green with no fix. replay.md once said "green means the fix
        // holds" without that caveat.
        let page = crate::doc_pages::pages()
            .into_iter()
            .find(|(p, _)| p == "docs/guides/replay.md")
            .expect("replay.md not found")
            .1;
        assert!(
            crate::doc_pages::sentences(&page)
                .iter()
                .any(|s| s.contains("quarantine file") && s.contains("still applies")),
            "docs/guides/replay.md must say the quarantine file still applies under replay"
        );
    }

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
    fn display_args_escapes_control_characters() {
        let a: Vec<String> = ["-p", "evil\x1b[2K\rtests"].map(String::from).to_vec();
        let shown = display_args(&a);
        assert!(!shown.chars().any(char::is_control), "got {shown:?}");
        assert!(shown.contains("evil"), "got {shown:?}");
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
    fn record_stamps_schema_version_and_time() {
        let before = crate::time::now_epoch_secs();
        let j = Journal::record(
            "uid".into(),
            2,
            "loadfile".into(),
            Some(7),
            vec!["tests".into(), "-k".into(), "slow".into()],
            Some("h".into()),
            3,
            vec![vec!["a::t".into()], vec!["b::t".into(), "c::t".into()]],
        );
        assert_eq!(j.schema, SCHEMA);
        assert_eq!(j.rstest_version, env!("CARGO_PKG_VERSION"));
        assert!(j.created_epoch >= before);
        assert_eq!(
            (
                j.run_uid.as_str(),
                j.workers,
                j.dist.as_str(),
                j.shuffle_seed
            ),
            ("uid", 2, "loadfile", Some(7))
        );
        assert_eq!(j.args, ["tests", "-k", "slow"]);
        let p = j.pinned();
        assert_eq!(p.assignment, j.assignment);
        assert_eq!(
            (p.collection_hash.as_deref(), p.collection_size),
            (Some("h"), 3)
        );
    }

    #[test]
    fn journaling_honors_the_opt_out_env() {
        let held = crate::test_env::lock();
        {
            let _v = crate::test_env::remove_var(&held, "RSTEST_NO_REPLAY_JOURNAL");
            assert!(journaling_enabled());
        }
        {
            let _v = crate::test_env::set_var(&held, "RSTEST_NO_REPLAY_JOURNAL", "1");
            assert!(!journaling_enabled());
        }
        let _v = crate::test_env::set_var(&held, "RSTEST_NO_REPLAY_JOURNAL", "false");
        assert!(journaling_enabled());
    }

    #[test]
    fn load_pinned_returns_the_journal_and_its_schedule() {
        with_cache_dir(|_| {
            let j = journal("pin", vec![vec!["a::t1"], vec!["a::t2", "a::t3"]]);
            write(&j);
            let (got, pinned) = load_pinned(Some("pin"), None).unwrap();
            assert_eq!(got, j);
            assert_eq!(pinned.assignment, j.assignment);
            assert_eq!(pinned.collection_hash.as_deref(), Some("abc"));
            assert_eq!(pinned.collection_size, 3);
            assert!(load_pinned(Some("absent"), None).is_err());
        });
    }

    #[test]
    fn load_reports_unparseable_journal() {
        with_cache_dir(|dir| {
            let p = dir.join("junk.json");
            std::fs::write(&p, b"{not json").unwrap();
            let err = load(None, Some(&p)).unwrap_err().to_string();
            assert_eq!(err, format!("parsing {} as a replay journal", p.display()));
        });
    }

    // Unix path spelling (`/no/such/...` is not absolute on Windows).
    #[cfg(unix)]
    #[test]
    fn run_replay_warns_about_the_journal_then_runs_its_args() {
        // The recorded args reach the run: `--looponfail` is rejected there,
        // before any interpreter is needed, so the run fails fast and the
        // warnings printed on the way are observable.
        let held = crate::test_env::lock();
        let base = std::env::temp_dir().join(format!(
            "rstest-replay-run-{}-{}",
            std::process::id(),
            crate::time::now_epoch_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let _remote = crate::test_env::remove_var(&held, "RSTEST_CACHE_REMOTE");
        let workers = 4 * std::thread::available_parallelism().map_or(1, usize::from) + 1;
        let mut j = journal("r", vec![vec!["a::t"]]);
        j.workers = workers;
        j.assignment.resize(workers, Vec::new());
        j.collection_size = 1;
        j.args = [
            "--looponfail",
            "--lf",
            "/no/such/dir/tests",
            "-k",
            "not slow",
        ]
        .map(String::from)
        .to_vec();
        let path = base.join("j.json");
        std::fs::write(&path, serde_json::to_vec(&j).unwrap()).unwrap();
        let cli = <crate::Cli as clap::Parser>::parse_from(["rstest"]);
        let (mut sink, cap) = crate::reporting::sink::Sink::captured();
        let err = run_replay(&cli, None, Some(&path), &mut sink)
            .unwrap_err()
            .to_string();
        let _ = std::fs::remove_dir_all(&base);
        assert!(err.starts_with("--looponfail is pytest-xdist's"), "{err}");
        let lines: Vec<String> = cap.err().lines().map(String::from).collect();
        assert_eq!(
            lines[0],
            format!("rstest: replay: run r (test recorded), {workers} worker(s), 1 test(s) across {workers} slot(s)")
        );
        assert_eq!(
            lines[1],
            "rstest: replay: args: --looponfail --lf /no/such/dir/tests -k 'not slow'"
        );
        assert!(
            lines[2].starts_with(&format!(
                "rstest: replay: the journal records {workers} workers"
            )),
            "{lines:?}"
        );
        assert!(
            lines[3]
                .starts_with("rstest: replay: recorded arg /no/such/dir/tests is an absolute path"),
            "{lines:?}"
        );
        assert!(
            lines[4].starts_with("rstest: replay: ignoring recorded --lf (they select"),
            "{lines:?}"
        );
        assert_eq!(lines.len(), 5, "{lines:?}");
        // A missing journal fails before printing anything.
        let (mut sink, cap) = crate::reporting::sink::Sink::captured();
        assert!(run_replay(&cli, None, Some(&base.join("gone.json")), &mut sink).is_err());
        assert_eq!(cap.err(), "");
    }

    #[cfg(unix)]
    #[test]
    fn canonical_lenient_keeps_the_missing_tail() {
        let tmp = std::env::temp_dir();
        let canon = std::fs::canonicalize(&tmp).unwrap();
        assert_eq!(canonical_lenient(&tmp), canon);
        assert_eq!(
            canonical_lenient(&tmp.join("no-such-dir-xyz/f.py")),
            canon.join("no-such-dir-xyz/f.py")
        );
        // A path under cwd that does not exist yet still relativizes.
        assert_eq!(
            relativize(&format!("{}/new/t.py::x", tmp.display()), &canon),
            "new/t.py::x"
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
