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
use crate::select::{current_sha256, CoverageFile, CoverageIndex, COVERAGE_INDEX_FILE};

/// Filename of the per-test outcome store within the cache dir.
pub const FILE: &str = "incremental_outcomes.json";

// Schema 2 added `test_lines` (nodeid -> def line) for restoring cached entries'
// source line; an older schema-1 store reads as absent (one full run to rebuild).
// Schema 3 added `test_named_hashes`: a schema-2 store never guarded shared
// test-named helpers, so it must not seed a skip.
const SCHEMA: u32 = 3;

/// Config files whose change invalidates the whole skip decision: markers,
/// addopts, and coverage config aren't reflected in per-test coverage, so a
/// change to any of them disables skipping for that run.
const CONFIG_FILES: [&str; 5] = [
    "pyproject.toml",
    "pytest.ini",
    "setup.cfg",
    "tox.ini",
    ".coveragerc",
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
    /// [`ConfigState::index_cov_args`] of the recorded run.
    #[serde(default)]
    index_cov_args: Option<Vec<String>>,
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
    ConfigState {
        fp: crate::incremental::hex_encode(&h.finalize()),
        test_named,
        index_cov_args: None,
    }
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
        })
        .unwrap_or_default()
}

/// The coverage-shaping args recorded with the last baseline (see
/// [`ConfigState::index_cov_args`]), ungated by the config fingerprint: a run
/// without `--cov` needs them to know what the index it reuses measured.
pub fn stored_index_cov_args(scope: &Path) -> Option<Vec<String>> {
    std::fs::read(cache::file_in(scope, FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Outcomes>(&b).ok())
        .filter(|o| o.schema == SCHEMA)
        .and_then(|o| o.index_cov_args)
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
pub fn record(
    scope: &Path,
    config: &ConfigState,
    green: HashSet<String>,
    lines: HashMap<String, u64>,
) {
    let test_file_hashes = test_file_hashes(&green);
    let doc = Outcomes {
        schema: SCHEMA,
        config_fp: config.fp.clone(),
        green,
        test_file_hashes,
        test_lines: lines,
        test_named_hashes: config.test_named.clone(),
        index_cov_args: config.index_cov_args.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&doc) {
        let _ = cache::write_atomic(&cache::file_in(scope, FILE), &bytes);
    }
}

/// Hash the (cwd-relative) source file of every green nodeid's test file, once
/// per distinct file. Unreadable files are simply omitted (a test whose file
/// can't be hashed won't be skippable next run).
fn test_file_hashes(green: &HashSet<String>) -> HashMap<String, String> {
    let mut files: HashMap<String, String> = HashMap::new();
    for id in green {
        let tf = test_file_of(id);
        if !files.contains_key(tf) {
            if let Some(h) = current_sha256(Path::new(tf)) {
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
                    });
                let slot = nf.lines.entry(*line).or_default();
                if !slot.contains(id) {
                    slot.push(id.clone());
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
/// covered file's CURRENT hash equal to the hash the index recorded.
/// `hash_of(relpath)` returns the live hash (`None` = unreadable/deleted → not
/// skippable). Pure over its inputs, for testing; [`skippable_now`] wires it to
/// the on-disk index + working tree.
pub fn skippable(
    index: &CoverageIndex,
    baseline: &Baseline,
    hash_of: impl Fn(&str) -> Option<String>,
) -> HashSet<String> {
    if baseline.green.is_empty() {
        return HashSet::new();
    }
    let by_test = covered_files(index);
    // Hash every file we might consult exactly once (many tests share files):
    // each green test's own file plus the files it covered.
    let mut needed: HashSet<&str> = HashSet::new();
    for (id, files) in &by_test {
        if baseline.green.contains(*id) {
            needed.insert(test_file_of(id));
            needed.extend(files.iter().copied());
        }
    }
    let cur: HashMap<&str, Option<String>> = needed.into_iter().map(|f| (f, hash_of(f))).collect();
    let live = |f: &str| cur.get(f).and_then(|o| o.as_deref());

    let mut skip = HashSet::new();
    'test: for (id, files) in &by_test {
        if !baseline.green.contains(*id) {
            continue;
        }
        // The test's OWN file must be tracked and unchanged (the index may not
        // measure test files under `--cov=<pkg>`).
        let tf = test_file_of(id);
        if live(tf) != baseline.test_file_hashes.get(tf).map(String::as_str) || live(tf).is_none() {
            continue;
        }
        for f in files {
            let stored = index.files.get(*f).map(|c| c.hash.as_str());
            match (live(f), stored) {
                (Some(l), Some(s)) if l == s => {}
                // Changed, deleted, or index missing the hash → must run it.
                _ => continue 'test,
            }
        }
        skip.insert((*id).to_string());
    }
    skip
}

/// [`skippable`] wired to the live working tree: hashes each file's current
/// content (`rel` is cwd-relative, matching the index keys). First, two
/// wholesale guards: an index of unknown coverage scope
/// ([`ConfigState::index_cov_args`]) and a changed shared helper
/// ([`helper_changed`]).
pub fn skippable_now(
    index: &CoverageIndex,
    baseline: &Baseline,
    config: &ConfigState,
) -> HashSet<String> {
    if config.index_cov_args.is_none() || helper_changed(baseline, &config.test_named) {
        return HashSet::new();
    }
    skippable(index, baseline, |rel| current_sha256(Path::new(rel)))
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
mod tests {
    use super::*;
    use crate::select::CoverageFile;

    /// `--cov=pkg` rooted at `scope`.
    fn pkg_scope(scope: &Path) -> CovScope {
        CovScope::from_sources(scope, &["pkg"])
    }

    /// Line -> nodeids that covered it (test fixture shorthand).
    type LineSpec<'a> = (u32, &'a [&'a str]);
    /// (file path, content hash, lines) for one file in a test index.
    type FileSpec<'a> = (&'a str, &'a str, &'a [LineSpec<'a>]);

    fn index(files: &[FileSpec]) -> CoverageIndex {
        let mut idx = CoverageIndex {
            schema: 0,
            files: HashMap::new(),
        };
        for (path, hash, lines) in files {
            let mut lm = HashMap::new();
            for (ln, ids) in *lines {
                lm.insert(*ln, ids.iter().map(|s| s.to_string()).collect());
            }
            idx.files.insert(
                (*path).to_string(),
                CoverageFile {
                    hash: (*hash).to_string(),
                    lines: lm,
                },
            );
        }
        idx
    }

    /// Baseline with `ids` green and each of their test files hashed to `tf_hash`.
    fn base(ids: &[&str], tf_hash: &str) -> Baseline {
        let green: HashSet<String> = ids.iter().map(|s| s.to_string()).collect();
        let test_file_hashes = green
            .iter()
            .map(|id| (test_file_of(id).to_string(), tf_hash.to_string()))
            .collect();
        Baseline {
            green,
            test_file_hashes,
            test_lines: HashMap::new(),
            test_named_hashes: HashMap::new(),
        }
    }

    /// A hash stub: test files hash to `tf`, everything else to `src`.
    fn stub<'a>(src: &'a str, tf: &'a str) -> impl Fn(&str) -> Option<String> + 'a {
        move |rel: &str| {
            Some(
                if rel.ends_with(".py") && rel.starts_with('t') {
                    tf
                } else {
                    src
                }
                .to_string(),
            )
        }
    }

    #[test]
    fn green_and_unchanged_is_skippable() {
        // mod.py (hash H) covered by test_a; both mod.py and the test file unchanged.
        let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
        let skip = skippable(&idx, &base(&["t.py::test_a"], "TF"), stub("H", "TF"));
        assert!(skip.contains("t.py::test_a"));
    }

    #[test]
    fn changed_covered_file_is_not_skippable() {
        let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
        // covered mod.py hash differs from stored H.
        let skip = skippable(
            &idx,
            &base(&["t.py::test_a"], "TF"),
            stub("DIFFERENT", "TF"),
        );
        assert!(skip.is_empty());
    }

    #[test]
    fn changed_test_file_is_not_skippable() {
        // The dependency is unchanged, but the test's OWN file was edited.
        let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
        let skip = skippable(&idx, &base(&["t.py::test_a"], "TF"), stub("H", "EDITED"));
        assert!(skip.is_empty(), "editing the test file must force a run");
    }

    #[test]
    fn non_green_test_is_not_skippable() {
        let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
        // test_a not in the green set (failed / never recorded).
        let skip = skippable(&idx, &base(&["t.py::test_b"], "TF"), stub("H", "TF"));
        assert!(!skip.contains("t.py::test_a"));
    }

    #[test]
    fn deleted_covered_file_is_not_skippable() {
        let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
        let skip = skippable(&idx, &base(&["t.py::test_a"], "TF"), |_| None);
        assert!(skip.is_empty());
    }

    #[test]
    fn test_covering_many_files_needs_all_unchanged() {
        // test_a covers a.py (H1) and b.py (H2); a changed, b not.
        let idx = index(&[
            ("a.py", "H1", &[(1, &["t.py::test_a"])]),
            ("b.py", "H2", &[(2, &["t.py::test_a"])]),
        ]);
        let skip = skippable(&idx, &base(&["t.py::test_a"], "TF"), |rel| {
            Some(
                match rel {
                    "a.py" => "CHANGED",
                    "b.py" => "H2",
                    _ => "TF", // the test file
                }
                .to_string(),
            )
        });
        assert!(skip.is_empty(), "one changed dependency must force a run");
    }

    #[test]
    fn carry_forward_restores_cached_test_coverage() {
        // Old index knew both tests; the new one (only test_b ran) dropped
        // test_a's coverage. Carrying test_a forward must restore it.
        let old = index(&[
            ("a.py", "HA", &[(1, &["t.py::test_a"])]),
            ("b.py", "HB", &[(2, &["t.py::test_b"])]),
        ]);
        let mut new = index(&[("b.py", "HB", &[(2, &["t.py::test_b"])])]);
        let cached: HashSet<String> = ["t.py::test_a".to_string()].into_iter().collect();
        carry_forward(&old, &mut new, &cached);
        assert_eq!(new.files["a.py"].hash, "HA");
        assert_eq!(
            new.files["a.py"].lines[&1],
            vec!["t.py::test_a".to_string()]
        );
        assert!(new.files.contains_key("b.py"), "ran test's coverage kept");
    }

    #[test]
    fn conftest_change_busts_config_fingerprint() {
        // A conftest.py outside the coverage scope must still bust skipping:
        // adding, editing, and removing one must each move the fingerprint.
        let scope = std::env::temp_dir().join(format!("rstest-conftest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(scope.join("tests")).unwrap();
        let base = config_fingerprint(&scope, &CovScope::default());
        std::fs::write(scope.join("tests/conftest.py"), b"import pytest\n").unwrap();
        let added = config_fingerprint(&scope, &CovScope::default());
        assert_ne!(base, added, "adding a nested conftest must bust");
        std::fs::write(scope.join("tests/conftest.py"), b"import pytest  # edit\n").unwrap();
        let edited = config_fingerprint(&scope, &CovScope::default());
        assert_ne!(added, edited, "editing a conftest must bust");
        std::fs::remove_file(scope.join("tests/conftest.py")).unwrap();
        assert_eq!(
            base,
            config_fingerprint(&scope, &CovScope::default()),
            "removing it returns to base"
        );
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn coverage_requested_detects_cov_flag_only() {
        assert!(coverage_requested(&["--cov=.".to_string()]));
        assert!(coverage_requested(&["--cov".to_string()]));
        assert!(!coverage_requested(&["-n".to_string(), "2".to_string()]));
        // Coverage sub-options alone don't enable coverage collection.
        assert!(!coverage_requested(&["--cov-report=".to_string()]));
        assert!(!coverage_requested(&["--cov-context=test".to_string()]));
    }

    #[test]
    fn config_fingerprint_folds_uncovered_first_party_source() {
        // Under --cov=pkg, editing an out-of-scope first-party module (other/mod.py)
        // must move the config fingerprint so the skip set busts; editing in-scope
        // source (covered per-file) must NOT — coverage already guards it.
        let scope = std::env::temp_dir().join(format!("rstest-uncov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(scope.join("pkg")).unwrap();
        std::fs::create_dir_all(scope.join("other")).unwrap();
        std::fs::write(scope.join("pkg/mod.py"), b"x = 1\n").unwrap();
        std::fs::write(scope.join("other/mod.py"), b"y = 1\n").unwrap();
        let scopes = pkg_scope(&scope);
        let base = config_fingerprint(&scope, &scopes);
        // Out-of-scope edit busts.
        std::fs::write(scope.join("other/mod.py"), b"y = 2\n").unwrap();
        let after_out = config_fingerprint(&scope, &scopes);
        assert_ne!(base, after_out, "out-of-scope edit must bust");
        // In-scope edit does NOT change the fingerprint (coverage guards it).
        let before_in = config_fingerprint(&scope, &scopes);
        std::fs::write(scope.join("pkg/mod.py"), b"x = 2\n").unwrap();
        assert_eq!(
            before_in,
            config_fingerprint(&scope, &scopes),
            "in-scope source is coverage-visible, not folded here"
        );
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn config_fingerprint_ignores_uncovered_source_when_whole_tree() {
        // With no narrowed scope (empty cov_scopes), out-of-scope .py is NOT
        // folded — whole-tree coverage measures it per-file already.
        let scope = std::env::temp_dir().join(format!("rstest-uncov-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(scope.join("other")).unwrap();
        std::fs::write(scope.join("other/mod.py"), b"y = 1\n").unwrap();
        let base = config_fingerprint(&scope, &CovScope::default());
        std::fs::write(scope.join("other/mod.py"), b"y = 2\n").unwrap();
        assert_eq!(
            base,
            config_fingerprint(&scope, &CovScope::default()),
            "not folded when whole-tree"
        );
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn is_test_file_matches_pytest_unittest_conventions() {
        assert!(is_test_file("test_foo.py"));
        assert!(is_test_file("foo_test.py"));
        assert!(!is_test_file("mod.py"));
        assert!(!is_test_file("contest.py"));
    }

    #[test]
    fn unmeasured_test_named_files_are_tracked_not_folded() {
        // Under --cov=pkg a test-named file outside pkg moves ConfigState's
        // test_named map, never the fingerprint (a test edit must not bust all).
        let scope = std::env::temp_dir().join(format!("rstest-tf-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(scope.join("tests")).unwrap();
        std::fs::write(scope.join("tests/test_utils.py"), b"def make(): return 1\n").unwrap();
        let cov = pkg_scope(&scope);
        let before = config_state(&scope, &cov);
        assert!(before.test_named.contains_key("tests/test_utils.py"));
        std::fs::write(scope.join("tests/test_utils.py"), b"def make(): return 2\n").unwrap();
        let after = config_state(&scope, &cov);
        assert_eq!(before.fp, after.fp);
        assert_ne!(before.test_named, after.test_named);
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn helper_change_busts_but_test_module_change_does_not() {
        // A changed test-named file that holds no green test is a shared helper:
        // nothing is skippable. One that IS a recorded test module is left to
        // the per-test own-file guard.
        let idx = index(&[("mod.py", "H", &[(1, &["t_a.py::test_a"])])]);
        let mut b = base(&["t_a.py::test_a"], "TF");
        b.test_named_hashes = [
            ("t_a.py".to_string(), "A1".to_string()),
            ("test_utils.py".to_string(), "U1".to_string()),
        ]
        .into_iter()
        .collect();
        let now = |a: &str, u: Option<&str>| -> HashMap<String, String> {
            let mut m: HashMap<String, String> = [("t_a.py".to_string(), a.to_string())].into();
            if let Some(u) = u {
                m.insert("test_utils.py".to_string(), u.to_string());
            }
            m
        };
        assert!(!helper_changed(&b, &now("A1", Some("U1"))));
        assert!(
            !helper_changed(&b, &now("A2", Some("U1"))),
            "test module edit"
        );
        assert!(helper_changed(&b, &now("A1", Some("U2"))), "helper edit");
        assert!(helper_changed(&b, &now("A1", None)), "helper deleted");
        let state = ConfigState {
            fp: String::new(),
            test_named: now("A1", Some("U2")),
            index_cov_args: Some(Vec::new()),
        };
        assert!(skippable_now(&idx, &b, &state).is_empty());
    }

    /// A fresh git repo at a temp path (hold the PATH lock: git is spawned).
    fn git_repo(_held: &crate::test_env::Held, name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-fp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
        ] {
            run_git(&d, args);
        }
        d
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    #[test]
    fn git_mode_folds_ignored_and_honors_staging() {
        // In a git checkout the walk still picks the files; git only hashes
        // them. An ignored first-party .py (generated code a test may import)
        // folds like any other, a tracked edit busts, and staging an edit
        // (content unchanged) does not.
        let held = crate::test_env::lock();
        let scope = git_repo(&held, "git");
        std::fs::create_dir_all(scope.join("pkg")).unwrap();
        std::fs::create_dir_all(scope.join("gen")).unwrap();
        std::fs::write(scope.join(".gitignore"), b"gen/\n").unwrap();
        std::fs::write(scope.join("pkg/mod.py"), b"x = 1\n").unwrap();
        std::fs::write(scope.join("helper.py"), b"y = 1\n").unwrap();
        std::fs::write(scope.join("gen/api_pb2.py"), b"g = 1\n").unwrap();
        run_git(&scope, &["add", "."]);
        run_git(&scope, &["commit", "-q", "-m", "init"]);
        let cov = pkg_scope(&scope);
        let paths = [scope.join("helper.py"), scope.join("gen/api_pb2.py")];
        assert!(
            git_blob_ids(&scope, &paths).is_some(),
            "git mode is in effect"
        );
        let base = config_fingerprint(&scope, &cov);
        std::fs::write(scope.join("gen/api_pb2.py"), b"g = 2\n").unwrap();
        let regen = config_fingerprint(&scope, &cov);
        assert_ne!(base, regen, "ignored first-party .py must fold");
        std::fs::write(scope.join("pkg/mod.py"), b"x = 2\n").unwrap();
        assert_eq!(
            regen,
            config_fingerprint(&scope, &cov),
            "in-scope edit must not bust"
        );
        std::fs::write(scope.join("helper.py"), b"y = 2\n").unwrap();
        let edited = config_fingerprint(&scope, &cov);
        assert_ne!(regen, edited, "tracked edit must bust");
        run_git(&scope, &["add", "helper.py"]);
        assert_eq!(
            edited,
            config_fingerprint(&scope, &cov),
            "staging alone must not bust"
        );
        std::fs::write(scope.join("new_helper.py"), b"z = 1\n").unwrap();
        assert_ne!(
            edited,
            config_fingerprint(&scope, &cov),
            "untracked .py must fold"
        );
        std::fs::remove_file(scope.join("new_helper.py")).unwrap();
        std::fs::remove_file(scope.join("helper.py")).unwrap();
        assert_ne!(
            edited,
            config_fingerprint(&scope, &cov),
            "deleting a tracked .py must bust"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn git_ids_match_walk_ids_in_a_repo_subdirectory() {
        // The project may sit in a SUBFOLDER of the repo (monorepo), or even be
        // ignored by a parent repo: hash-object resolves paths from the repo
        // root, so ids must still be right. Git ids equal `git hash-object`'s.
        let held = crate::test_env::lock();
        let repo = git_repo(&held, "subdir");
        let scope = repo.join("services/api");
        std::fs::create_dir_all(&scope).unwrap();
        std::fs::write(scope.join("helper.py"), b"y = 1\n").unwrap();
        std::fs::write(scope.join("tracked.py"), b"t = 1\n").unwrap();
        run_git(&repo, &["add", "services/api/tracked.py"]);
        let paths = [scope.join("helper.py"), scope.join("tracked.py")];
        let ids = git_blob_ids(&scope, &paths).unwrap();
        let expect = |p: &Path| {
            let out = std::process::Command::new("git")
                .args(["hash-object"])
                .arg(p)
                .current_dir(&repo)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        assert_eq!(ids, vec![expect(&paths[0]), expect(&paths[1])]);
        // A parent repo that ignores the whole project still yields ids.
        std::fs::write(repo.join(".gitignore"), b"services/\n").unwrap();
        let cov = pkg_scope(&scope);
        let base = config_fingerprint(&scope, &cov);
        std::fs::write(scope.join("helper.py"), b"y = 2\n").unwrap();
        assert_ne!(
            base,
            config_fingerprint(&scope, &cov),
            "ignored project still folds"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn git_mode_prunes_virtualenv_and_folds_nested_repos() {
        // The walk prunes a venv of any name by pyvenv.cfg, and descends into a
        // nested repo and a submodule checkout (ls-files lists neither's files).
        let held = crate::test_env::lock();
        let scope = git_repo(&held, "venv-nested");
        let sp = scope.join("env/lib/python3.12/site-packages");
        std::fs::create_dir_all(&sp).unwrap();
        std::fs::write(scope.join("env/pyvenv.cfg"), b"home = /usr\n").unwrap();
        std::fs::write(sp.join("dep.py"), b"x = 1\n").unwrap();
        let shared = scope.join("libs/shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("util.py"), b"u = 1\n").unwrap();
        run_git(&shared, &["init", "-q"]);
        let cov = pkg_scope(&scope);
        let base = config_fingerprint(&scope, &cov);
        std::fs::write(sp.join("dep.py"), b"x = 2\n").unwrap();
        assert_eq!(
            base,
            config_fingerprint(&scope, &cov),
            "venv contents must not fold"
        );
        std::fs::write(shared.join("util.py"), b"u = 22\n").unwrap();
        assert_ne!(
            base,
            config_fingerprint(&scope, &cov),
            "nested-repo .py must fold"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_files_and_dirs_fold_by_target_content() {
        // A tracked symlink's git id hashes the link TEXT; a symlinked package
        // dir isn't a dir to `file_type()`. Editing the target must bust both.
        let held = crate::test_env::lock();
        let repo = git_repo(&held, "symlink");
        let scope = repo.join("proj");
        let shared = repo.join("libs/shared");
        std::fs::create_dir_all(&scope).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("util.py"), b"u = 1\n").unwrap();
        std::fs::write(repo.join("libs/helper.py"), b"h = 1\n").unwrap();
        std::os::unix::fs::symlink("../libs/shared", scope.join("shared")).unwrap();
        std::os::unix::fs::symlink("../libs/helper.py", scope.join("helper.py")).unwrap();
        // A loop back to an ancestor must not recurse forever.
        std::os::unix::fs::symlink("..", shared.join("up")).unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-q", "-m", "init"]);
        let cov = pkg_scope(&scope);
        let base = config_fingerprint(&scope, &cov);
        std::fs::write(repo.join("libs/helper.py"), b"h = 22\n").unwrap();
        let after_file = config_fingerprint(&scope, &cov);
        assert_ne!(base, after_file, "symlinked file target edit must bust");
        std::fs::write(shared.join("util.py"), b"u = 22\n").unwrap();
        assert_ne!(
            after_file,
            config_fingerprint(&scope, &cov),
            "symlinked dir edit must bust"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn git_mode_hashes_thousands_of_dirty_files_without_deadlock() {
        // Enough untracked paths that both hash-object's stdin (paths) and its
        // stdout (ids) exceed a pipe buffer: writing all input before reading
        // any output would hang here.
        let held = crate::test_env::lock();
        let scope = git_repo(&held, "many");
        let dir = scope.join("a_directory_name_long_enough_to_fill_the_pipe_quickly");
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = (0..3000)
            .map(|i| {
                let p = dir.join(format!("module_{i:05}.py"));
                std::fs::write(&p, format!("v = {i}\n")).unwrap();
                p
            })
            .collect();
        let ids = git_blob_ids(&scope, &paths).unwrap();
        assert_eq!(ids.len(), 3000);
        assert_ne!(ids[0], ids[1]);
        drop(held);
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn config_fingerprint_does_not_fold_out_of_scope_test_files() {
        // Under --cov=pkg, editing an out-of-scope TEST file must NOT move the
        // config fingerprint: test-file edits are guarded per-test elsewhere, so
        // folding them here would bust the whole skip set.
        let scope = std::env::temp_dir().join(format!("rstest-tf-fold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(scope.join("tests")).unwrap();
        std::fs::write(scope.join("tests/test_foo.py"), b"def test_x(): pass\n").unwrap();
        let scopes = pkg_scope(&scope);
        let base = config_fingerprint(&scope, &scopes);
        std::fs::write(
            scope.join("tests/test_foo.py"),
            b"def test_x(): pass  # edit\n",
        )
        .unwrap();
        assert_eq!(
            base,
            config_fingerprint(&scope, &scopes),
            "out-of-scope test-file edit must NOT bust the fingerprint"
        );
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn config_fingerprint_prunes_venv_by_pyvenv_cfg() {
        // A venv of a non-standard name (not in PRUNE_DIRS) is pruned by its
        // pyvenv.cfg marker: its site-packages .py must NOT fold as first-party.
        let scope = std::env::temp_dir().join(format!("rstest-venv-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        let sp = scope
            .join("env")
            .join("lib")
            .join("python3.12")
            .join("site-packages");
        std::fs::create_dir_all(&sp).unwrap();
        std::fs::write(scope.join("env/pyvenv.cfg"), b"home = /usr\n").unwrap();
        std::fs::write(sp.join("dep.py"), b"x = 1\n").unwrap();
        let scopes = pkg_scope(&scope);
        let base = config_fingerprint(&scope, &scopes);
        // Editing a third-party module inside the venv must not move the fp.
        std::fs::write(sp.join("dep.py"), b"x = 2\n").unwrap();
        assert_eq!(
            base,
            config_fingerprint(&scope, &scopes),
            "venv (pyvenv.cfg) contents must not fold as first-party"
        );
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn config_fingerprint_folds_dot_slash_scoped_source_correctly() {
        // --cov=./pkg must classify pkg/ as in-scope (not fold the whole tree):
        // editing in-scope source stays stable, out-of-scope busts.
        let scope = std::env::temp_dir().join(format!("rstest-dotslash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(scope.join("pkg")).unwrap();
        std::fs::create_dir_all(scope.join("other")).unwrap();
        std::fs::write(scope.join("pkg/mod.py"), b"x = 1\n").unwrap();
        std::fs::write(scope.join("other/mod.py"), b"y = 1\n").unwrap();
        let scopes = CovScope::from_sources(&scope, &["./pkg"]);
        let base = config_fingerprint(&scope, &scopes);
        // In-scope edit must NOT bust (proves ./pkg matched, not folded-everything).
        std::fs::write(scope.join("pkg/mod.py"), b"x = 2\n").unwrap();
        assert_eq!(
            base,
            config_fingerprint(&scope, &scopes),
            "in-scope stays stable"
        );
        // Out-of-scope edit still busts.
        std::fs::write(scope.join("other/mod.py"), b"y = 2\n").unwrap();
        assert_ne!(
            base,
            config_fingerprint(&scope, &scopes),
            "out-of-scope busts"
        );
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn config_file_change_busts_config_fingerprint() {
        // A tracked config file's presence and content must fold into the
        // fingerprint (the CONFIG_FILES loop body).
        let scope = std::env::temp_dir().join(format!("rstest-cfgfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(&scope).unwrap();
        let empty = config_fingerprint(&scope, &CovScope::default());
        std::fs::write(scope.join("pyproject.toml"), b"[tool.pytest]\n").unwrap();
        let added = config_fingerprint(&scope, &CovScope::default());
        assert_ne!(empty, added, "adding pyproject.toml must bust");
        std::fs::write(scope.join("pyproject.toml"), b"[tool.pytest]  # edit\n").unwrap();
        assert_ne!(
            added,
            config_fingerprint(&scope, &CovScope::default()),
            "editing it must bust"
        );
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn config_fingerprint_on_missing_scope_is_stable() {
        // Nonexistent scope: read_dir fails (the walk returns early) and
        // no config file hashes → the empty-hash fingerprint, computed twice equal.
        let scope = std::env::temp_dir().join(format!("rstest-nodir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        assert!(!scope.exists());
        assert_eq!(
            config_fingerprint(&scope, &CovScope::default()),
            config_fingerprint(&scope, &CovScope::default())
        );
    }

    #[test]
    fn test_file_hashes_hashes_existing_files_only() {
        // An existing test file is hashed once; a nonexistent one is omitted.
        let dir = std::env::temp_dir().join(format!("rstest-tfh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("t_real.py");
        std::fs::write(&real, b"def test_x(): pass\n").unwrap();
        let real_id = format!("{}::test_x", real.to_string_lossy());
        let green: HashSet<String> = [real_id.clone(), "does_not_exist.py::test_y".to_string()]
            .into_iter()
            .collect();
        let hashes = test_file_hashes(&green);
        assert!(hashes.contains_key(real.to_string_lossy().as_ref()));
        assert!(!hashes.contains_key("does_not_exist.py"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_and_read_round_trip_with_config_gate() {
        let scope = std::env::temp_dir().join(format!("rstest-covskip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(&scope).unwrap();
        let green: HashSet<String> = ["t.py::test_a".to_string()].into_iter().collect();
        let lines: HashMap<String, u64> = [("t.py::test_a".to_string(), 7)].into_iter().collect();
        let cfg = |fp: &str| ConfigState {
            fp: fp.to_string(),
            test_named: HashMap::new(),
            index_cov_args: Some(vec!["--cov=pkg".to_string()]),
        };
        record(&scope, &cfg("cfg-A"), green, lines);
        let b = load(&scope, "cfg-A");
        assert!(b.green.contains("t.py::test_a"));
        // The def line round-trips for restoring a cached entry next run.
        assert_eq!(b.test_lines.get("t.py::test_a"), Some(&7));
        // A config change (different fingerprint) disables skipping.
        assert!(load(&scope, "cfg-B").green.is_empty());
        // The index's coverage args round-trip, ungated by the fingerprint.
        assert_eq!(
            stored_index_cov_args(&scope),
            Some(vec!["--cov=pkg".to_string()])
        );
    }

    #[test]
    fn test_file_hashes_dedups_shared_test_file() {
        // Two green nodeids in ONE test file: the file is hashed exactly once
        // (the `contains_key` short-circuit), not re-read per nodeid.
        let dir = std::env::temp_dir().join(format!("rstest-tfh-dedup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tf = dir.join("t_shared.py");
        std::fs::write(&tf, b"def test_x(): pass\ndef test_y(): pass\n").unwrap();
        let rel = tf.to_string_lossy().to_string();
        let green: HashSet<String> = [format!("{rel}::test_x"), format!("{rel}::test_y")]
            .into_iter()
            .collect();
        let hashes = test_file_hashes(&green);
        // One distinct file -> one entry, regardless of the two nodeids.
        assert_eq!(hashes.len(), 1);
        assert!(hashes.contains_key(rel.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skippable_empty_green_baseline_skips_nothing() {
        // No recorded green set -> the early return: nothing is skippable even
        // when the index and hashes would otherwise match.
        let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
        let empty = Baseline::default();
        assert!(skippable(&idx, &empty, stub("H", "TF")).is_empty());
    }

    #[test]
    fn skippable_now_hashes_live_files() {
        // Wire [`skippable`] to the real working tree via absolute paths (so it
        // is cwd-independent): a green test whose covered file + own file are
        // byte-identical to the index/baseline hashes is skippable.
        let dir = std::env::temp_dir().join(format!("rstest-skipnow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let modf = dir.join("mod.py");
        let testf = dir.join("t.py");
        std::fs::write(&modf, b"x = 1\n").unwrap();
        std::fs::write(&testf, b"def test_a(): pass\n").unwrap();
        let modrel = modf.to_string_lossy().to_string();
        let testrel = testf.to_string_lossy().to_string();
        let id = format!("{testrel}::test_a");
        let mod_hash = current_sha256(&modf).unwrap();
        let test_hash = current_sha256(&testf).unwrap();
        let idx = index(&[(modrel.as_str(), mod_hash.as_str(), &[(1, &[id.as_str()])])]);
        let green: HashSet<String> = [id.clone()].into_iter().collect();
        let baseline = Baseline {
            green,
            test_file_hashes: [(testrel, test_hash)].into_iter().collect(),
            test_lines: HashMap::new(),
            test_named_hashes: HashMap::new(),
        };
        let cfg = ConfigState {
            index_cov_args: Some(Vec::new()),
            ..ConfigState::default()
        };
        assert!(skippable_now(&idx, &baseline, &cfg).contains(&id));
        // An index of unknown coverage scope is never trusted.
        assert!(skippable_now(&idx, &baseline, &ConfigState::default()).is_empty());
        // Editing the covered file busts it.
        std::fs::write(&modf, b"x = 2\n").unwrap();
        assert!(skippable_now(&idx, &baseline, &cfg).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_fingerprint_inputs_prunes_and_descends() {
        // Real filesystem so the walk sees genuine dir/file FileTypes: a conftest
        // at root and in a nested source dir is collected; one under a pruned dir
        // (.venv), a dot dir, and an Err file_type arm are not reached.
        let root = std::env::temp_dir().join(format!("rstest-conf-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".venv")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("conftest.py"), b"").unwrap();
        std::fs::write(root.join("src/conftest.py"), b"").unwrap();
        std::fs::write(root.join("not_a_conftest.py"), b"").unwrap();
        std::fs::write(root.join(".venv/conftest.py"), b"").unwrap();
        std::fs::write(root.join(".hidden/conftest.py"), b"").unwrap();
        let mut out = FingerprintInputs::default();
        collect_fingerprint_inputs(&root, &root, None, &mut out);
        let mut names: Vec<String> = out
            .conftests
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["conftest.py", "src/conftest.py"]);
        // With no cov scope, no uncovered .py is collected (not_a_conftest.py skipped).
        assert!(out.unmeasured.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
