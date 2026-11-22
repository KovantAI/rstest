//! Dispatch-level incremental skip: after collection, don't dispatch tests that
//! are provably unaffected AND were green last run — inject them as cached
//! passes instead. Content-addressed via the coverage index (per-file hash +
//! line→nodeids), so it needs no git and skips at per-TEST granularity.
//!
//! Soundness: a test is skipped only when every source file it *executed* last
//! time is byte-identical now. A change to a covered file (its own test file
//! included — the test code is covered) busts it. Changes the coverage can't
//! see are guarded separately: a config-file change (markers/addopts/coverage
//! config) disables skipping wholesale, and an in-place dependency upgrade is
//! the known gap shared with `--changed` (bust by deleting the cache file).
//!
//! Two more coverage-invisible inputs are guarded:
//! - import-time code. A module-level constant runs while the test module is
//!   imported, under coverage's empty context, so no test owns those lines.
//!   Each green test file's transitive import closure (package `__init__`s
//!   included) is recorded with content hashes ([`test_import_closures`]); a
//!   change to any of them re-runs that file's tests.
//! - data files. Every git-tracked non-Python file folds into the config
//!   fingerprint ([`tracked_data_ids`]), so editing a fixture a test reads
//!   busts skipping wholesale. Untracked/ignored files, and every non-Python
//!   file outside a git checkout, remain a documented gap.
//!
//! One more coverage-invisible gap, now CLOSED: FIRST-PARTY source coverage
//! doesn't MEASURE. A narrowed `--cov=<pkg>` (on the CLI, in `addopts`, or via
//! the coverage config's `source`), an `include`, or an `omit` leaves some
//! first-party files unmeasured ([`CovScope`]); a test importing one records no
//! coverage for it, so editing it would leave every tracked hash byte-identical
//! and wrongly cache the test (a stale false-green). [`config_state`] closes it:
//! - every unmeasured NON-test `.py` folds into the config fingerprint, so an
//!   edit to any of them busts skipping wholesale — sound, if coarse;
//! - unmeasured TEST-named files (`test_*.py` / `*_test.py`) are tracked in
//!   [`ConfigState::test_named`] instead. A changed one that is a recorded test
//!   module is guarded per-test by [`Baseline::test_file_hashes`]; a changed one
//!   holding no green test is a shared helper (`test_utils.py`, a `TestMixin`
//!   base) and busts skipping wholesale ([`skippable_now`]).
//!
//! Which files count is decided by one pruned tree walk (dot-dirs,
//! [`PRUNE_DIRS`], and virtualenvs of any name by `pyvenv.cfg`), identical with
//! or without git: gitignored first-party code (generated `*_pb2.py`),
//! submodules, and nested repos all count, since a test may import any of
//! them. Inside a git checkout, git only makes hashing cheap: clean tracked
//! files reuse their index blob id, the rest go through `git hash-object`
//! ([`git_blob_ids`]); outside git each is SHA-256 hashed. Residual gap: a
//! real test module that another test also imports. `--cov=.` with no
//! include/omit restores per-file granularity; one full run re-establishes, or
//! delete the cache file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache;
use crate::cov_scope::CovScope;
use crate::select::{current_sha256, CoverageFile, CoverageIndex, ModuleKey, COVERAGE_INDEX_FILE};

/// Filename of the per-test outcome store within the cache dir.
pub const FILE: &str = "incremental_outcomes.json";

const SCHEMA: u32 = 6;

/// Config files whose change invalidates the whole skip decision: markers,
/// addopts, and coverage config aren't reflected in per-test coverage, so a
/// change to any of them disables skipping for that run.
/// The pytest 9 names are appended (not merged into probe order) so an existing
/// project's fingerprint is unchanged by their addition.
const CONFIG_FILES: [&str; 8] = [
    "pyproject.toml",
    "pytest.ini",
    "setup.cfg",
    "tox.ini",
    ".coveragerc",
    "pytest.toml",
    ".pytest.toml",
    ".pytest.ini",
];

/// Directories never worth descending into when hunting for `conftest.py`:
/// virtualenvs, VCS, caches. Keeps the walk bounded on real projects.
const PRUNE_DIRS: [&str; 7] = [
    ".git",
    ".venv",
    "venv",
    "node_modules",
    "__pycache__",
    ".rstest_cache",
    ".tox",
];

#[derive(Default, Serialize, Deserialize)]
struct Outcomes {
    #[serde(default)]
    schema: u32,
    /// Config fingerprint at record time; a change disables skipping next run.
    #[serde(default)]
    config_fp: String,
    /// nodeids that were GREEN (passed, no fail/error) on the recorded run.
    #[serde(default)]
    green: HashSet<String>,
    /// Test-file relpath -> content hash at record time. The coverage index may
    /// not include test files (e.g. `--cov=<pkg>` scopes coverage to the
    /// package), so a test's OWN file is tracked here independently: editing it
    /// must bust the skip even though the index never measured it.
    #[serde(default)]
    test_file_hashes: HashMap<String, String>,
    /// nodeid -> source def line at record time. A cached (not-run) test has no
    /// pytest report, so its report-json/junit line would be blank; restoring it
    /// from here keeps every artifact's line accurate. Its file is unchanged
    /// (that is why it was skipped), so the line is still valid.
    #[serde(default)]
    test_lines: HashMap<String, u64>,
    /// [`ConfigState::test_named`] at the start of the recorded run.
    #[serde(default)]
    test_named_hashes: HashMap<String, String>,
    /// Every file in some green test file's import closure -> its content hash
    /// at record time (see [`test_import_closures`]).
    #[serde(default)]
    import_hashes: HashMap<String, String>,
    /// Test-file relpath -> the files its import closure reached (keys of
    /// `import_hashes`).
    #[serde(default)]
    test_imports: HashMap<String, Vec<String>>,
    /// Import-closure file hashed at module level -> how (see
    /// [`ModuleHash`]). Absent for a file hashed whole (not measured by
    /// coverage, or the helper couldn't parse it).
    #[serde(default)]
    import_keep: HashMap<String, ModuleHash>,
    /// [`ConfigState::index_cov_args`] of the recorded run.
    #[serde(default)]
    index_cov_args: Option<Vec<String>>,
    /// [`ConfigState::index_cov_cwd`] of the recorded run.
    #[serde(default)]
    index_cov_cwd: Option<String>,
}

/// The recorded green baseline: which tests passed and the content hashes of
/// their source files, both gated by the config fingerprint.
#[derive(Default)]
pub struct Baseline {
    pub green: HashSet<String>,
    pub test_file_hashes: HashMap<String, String>,
    /// nodeid -> source def line, for restoring a cached entry's line (see
    /// [`Outcomes::test_lines`]).
    pub test_lines: HashMap<String, u64>,
    /// Unmeasured test-named files at record time (see [`ConfigState::test_named`]).
    pub test_named_hashes: HashMap<String, String>,
    /// Import-closure file -> hash at record time (see [`Outcomes::import_hashes`]).
    pub import_hashes: HashMap<String, String>,
    /// Test file -> its import closure (see [`Outcomes::test_imports`]).
    pub test_imports: HashMap<String, Vec<String>>,
    /// How each closure file was hashed (see [`Outcomes::import_keep`]).
    pub import_keep: HashMap<String, ModuleHash>,
}

/// How an import-closure file's module-level hash was taken (its value is in
/// `import_hashes`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModuleHash {
    /// The functions whose bodies the hash kept.
    pub keep: Vec<String>,
    /// The whole file's hash then: while it still matches, the module-level
    /// hash can't have moved, so a check reuses it without the helper.
    pub file: String,
    /// Digest of the shared lines it was taken from: a record reuses the
    /// entry while both this and `file` are unchanged.
    pub lines: String,
}

/// The skip-gating state computed at the start of an incremental run.
#[derive(Debug, Default, Clone)]
pub struct ConfigState {
    /// Config fingerprint: a change disables skipping wholesale.
    pub fp: String,
    /// Test-NAMED (`test_*.py` / `*_test.py`) first-party files coverage does
    /// not measure, relpath -> content id. Kept OUT of `fp` so a single test-file
    /// edit doesn't bust every test; [`skippable_now`] instead busts only when a
    /// changed one is NOT a recorded test module (i.e. it is a shared helper).
    pub test_named: HashMap<String, String>,
    /// The coverage-shaping args ([`crate::cov_scope::coverage_args`]) of the
    /// run that WROTE the coverage index skipping trusts: this run's when it
    /// requests coverage, else the ones recorded last time
    /// ([`stored_index_cov_args`]). `None` = unknown (e.g. an index from a
    /// remote cache), so which files it measured is unknown too and
    /// [`skippable_now`] skips nothing.
    pub index_cov_args: Option<Vec<String>>,
    /// The directory (relative to the project root, `/`-separated) the run
    /// that wrote the index started from: coverage resolves path sources
    /// such as `--cov=.` from there. `None` = unknown (an older record): the
    /// current run's cwd stands in.
    pub index_cov_cwd: Option<String>,
}

/// Hash the current content of the project's config files (order-stable), so a
/// change to any of them can bust the skip decision. `conftest.py` files are
/// folded in the same way: they carry fixtures/hooks/addopts that change test
/// behavior but frequently live OUTSIDE the coverage scope (e.g. `--cov=<pkg>`
/// with `tests/conftest.py`), so per-file coverage hashing can't see a conftest
/// edit. Folding every conftest under `scope` here means adding, editing, or
/// removing one busts skipping wholesale — the same guard the config files get.
/// When coverage measures only part of the tree ([`CovScope::is_partial`]),
/// every unmeasured non-test first-party `.py` is folded too (module note), and
/// the unmeasured test-named ones are returned in [`ConfigState::test_named`].
pub fn config_state(scope: &Path, cov: &CovScope) -> ConfigState {
    let mut h = Sha256::new();
    for name in CONFIG_FILES {
        if let Some(sha) = current_sha256(&scope.join(name)) {
            h.update(name.as_bytes());
            h.update(sha.as_bytes());
        }
    }
    // The walk decides WHICH files count (the same set with or without git);
    // git, when available, only makes hashing them cheap (see the module note).
    let walk_cov = cov.is_partial().then_some(cov);
    let mut walk = FingerprintInputs::default();
    collect_fingerprint_inputs(scope, scope, walk_cov, &mut walk);
    walk.conftests.sort();
    for path in &walk.conftests {
        if let Some(sha) = current_sha256(path) {
            let rel = path.strip_prefix(scope).unwrap_or(path);
            h.update(rel.to_string_lossy().as_bytes());
            h.update(sha.as_bytes());
        }
    }
    let ids = content_ids(scope, &walk.unmeasured, &walk.linked);
    // Unmeasured source folds (editing one busts skipping); unmeasured test-named
    // files are returned separately (see ConfigState::test_named).
    let mut test_named = HashMap::new();
    let mut folded: Vec<(String, String)> = Vec::new();
    for (rel, id) in ids {
        let name = Path::new(&rel)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if is_test_file(&name) {
            test_named.insert(rel, id);
        } else {
            folded.push((rel, id));
        }
    }
    folded.sort();
    for (rel, id) in &folded {
        h.update(b"py:");
        h.update(rel.as_bytes());
        h.update(id.as_bytes());
    }
    for (rel, id) in tracked_data_ids(scope) {
        h.update(b"data:");
        h.update(rel.as_bytes());
        h.update(id.as_bytes());
    }
    ConfigState {
        fp: crate::incremental::hex_encode(&h.finalize()),
        test_named,
        index_cov_args: None,
        index_cov_cwd: None,
    }
}

/// `(relpath, content id)` of every git-tracked non-Python file under `scope`,
/// sorted. Coverage never measures a data file (a JSON fixture, a template, a
/// golden output), so a test that reads one would stay cached after it changed;
/// folding them into the fingerprint makes such an edit bust skipping
/// wholesale. Tracked files only: untracked and ignored files are where test
/// and report outputs land, and folding those would bust every run. Cheap: a
/// clean tracked file reuses its index blob id ([`git_blob_ids`]). Empty outside
/// a git checkout (the documented gap) or on any git failure.
fn tracked_data_ids(scope: &Path) -> Vec<(String, String)> {
    let Some(out) = git_in(scope, &["ls-files", "-z"], None) else {
        return Vec::new();
    };
    let Some(recs) = nul_records(&out) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut linked: HashSet<PathBuf> = HashSet::new();
    for rel in recs {
        let p = Path::new(rel);
        if p.extension().is_some_and(|e| e == "py") || !is_data_input(p) {
            continue;
        }
        let abs = scope.join(p);
        // Deleted (drops out of the fold, which moves it), or a submodule dir.
        if !abs.is_file() {
            continue;
        }
        if std::fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink()) {
            linked.insert(abs.clone());
        }
        paths.push(abs);
    }
    let mut ids = content_ids(scope, &paths, &linked);
    ids.sort();
    ids
}

/// Whether a tracked non-Python path can be a test input: runner artifacts
/// (coverage data, caches) churn every run and are never read by a test.
fn is_data_input(rel: &Path) -> bool {
    !rel.components().any(|c| {
        matches!(
            c.as_os_str().to_str().unwrap_or(""),
            ".pytest_cache" | ".rstest_cache" | "__pycache__" | "htmlcov"
        )
    }) && !rel
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".coverage") || n == "coverage.xml" || n.ends_with(".pyc"))
}

/// Just the fingerprint of [`config_state`].
#[cfg(test)]
fn config_fingerprint(scope: &Path, cov: &CovScope) -> String {
    config_state(scope, cov).fp
}

/// Run `git -C <scope> <args>` (optionally feeding `stdin`), returning stdout on
/// success. `None` on any failure — not a repo, no git — so the caller falls
/// back to the tree walk. Stdin is written from its own thread while stdout
/// drains here: writing it all first would deadlock once both pipes fill.
fn git_in(scope: &Path, args: &[&str], stdin: Option<Vec<u8>>) -> Option<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .arg("-C")
        .arg(scope)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let writer = match stdin {
        Some(bytes) => {
            let mut pipe = child.stdin.take()?;
            Some(std::thread::spawn(move || pipe.write_all(&bytes).is_ok()))
        }
        None => None,
    };
    let out = child.wait_with_output().ok()?;
    let wrote = writer.is_none_or(|w| w.join().unwrap_or(false));
    (wrote && out.status.success()).then_some(out.stdout)
}

/// NUL-separated git output as UTF-8 records (a non-UTF-8 path fails the whole
/// git path: `None`, walk fallback).
fn nul_records(bytes: &[u8]) -> Option<Vec<&str>> {
    bytes
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| std::str::from_utf8(r).ok())
        .collect()
}

/// `(relpath, content id)` for each walked file. Git blob ids when git can
/// supply them all ([`git_blob_ids`]); otherwise SHA-256 of the content, with a
/// `sha256:` prefix so the two id kinds never collide. An unreadable file is
/// dropped (its removal from the fold still moves the fingerprint).
///
/// Paths in `linked` (a symlink, or reached through a symlinked directory) are
/// always SHA-256 hashed through the link: git's id for a tracked symlink is a
/// hash of the link TEXT, so editing its target would never move it, and
/// `hash-object` refuses paths beyond a symlink.
fn content_ids(
    scope: &Path,
    paths: &[PathBuf],
    linked: &HashSet<PathBuf>,
) -> Vec<(String, String)> {
    let rel_of = |p: &Path| {
        p.strip_prefix(scope)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let sha = |p: &PathBuf| Some((rel_of(p), format!("sha256:{}", current_sha256(p)?)));
    let (via_link, plain): (Vec<PathBuf>, Vec<PathBuf>) =
        paths.iter().cloned().partition(|p| linked.contains(p));
    let mut ids: Vec<(String, String)> = via_link.iter().filter_map(sha).collect();
    match (!plain.is_empty())
        .then(|| git_blob_ids(scope, &plain))
        .flatten()
    {
        Some(git) => ids.extend(plain.iter().map(|p| rel_of(p)).zip(git)),
        None => ids.extend(plain.iter().filter_map(sha)),
    }
    ids
}

/// Git blob ids for `paths` (absolute, under `scope`), in order. A tracked file
/// unmodified since the index was written reuses its index blob id (no read);
/// every other file — modified, untracked, ignored, in a submodule or nested
/// repo — goes through `git hash-object`, which yields the id it would have
/// once staged, so staging alone never moves the fingerprint. Absolute paths
/// throughout: `ls-files` reports paths relative to `-C`, but `hash-object
/// --stdin-paths` resolves them from the repo root. `None` (SHA-256 fallback)
/// outside a git checkout or on any git failure.
fn git_blob_ids(scope: &Path, paths: &[PathBuf]) -> Option<Vec<String>> {
    let staged = git_in(scope, &["ls-files", "-z", "-s"], None)?;
    let modified = git_in(scope, &["ls-files", "-z", "-m"], None)?;
    let modified: HashSet<&str> = nul_records(&modified)?.into_iter().collect();
    // `-s` records are `<mode> <blob> <stage>\t<path>`.
    let mut clean: HashMap<PathBuf, &str> = HashMap::new();
    for rec in nul_records(&staged)? {
        let (meta, rel) = rec.split_once('\t')?;
        if !modified.contains(rel) {
            clean.insert(scope.join(rel), meta.split(' ').nth(1)?);
        }
    }
    let to_hash: Vec<&Path> = paths
        .iter()
        .filter(|p| !clean.contains_key(*p))
        .map(PathBuf::as_path)
        .collect();
    let mut hashed: HashMap<&Path, String> = HashMap::new();
    if !to_hash.is_empty() {
        let lines: Vec<String> = to_hash
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        // `--stdin-paths` is newline-delimited: a path holding one can't be fed.
        if lines.iter().any(|l| l.contains('\n')) {
            return None;
        }
        let input = lines.join("\n") + "\n";
        let out = git_in(
            scope,
            &["hash-object", "--stdin-paths"],
            Some(input.into_bytes()),
        )?;
        let out = String::from_utf8(out).ok()?;
        let ids: Vec<&str> = out.lines().collect();
        if ids.len() != to_hash.len() {
            return None;
        }
        hashed = to_hash
            .into_iter()
            .zip(ids.into_iter().map(String::from))
            .collect();
    }
    paths
        .iter()
        .map(|p| match clean.get(p) {
            Some(blob) => Some((*blob).to_string()),
            None => hashed.get(p.as_path()).cloned(),
        })
        .collect()
}

/// The two path sets [`config_state`] gathers in one tree walk.
#[derive(Default)]
struct FingerprintInputs {
    /// Every `conftest.py` under `scope` (folded regardless of coverage).
    conftests: Vec<PathBuf>,
    /// Unmeasured first-party `.py` (test-named included; the caller splits
    /// them); empty unless the walk was given a partial [`CovScope`].
    unmeasured: Vec<PathBuf>,
    /// The subset of `unmeasured` that is a symlink or lies under a symlinked
    /// directory (hashed through the link: see [`content_ids`]).
    linked: HashSet<PathBuf>,
    /// Canonical targets of the symlinked directories already walked (loop
    /// and duplicate guard).
    visited_links: HashSet<PathBuf>,
}

/// Whether `name` is a test file by pytest/unittest convention (`test_*.py` /
/// `*_test.py`).
fn is_test_file(name: &str) -> bool {
    name.starts_with("test_") || name.ends_with("_test.py")
}

/// One recursive pass over `scope` populating [`FingerprintInputs`]: collects
/// every `conftest.py`, and — when `cov` is `Some` (partial coverage) — every
/// unmeasured first-party `.py`. Prunes VCS / cache / virtualenv directories:
/// dot-dirs, [`PRUNE_DIRS`], and any directory holding a `pyvenv.cfg` (a venv of
/// ANY name — its site-packages is third-party, not first-party). Descends into
/// measured dirs too, since they may hold conftests; their `.py` are filtered
/// out per-file. Follows symlinked directories (a first-party package linked in
/// from elsewhere in a monorepo is still importable), each target once, and
/// never into its own ancestor.
fn collect_fingerprint_inputs(
    scope: &Path,
    dir: &Path,
    cov: Option<&CovScope>,
    out: &mut FingerprintInputs,
) {
    walk_dir(scope, dir, cov, false, out);
}

/// [`collect_fingerprint_inputs`]'s recursion; `via_link` = `dir` was reached
/// through a symlinked directory.
fn walk_dir(
    scope: &Path,
    dir: &Path,
    cov: Option<&CovScope>,
    via_link: bool,
    out: &mut FingerprintInputs,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        // Resolve a symlink to what it points at (a dangling one is skipped).
        let (is_dir, is_link) = if ft.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(md) => (md.is_dir(), true),
                Err(_) => continue,
            }
        } else {
            (ft.is_dir(), false)
        };
        if is_dir {
            if name.starts_with('.') || PRUNE_DIRS.contains(&name.as_ref()) {
                continue;
            }
            // A virtualenv of any name (PEP 405 marker) is not first-party.
            if path.join("pyvenv.cfg").is_file() {
                continue;
            }
            if is_link {
                let Ok(target) = path.canonicalize() else {
                    continue;
                };
                let here = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
                if here.starts_with(&target) || !out.visited_links.insert(target) {
                    continue;
                }
            }
            walk_dir(scope, &path, cov, via_link || is_link, out);
        } else if name == "conftest.py" {
            out.conftests.push(path);
        } else if let Some(cov) = cov.filter(|_| name.ends_with(".py")) {
            let rel = path.strip_prefix(scope).unwrap_or(&path);
            if !cov.measures(rel) {
                if via_link || is_link {
                    out.linked.insert(path.clone());
                }
                out.unmeasured.push(path);
            }
        }
    }
}

/// Whether the pytest args request coverage this run (so covtool will refresh
/// the coverage index). `--incremental` relies on the index advancing each run;
/// without `--cov` the index is never rewritten, so a test whose dependency
/// changed re-runs on every invocation until a coverage run refreshes it.
pub fn coverage_requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "--cov" || a.starts_with("--cov="))
}

/// The recorded baseline, but ONLY if the config fingerprint still matches — a
/// config change disables skipping (returns empty). Absent / corrupt / schema-
/// mismatched store also yields empty (nothing skippable).
pub fn load(scope: &Path, config_fp: &str) -> Baseline {
    std::fs::read(cache::file_in(scope, FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Outcomes>(&b).ok())
        .filter(|o| o.schema == SCHEMA && o.config_fp == config_fp)
        .map(|o| Baseline {
            green: o.green,
            test_file_hashes: o.test_file_hashes,
            test_lines: o.test_lines,
            test_named_hashes: o.test_named_hashes,
            import_hashes: o.import_hashes,
            test_imports: o.test_imports,
            import_keep: o.import_keep,
        })
        .unwrap_or_default()
}

/// The coverage-shaping args recorded with the last baseline (see
/// [`ConfigState::index_cov_args`]), ungated by the config fingerprint: a run
/// without `--cov` needs them to know what the index it reuses measured.
#[cfg(test)]
pub fn stored_index_cov_args(scope: &Path) -> Option<Vec<String>> {
    stored_index_cov(scope).map(|(args, _)| args)
}

/// [`stored_index_cov_args`] plus the directory that run started from (see
/// [`ConfigState::index_cov_cwd`]).
pub fn stored_index_cov(scope: &Path) -> Option<(Vec<String>, Option<String>)> {
    let o = read_outcomes(scope)?;
    Some((o.index_cov_args?, o.index_cov_cwd))
}

/// The stored record, when present and of the current schema.
fn read_outcomes(scope: &Path) -> Option<Outcomes> {
    std::fs::read(cache::file_in(scope, FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Outcomes>(&b).ok())
        .filter(|o| o.schema == SCHEMA)
}

/// The last run's green set and per-nodeid def line, ungated by the config
/// fingerprint. `load` returns empty when the config changed (skipping is off);
/// `explain` reports *history* rather than skip-eligibility, so it wants the raw
/// record regardless. Empty on absent / corrupt / schema-mismatched store.
pub fn load_raw(scope: &Path) -> (HashSet<String>, HashMap<String, u64>) {
    std::fs::read(cache::file_in(scope, FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Outcomes>(&b).ok())
        .filter(|o| o.schema == SCHEMA)
        .map(|o| (o.green, o.test_lines))
        .unwrap_or_default()
}

/// Persist the green set + per-test-file hashes + per-nodeid def lines + config
/// fingerprint after a run. Best-effort: a cache-write failure never fails the
/// run. `lines` maps green nodeids to their source def line (restores a cached
/// entry's line next run); nodeids without a known line are simply absent.
///
/// `ran` holds the nodeids this run executed. A green test from the previous
/// record that this run did not execute (a subset run: a path, `-k`, a run
/// from a subdirectory) stays recorded, with the hash its test file was
/// recorded under, provided the config fingerprint is unchanged and that file
/// was not seen with different content this run. The store is shared by every
/// run in the rootdir, so a subset run must not erase the rest of the suite.
///
/// `python` runs the module-level hashing helper; `None` hashes every
/// import-closure file whole.
pub fn record(
    scope: &Path,
    config: &ConfigState,
    mut green: HashSet<String>,
    mut lines: HashMap<String, u64>,
    ran: &HashSet<String>,
    python: Option<&Path>,
) {
    let mut test_file_hashes = test_file_hashes(&green);
    let prev_store = read_outcomes(scope);
    if let Some(prev) = prev_store.as_ref().filter(|o| o.config_fp == config.fp) {
        let prev_deps = prev.test_imports.values().flatten().map(String::as_str);
        let prev_now = closure_hashes(python, prev_deps, &prev.import_keep, &prev.import_hashes);
        for id in prev.green.iter().cloned() {
            if ran.contains(&id) || green.contains(&id) {
                continue;
            }
            let tf = test_file_of(&id);
            let Some(old) = prev.test_file_hashes.get(tf) else {
                continue;
            };
            if !cache::resolve(tf).exists() {
                continue; // the test file is gone
            }
            match test_file_hashes.get(tf) {
                Some(now) if now != old => continue, // its file changed
                Some(_) => {}
                None => {
                    test_file_hashes.insert(tf.to_string(), old.clone());
                }
            }
            // Its import closure must be unchanged too: the closure is
            // re-hashed below, so carrying a test whose import changed while
            // it didn't run would vouch for code it never ran against.
            let closure_unchanged = prev.test_imports.get(tf).is_some_and(|deps| {
                deps.iter().all(|d| {
                    let now = prev_now.get(d.as_str()).cloned().flatten();
                    now.is_some() && now.as_ref() == prev.import_hashes.get(d)
                })
            });
            if !closure_unchanged {
                continue;
            }
            if let Some(l) = prev.test_lines.get(&id) {
                lines.entry(id.clone()).or_insert(*l);
            }
            green.insert(id);
        }
    }
    let shared = crate::select::load_coverage_index()
        .map(|i| i.shared_lines)
        .unwrap_or_default();
    let Closures {
        hashes: import_hashes,
        imports: test_imports,
        keep: import_keep,
    } = test_import_closures(
        test_file_hashes.keys(),
        python,
        &shared,
        prev_store.as_ref(),
    );
    let doc = Outcomes {
        schema: SCHEMA,
        config_fp: config.fp.clone(),
        green,
        test_file_hashes,
        test_lines: lines,
        test_named_hashes: config.test_named.clone(),
        import_hashes,
        test_imports,
        import_keep,
        index_cov_args: config.index_cov_args.clone(),
        index_cov_cwd: config.index_cov_cwd.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&doc) {
        let _ = cache::write_atomic(&cache::file_in(scope, FILE), &bytes);
    }
}

/// Each test file's import closure ([`crate::select::import_closures`] over
/// the rootdir), keyed and listed as rootdir-relative paths like the test file
/// hashes, plus one content hash per distinct file. This is what catches
/// import-time code: a module-level constant runs while the test module is
/// imported, under coverage's empty context, so no test owns those lines in
/// the index and editing them moves no hash the index ties to a test. A
/// closure file that can't be hashed gets no entry, so its importers are not
/// skippable next run.
///
/// A file coverage measured (it has `shared` lines) is hashed at module level
/// through the `modhash` helper, keeping the bodies that ran outside a test's
/// call phase; the third map names them. Others are hashed whole.
fn test_import_closures<'a>(
    test_files: impl Iterator<Item = &'a String>,
    python: Option<&Path>,
    shared: &HashMap<String, Vec<u32>>,
    prev: Option<&Outcomes>,
) -> Closures {
    let by_path: HashMap<PathBuf, String> = test_files
        .map(|tf| (cache::resolve(tf), tf.clone()))
        .collect();
    if by_path.is_empty() {
        return Closures::default();
    }
    let root = cache::base_dir();
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
    let rel = |p: &Path| {
        p.strip_prefix(&root_canon)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let roots: Vec<PathBuf> = by_path.keys().cloned().collect();
    let mut closures: HashMap<String, Vec<String>> = HashMap::new();
    for (abs, deps) in crate::select::import_closures(&root, &roots) {
        if let Some(tf) = by_path.get(&abs) {
            closures.insert(tf.clone(), deps.iter().map(|d| rel(d)).collect());
        }
    }
    let all: HashSet<&String> = closures.values().flatten().collect();
    let mut hashes: HashMap<String, String> = HashMap::new();
    let mut keep: HashMap<String, ModuleHash> = HashMap::new();
    // Measured files: reuse the previous record's module-level hash while the
    // file and its shared lines are unchanged; ask the helper for the rest.
    let mut asks: HashMap<String, Hashing> = HashMap::new();
    let mut fresh: HashMap<String, (String, String)> = HashMap::new();
    for d in &all {
        let (Some(lines), Some(file)) = (shared.get(*d), current_sha256(&cache::resolve(d))) else {
            continue;
        };
        let digest = lines_digest(lines);
        let reused = prev.and_then(|p| reusable(p, d, &file, &digest));
        match reused {
            Some((h, m)) => {
                hashes.insert((*d).clone(), h);
                keep.insert((*d).clone(), m);
            }
            None => {
                asks.insert((*d).clone(), Hashing::Lines(lines.clone()));
                fresh.insert((*d).clone(), (file, digest));
            }
        }
    }
    let module = python.map_or_else(HashMap::new, |py| module_hashes(py, &asks));
    for (d, m) in module {
        if let Some((file, lines)) = fresh.remove(&d) {
            hashes.insert(d.clone(), m.hash);
            keep.insert(
                d,
                ModuleHash {
                    keep: m.keep,
                    file,
                    lines,
                },
            );
        }
    }
    for d in all {
        if !hashes.contains_key(d) {
            if let Some(h) = current_sha256(&cache::resolve(d)) {
                hashes.insert(d.clone(), h);
            }
        }
    }
    Closures {
        hashes,
        imports: closures,
        keep,
    }
}

/// What [`test_import_closures`] records (the `import_*` / `test_imports`
/// fields of [`Outcomes`]).
#[derive(Default)]
struct Closures {
    /// Closure file -> its hash (module-level, or whole when not in `keep`).
    hashes: HashMap<String, String>,
    /// Test file -> its import closure.
    imports: HashMap<String, Vec<String>>,
    /// Closure file hashed at module level -> how (see [`ModuleHash`]).
    keep: HashMap<String, ModuleHash>,
}

/// The previous record's module-level hash of `file` and how it was taken,
/// when the file (`whole`, its whole-file hash) and its shared lines
/// (`digest`) are both what they were then.
fn reusable(
    prev: &Outcomes,
    file: &str,
    whole: &str,
    digest: &str,
) -> Option<(String, ModuleHash)> {
    let m = prev.import_keep.get(file)?;
    (m.file == whole && m.lines == digest).then_some(())?;
    Some((prev.import_hashes.get(file)?.clone(), m.clone()))
}

/// A short, order-stable digest of a file's shared lines.
fn lines_digest(lines: &[u32]) -> String {
    let mut sorted = lines.to_vec();
    sorted.sort_unstable();
    let joined: Vec<String> = sorted.iter().map(u32::to_string).collect();
    crate::incremental::hex_encode(&Sha256::digest(joined.join(",").as_bytes()))
}

/// What the `modhash` helper is asked for one file.
enum Hashing {
    /// Recording: keep the bodies of functions holding these lines.
    Lines(Vec<u32>),
    /// Checking: keep the bodies of these recorded functions.
    Keep(Vec<String>),
}

/// Module-level hashes of `files` (rootdir-relative) from the `modhash` helper
/// run under `python`: file -> its [`ModuleKey`] (the hash, the functions whose
/// bodies it kept, and every other body's span and hash). A file
/// the helper could not hash is absent; any failure of the helper itself
/// leaves the whole map empty.
fn module_hashes(python: &Path, files: &HashMap<String, Hashing>) -> HashMap<String, ModuleKey> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if files.is_empty() {
        return HashMap::new();
    }
    let files: serde_json::Map<String, serde_json::Value> = files
        .iter()
        .map(|(f, h)| {
            let spec = match h {
                Hashing::Lines(l) => serde_json::json!({ "lines": l }),
                Hashing::Keep(k) => serde_json::json!({ "keep": k }),
            };
            (f.clone(), spec)
        })
        .collect();
    let request = serde_json::json!({ "root": cache::base_dir(), "files": files });
    let mut cmd = Command::new(python);
    cmd.args(["-m", "rstest_worker.modhash"])
        .env("PYTHONPATH", crate::scheduling::worker::worker_pythonpath())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::scheduling::worker::scrub_secrets(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        return HashMap::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(request.to_string().as_bytes());
    }
    let Ok(out) = child.wait_with_output() else {
        return HashMap::new();
    };
    serde_json::from_slice::<HashMap<String, ModuleKey>>(&out.stdout).unwrap_or_default()
}

/// The current hash of each closure file in `deps`, taken the way it was
/// recorded: at module level for a file in `keep`, whole otherwise. A file
/// whose whole-file hash still matches the record keeps its recorded
/// module-level hash (`recorded`); only changed ones go to the helper, with
/// the recorded kept bodies. `None`: it can't be hashed now (deleted,
/// unparseable, or no helper), so its importers must run.
fn closure_hashes<'a>(
    python: Option<&Path>,
    deps: impl Iterator<Item = &'a str>,
    keep: &HashMap<String, ModuleHash>,
    recorded: &HashMap<String, String>,
) -> HashMap<&'a str, Option<String>> {
    let deps: HashSet<&str> = deps.collect();
    let mut now: HashMap<&'a str, Option<String>> = HashMap::new();
    let mut asks: HashMap<String, Hashing> = HashMap::new();
    for d in deps {
        let whole = current_sha256(&cache::resolve(d));
        match keep.get(d) {
            None => {
                now.insert(d, whole);
            }
            Some(m) if whole.as_deref() == Some(m.file.as_str()) => {
                now.insert(d, recorded.get(d).cloned());
            }
            Some(_) if whole.is_none() => {
                now.insert(d, None);
            }
            Some(m) => {
                asks.insert(d.to_string(), Hashing::Keep(m.keep.clone()));
                now.insert(d, None);
            }
        }
    }
    let module = python.map_or_else(HashMap::new, |py| module_hashes(py, &asks));
    for (d, slot) in now.iter_mut() {
        if let Some(m) = module.get(*d) {
            *slot = Some(m.hash.clone());
        }
    }
    now
}

/// Hash the source file of every green nodeid's test file, once per distinct
/// file. The nodeid's path is relative to the rootdir (as pytest makes it), so
/// it resolves there, not against the cwd. Unreadable files are simply omitted
/// (a test whose file can't be hashed won't be skippable next run).
fn test_file_hashes(green: &HashSet<String>) -> HashMap<String, String> {
    let mut files: HashMap<String, String> = HashMap::new();
    for id in green {
        let tf = test_file_of(id);
        if !files.contains_key(tf) {
            if let Some(h) = current_sha256(&cache::resolve(tf)) {
                files.insert(tf.to_string(), h);
            }
        }
    }
    files
}

/// The test-file portion of a nodeid (`path::Class::test` -> `path`).
fn test_file_of(nodeid: &str) -> &str {
    crate::text::nodeid_file(nodeid)
}

/// Fold the coverage of cached (skipped) tests from the pre-run index (`old`)
/// back into the freshly-written one (`new`). A skipped test produces no
/// coverage, so covtool rewrites the index without it; without this, a
/// cached test would drop out of the index and be forced to re-run next time
/// ("skip once" thrashing). Cached tests' files are unchanged (that is *why*
/// they were skipped), so `old`'s hashes stay valid.
pub fn carry_forward(old: &CoverageIndex, new: &mut CoverageIndex, cached: &HashSet<String>) {
    if new.schema == 0 {
        new.schema = old.schema;
    }
    for (file, ofile) in &old.files {
        for (line, ids) in &ofile.lines {
            for id in ids {
                if !cached.contains(id) {
                    continue;
                }
                let nf = new
                    .files
                    .entry(file.clone())
                    .or_insert_with(|| CoverageFile {
                        hash: ofile.hash.clone(),
                        lines: HashMap::new(),
                        module: ofile.module.clone(),
                    });
                let slot = nf.lines.entry(*line).or_default();
                if !slot.contains(id) {
                    slot.push(id.clone());
                }
            }
        }
    }
}

/// Fold back into the freshly-written index (`new`) the coverage of every test
/// in the pre-run index (`old`) that this run did not execute (`ran`): cached
/// tests, and the rest of the suite when this was a subset run (a path, `-k`, a
/// run from a subdirectory). covtool rewrites the index from only the tests
/// that ran, and the index is shared by every run in the rootdir. A test is
/// carried only when every file it covered is either unmeasured this run or
/// measured with the content the old index recorded, or its lines there can be
/// moved onto the new content: the module level unchanged and each line inside
/// a function whose body text is identical (comments and blank lines too, so
/// every line kept its offset), shifted as far as that function moved. Otherwise its old line
/// map is stale, so it is dropped (and so re-runs) instead.
pub fn carry_forward_unrun(old: &CoverageIndex, new: &mut CoverageIndex, ran: &HashSet<String>) {
    let mut stale: HashSet<&str> = HashSet::new();
    // Changed file -> nodeid -> its lines moved onto the new content.
    let mut moved: HashMap<&str, HashMap<&str, Vec<u32>>> = HashMap::new();
    for (file, ofile) in &old.files {
        let Some(nfile) = new.files.get(file).filter(|nf| nf.hash != ofile.hash) else {
            continue;
        };
        // A moved test counts as run on the new content, so the module level
        // must be as it was too.
        let shift = match (&ofile.module, &nfile.module) {
            (Some(o), Some(n)) if o.hash == n.hash => Some((def_lookup(o), o, n)),
            _ => None,
        };
        let move_line = |line: u32| {
            let (lookup, o, n) = shift.as_ref()?;
            let (q, first, _) = lookup(line)?;
            let (nfirst, _, _) = n.defs.get(q)?;
            let same = o.layout.get(q).is_some_and(|l| n.layout.get(q) == Some(l));
            same.then(|| line - first + nfirst)
        };
        let per = moved.entry(file.as_str()).or_default();
        for (line, ids) in &ofile.lines {
            let to = move_line(*line);
            for id in ids {
                match to {
                    Some(to) => per.entry(id.as_str()).or_default().push(to),
                    None => {
                        stale.insert(id.as_str());
                    }
                }
            }
        }
    }
    let keep: HashSet<String> = old
        .files
        .values()
        .flat_map(|f| f.lines.values().flatten())
        .filter(|id| !ran.contains(*id) && !stale.contains(id.as_str()))
        .cloned()
        .collect();
    let unchanged = CoverageIndex {
        schema: old.schema,
        shared_lines: HashMap::new(),
        files: old
            .files
            .iter()
            .filter(|(f, _)| !moved.contains_key(f.as_str()))
            .map(|(f, c)| (f.clone(), c.clone()))
            .collect(),
    };
    carry_forward(&unchanged, new, &keep);
    for (file, per) in moved {
        let nfile = new
            .files
            .get_mut(file)
            .expect("moved files are in the new index");
        for (id, lines) in per {
            if !keep.contains(id) {
                continue;
            }
            for line in lines {
                let slot = nfile.lines.entry(line).or_default();
                if !slot.iter().any(|x| x == id) {
                    slot.push(id.to_string());
                }
            }
        }
    }
}

/// Write the coverage index back to the local cache (same path covtool uses),
/// after [`carry_forward`]. Best-effort.
pub fn write_index(index: &CoverageIndex) {
    if let Ok(bytes) = serde_json::to_vec(index) {
        let _ = cache::write_atomic(&cache::file(COVERAGE_INDEX_FILE), &bytes);
    }
}

/// The function body holding `line`, as (qualified name, first body line, body
/// hash); `None` for a module-level line.
fn def_lookup<'a>(key: &'a ModuleKey) -> impl Fn(u32) -> Option<(&'a str, u32, &'a str)> {
    let mut spans: Vec<(u32, u32, &str, &str)> = key
        .defs
        .iter()
        .map(|(q, (first, last, hash))| (*first, *last, q.as_str(), hash.as_str()))
        .collect();
    spans.sort_unstable();
    move |line| {
        let i = spans.partition_point(|s| s.0 <= line);
        i.checked_sub(1)
            .map(|i| spans[i])
            .filter(|s| line <= s.1)
            .map(|s| (s.2, s.0, s.3))
    }
}

/// For a covered file with a per-function key: nodeid -> the functions whose
/// bodies hold lines it covered. Covered lines outside every listed body are
/// module-level (guarded by the module-level hash). `None` without a key.
fn covered_defs(file: &CoverageFile) -> Option<HashMap<&str, HashSet<&str>>> {
    let def_at = def_lookup(file.module.as_ref()?);
    let mut out: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (line, ids) in &file.lines {
        if let Some((q, _, _)) = def_at(*line) {
            for id in ids {
                out.entry(id.as_str()).or_default().insert(q);
            }
        }
    }
    Some(out)
}

/// Invert the coverage index: nodeid -> the set of files it covered.
fn covered_files(index: &CoverageIndex) -> HashMap<&str, HashSet<&str>> {
    let mut map: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (file, cov) in &index.files {
        for ids in cov.lines.values() {
            for id in ids {
                map.entry(id.as_str()).or_default().insert(file.as_str());
            }
        }
    }
    map
}

/// The nodeids provably skippable this run: green last time, its own test file
/// unchanged since then, present in the index (covered ≥1 file), and every
/// covered file's CURRENT hash equal to the hash the index recorded, and every
/// file its test file transitively imports ([`Baseline::test_imports`])
/// unchanged since the record. `hash_of(relpath)` returns the live hash
/// (`None` = unreadable/deleted → not skippable). Pure over its inputs, for
/// testing; [`skippable_now`] wires it to the on-disk index + working tree.
///
/// `hash_of` hashes a test or covered file whole; `dep_hash_of` hashes an
/// import-closure file the way it was recorded (see [`closure_hashes`]).
#[cfg(test)]
pub fn skippable(
    index: &CoverageIndex,
    baseline: &Baseline,
    hash_of: impl Fn(&str) -> Option<String>,
    dep_hash_of: impl Fn(&str) -> Option<String>,
) -> HashSet<String> {
    skippable_keyed(index, baseline, hash_of, dep_hash_of, &HashMap::new())
}

/// [`skippable`], with per-function keys: `changed` holds the current
/// [`ModuleKey`] of covered files whose content changed since the index. A
/// test still skips such a file when its module-level hash and the bodies of
/// every function the test covered there are unchanged (only other
/// functions were edited).
pub fn skippable_keyed(
    index: &CoverageIndex,
    baseline: &Baseline,
    hash_of: impl Fn(&str) -> Option<String>,
    dep_hash_of: impl Fn(&str) -> Option<String>,
    changed: &HashMap<String, ModuleKey>,
) -> HashSet<String> {
    if baseline.green.is_empty() {
        return HashSet::new();
    }
    let by_test = covered_files(index);
    // Hash every file we might consult exactly once (many tests share files):
    // each green test's own file plus the files it covered.
    let mut needed: HashSet<&str> = HashSet::new();
    let mut needed_deps: HashSet<&str> = HashSet::new();
    for (id, files) in &by_test {
        if baseline.green.contains(*id) {
            let tf = test_file_of(id);
            needed.insert(tf);
            needed.extend(files.iter().copied());
            if let Some(deps) = baseline.test_imports.get(tf) {
                needed_deps.extend(deps.iter().map(String::as_str));
            }
        }
    }
    let cur: HashMap<&str, Option<String>> = needed.into_iter().map(|f| (f, hash_of(f))).collect();
    let live = |f: &str| cur.get(f).and_then(|o| o.as_deref());
    let cur_deps: HashMap<&str, Option<String>> = needed_deps
        .into_iter()
        .map(|f| (f, dep_hash_of(f)))
        .collect();
    let live_dep = |f: &str| cur_deps.get(f).and_then(|o| o.as_deref());
    // For each changed file with a key: test -> the functions it covered there.
    let covered: HashMap<&str, HashMap<&str, HashSet<&str>>> = changed
        .keys()
        .filter_map(|f| {
            let (k, cf) = index.files.get_key_value(f.as_str())?;
            Some((k.as_str(), covered_defs(cf)?))
        })
        .collect();
    let same_functions = |f: &str, id: &str| -> bool {
        let (Some(rec), Some(now), Some(by_test)) = (
            index.files.get(f).and_then(|c| c.module.as_ref()),
            changed.get(f),
            covered.get(f),
        ) else {
            return false;
        };
        let body = |m: &ModuleKey, q: &str| m.defs.get(q).map(|d| d.2.clone());
        rec.hash == now.hash
            && by_test.get(id).is_none_or(|qs| {
                qs.iter()
                    .all(|q| body(rec, q).is_some() && body(rec, q) == body(now, q))
            })
    };

    let mut skip = HashSet::new();
    'test: for (id, files) in &by_test {
        if !baseline.green.contains(*id) {
            continue;
        }
        // The test's OWN file must be tracked and unchanged (the index may not
        // measure test files under `--cov=<pkg>`). When coverage measured it,
        // a change elsewhere in the file is fine: its module-level code
        // (imports, fixtures, decorators) and the functions this test ran
        // there must be as they were, as for any covered file.
        let tf = test_file_of(id);
        let recorded = baseline.test_file_hashes.get(tf).map(String::as_str);
        if recorded.is_none() || live(tf).is_none() {
            continue;
        }
        if live(tf) != recorded && !same_functions(tf, id) {
            continue;
        }
        // Every file the test file transitively imports must be unchanged too:
        // import-time lines (module constants) belong to no test in the index.
        // No recorded closure -> nothing vouches for them -> must run.
        let Some(deps) = baseline.test_imports.get(tf) else {
            continue;
        };
        for d in deps {
            let stored = baseline.import_hashes.get(d).map(String::as_str);
            if live_dep(d).is_none() || live_dep(d) != stored {
                continue 'test;
            }
        }
        for f in files {
            let stored = index.files.get(*f).map(|c| c.hash.as_str());
            match (live(f), stored) {
                (Some(l), Some(s)) if l == s => {}
                // Changed, but only in functions this test never ran.
                (Some(_), Some(_)) if same_functions(f, id) => {}
                // Changed, deleted, or index missing the hash → must run it.
                _ => continue 'test,
            }
        }
        skip.insert((*id).to_string());
    }
    skip
}

/// [`skippable`] wired to the live working tree: hashes each file's current
/// content (`rel` is rootdir-relative, like the index keys and nodeids). First, two
/// wholesale guards: an index of unknown coverage scope
/// ([`ConfigState::index_cov_args`]) and a changed shared helper
/// ([`helper_changed`]).
pub fn skippable_now(
    index: &CoverageIndex,
    baseline: &Baseline,
    config: &ConfigState,
    python: Option<&Path>,
) -> HashSet<String> {
    if config.index_cov_args.is_none() || helper_changed(baseline, &config.test_named) {
        return HashSet::new();
    }
    let deps = baseline.test_imports.values().flatten().map(String::as_str);
    let now = closure_hashes(python, deps, &baseline.import_keep, &baseline.import_hashes);
    // Covered files with a per-function key that changed since the index: ask
    // the helper for their current key, with the recorded kept bodies.
    let whole: HashMap<&str, Option<String>> = index
        .files
        .iter()
        .filter(|(_, c)| c.module.is_some())
        .map(|(f, _)| (f.as_str(), current_sha256(&cache::resolve(f))))
        .collect();
    let asks: HashMap<String, Hashing> = index
        .files
        .iter()
        .filter_map(|(f, c)| {
            let m = c.module.as_ref()?;
            let now = whole.get(f.as_str())?.as_deref()?;
            (now != c.hash).then(|| (f.clone(), Hashing::Keep(m.keep.clone())))
        })
        .collect();
    let changed = python.map_or_else(HashMap::new, |py| module_hashes(py, &asks));
    skippable_keyed(
        index,
        baseline,
        |rel| match whole.get(rel) {
            Some(h) => h.clone(),
            None => current_sha256(&cache::resolve(rel)),
        },
        |rel| now.get(rel).cloned().flatten(),
        &changed,
    )
}

/// Whether an unmeasured test-NAMED file recorded last run has changed (edited
/// or deleted) and is NOT a recorded test module — no green test lives in it,
/// so no per-test hash guards it: it is a shared helper (`test_utils.py`, a
/// `TestMixin` base), and any test may import it. Such an edit busts skipping
/// wholesale. A changed test module is left to the per-test own-file guard. A
/// file new since the record needs no check: any test importing it had to be
/// edited to do so, and that edit re-runs the importer.
fn helper_changed(baseline: &Baseline, current: &HashMap<String, String>) -> bool {
    baseline.test_named_hashes.iter().any(|(rel, old)| {
        current.get(rel) != Some(old) && !baseline.test_file_hashes.contains_key(rel)
    })
}

#[cfg(test)]
mod tests;
