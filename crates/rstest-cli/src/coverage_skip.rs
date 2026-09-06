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
//! One more coverage-invisible gap: FIRST-PARTY source outside the `--cov`
//! scope. Under a narrowed `--cov=<pkg>`, coverage only measures `<pkg>`, so a
//! test that imports a sibling first-party module NOT under `<pkg>` (and that
//! isn't a conftest — those are folded into the config fingerprint) records no
//! coverage for it. Editing that module then leaves every tracked hash
//! byte-identical → the test is wrongly cached (a stale false-green). `--cov=.`
//! (cover the whole tree) closes this; [`cov_scope_narrowed`] detects the risky
//! case so the run can warn. Same escape hatch as the other gaps: one `--cov=.`
//! or full run re-establishes, or delete the cache file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache;
use crate::select::{current_sha256, CoverageFile, CoverageIndex, COVERAGE_INDEX_FILE};

/// Filename of the per-test outcome store within the cache dir.
pub const FILE: &str = "incremental_outcomes.json";

// Schema 2 added `test_lines` (nodeid -> def line) for restoring cached entries'
// source line; an older schema-1 store reads as absent (one full run to rebuild).
const SCHEMA: u32 = 2;

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
}

/// Hash the current content of the project's config files (order-stable), so a
/// change to any of them can bust the skip decision. `conftest.py` files are
/// folded in the same way: they carry fixtures/hooks/addopts that change test
/// behavior but frequently live OUTSIDE the coverage scope (e.g. `--cov=<pkg>`
/// with `tests/conftest.py`), so per-file coverage hashing can't see a conftest
/// edit. Folding every conftest under `scope` here means adding, editing, or
/// removing one busts skipping wholesale — the same guard the config files get.
pub fn config_fingerprint(scope: &Path) -> String {
    let mut h = Sha256::new();
    for name in CONFIG_FILES {
        if let Some(sha) = current_sha256(&scope.join(name)) {
            h.update(name.as_bytes());
            h.update(sha.as_bytes());
        }
    }
    let mut conftests = Vec::new();
    collect_conftests(scope, &mut conftests);
    conftests.sort();
    for path in &conftests {
        if let Some(sha) = current_sha256(path) {
            let rel = path.strip_prefix(scope).unwrap_or(path);
            h.update(rel.to_string_lossy().as_bytes());
            h.update(sha.as_bytes());
        }
    }
    format!("{:x}", h.finalize())
}

/// What to do with one directory entry while hunting for `conftest.py`.
enum EntryAction {
    /// Ignore it (unreadable type, pruned/dot dir, or an unrelated file).
    Skip,
    /// A directory worth recursing into.
    Descend,
    /// A `conftest.py` to collect.
    Collect,
}

/// Classify one entry from its `file_type()` result and name. Pure over its
/// inputs (no filesystem access) so the unreadable-`file_type` arm is testable:
/// an `Err` — which only happens when the FS returns `DT_UNKNOWN` and the
/// follow-up `lstat` fails, unreachable on APFS/ext4 — is simply skipped.
fn classify_entry(ft: std::io::Result<std::fs::FileType>, name: &std::ffi::OsStr) -> EntryAction {
    let Ok(ft) = ft else {
        return EntryAction::Skip;
    };
    if ft.is_dir() {
        let name = name.to_string_lossy();
        if name.starts_with('.') || PRUNE_DIRS.contains(&name.as_ref()) {
            EntryAction::Skip
        } else {
            EntryAction::Descend
        }
    } else if name == "conftest.py" {
        EntryAction::Collect
    } else {
        EntryAction::Skip
    }
}

/// Recursively collect every `conftest.py` under `scope`, pruning virtualenv /
/// VCS / cache directories (and any dot-directory) so the walk stays bounded.
fn collect_conftests(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        match classify_entry(entry.file_type(), &entry.file_name()) {
            EntryAction::Descend => collect_conftests(&entry.path(), out),
            EntryAction::Collect => out.push(entry.path()),
            EntryAction::Skip => {}
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

/// Whether coverage is scoped to a subtree rather than the whole project — the
/// condition under which first-party source OUTSIDE the scope is coverage-
/// invisible (see the module-level soundness note). Whole-tree scopes cover the
/// cwd and are NOT narrowed: bare `--cov`, and `--cov=` with an empty / `.` /
/// `./` value (pytest-cov reads all of these as "measure the cwd"). A subtree
/// value like `--cov=pkg` or `--cov=src/pkg` IS narrowed. Coverage is the UNION
/// of every `--cov`, so a single whole-tree `--cov` broadens back to everything
/// even alongside a scoped one (`--cov=pkg --cov=.` is not narrowed). Sub-options
/// (`--cov-report`/`--cov-context`) don't set the measured set and are ignored.
pub fn cov_scope_narrowed(args: &[String]) -> bool {
    let is_whole_tree = |v: &str| matches!(v, "" | "." | "./");
    let mut any_scoped = false;
    for a in args {
        // Bare `--cov` measures the cwd tree: unions in everything.
        if a == "--cov" {
            return false;
        }
        if let Some(v) = a.strip_prefix("--cov=") {
            if is_whole_tree(v) {
                return false;
            }
            any_scoped = true;
        }
    }
    any_scoped
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
        })
        .unwrap_or_default()
}

/// Persist the green set + per-test-file hashes + per-nodeid def lines + config
/// fingerprint after a run. Best-effort: a cache-write failure never fails the
/// run. `lines` maps green nodeids to their source def line (restores a cached
/// entry's line next run); nodeids without a known line are simply absent.
pub fn record(scope: &Path, config_fp: &str, green: HashSet<String>, lines: HashMap<String, u64>) {
    let test_file_hashes = test_file_hashes(&green);
    let doc = Outcomes {
        schema: SCHEMA,
        config_fp: config_fp.to_string(),
        green,
        test_file_hashes,
        test_lines: lines,
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
    nodeid.split("::").next().unwrap_or(nodeid)
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
/// content (`rel` is cwd-relative, matching the index keys).
pub fn skippable_now(index: &CoverageIndex, baseline: &Baseline) -> HashSet<String> {
    skippable(index, baseline, |rel| current_sha256(Path::new(rel)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::CoverageFile;

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
        let base = config_fingerprint(&scope);
        std::fs::write(scope.join("tests/conftest.py"), b"import pytest\n").unwrap();
        let added = config_fingerprint(&scope);
        assert_ne!(base, added, "adding a nested conftest must bust");
        std::fs::write(scope.join("tests/conftest.py"), b"import pytest  # edit\n").unwrap();
        let edited = config_fingerprint(&scope);
        assert_ne!(added, edited, "editing a conftest must bust");
        std::fs::remove_file(scope.join("tests/conftest.py")).unwrap();
        assert_eq!(
            base,
            config_fingerprint(&scope),
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
    fn cov_scope_narrowed_flags_subtree_scopes_only() {
        // Whole-tree scopes are not narrowed.
        assert!(!cov_scope_narrowed(&["--cov".to_string()]));
        assert!(!cov_scope_narrowed(&["--cov=.".to_string()]));
        assert!(!cov_scope_narrowed(&["--cov=./".to_string()]));
        // `--cov=` (empty value) reads as cover-cwd in pytest-cov, not narrowed.
        assert!(!cov_scope_narrowed(&["--cov=".to_string()]));
        // A package/subtree scope is narrowed (first-party outside is invisible).
        assert!(cov_scope_narrowed(&["--cov=pkg".to_string()]));
        assert!(cov_scope_narrowed(&["--cov=src/pkg".to_string()]));
        // Coverage is the UNION of every --cov: a whole-tree entry broadens back
        // to everything even next to a scoped one, in either order.
        assert!(!cov_scope_narrowed(&[
            "--cov=pkg".to_string(),
            "--cov=.".to_string()
        ]));
        assert!(!cov_scope_narrowed(&[
            "--cov=pkg".to_string(),
            "--cov".to_string()
        ]));
        assert!(!cov_scope_narrowed(&[
            "--cov".to_string(),
            "--cov=pkg".to_string()
        ]));
        // Sub-options don't set the measured scope.
        assert!(!cov_scope_narrowed(&["--cov-context=test".to_string()]));
        assert!(!cov_scope_narrowed(&["--cov-report=".to_string()]));
    }

    #[test]
    fn classify_entry_skips_on_file_type_error() {
        // Unreadable file_type (DT_UNKNOWN + failed lstat) must be skipped, not
        // collected or descended into — the arm no real FS reaches in-process.
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(matches!(
            classify_entry(Err(err), std::ffi::OsStr::new("conftest.py")),
            EntryAction::Skip
        ));
    }

    #[test]
    fn config_file_change_busts_config_fingerprint() {
        // A tracked config file's presence and content must fold into the
        // fingerprint (the CONFIG_FILES loop body).
        let scope = std::env::temp_dir().join(format!("rstest-cfgfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(&scope).unwrap();
        let empty = config_fingerprint(&scope);
        std::fs::write(scope.join("pyproject.toml"), b"[tool.pytest]\n").unwrap();
        let added = config_fingerprint(&scope);
        assert_ne!(empty, added, "adding pyproject.toml must bust");
        std::fs::write(scope.join("pyproject.toml"), b"[tool.pytest]  # edit\n").unwrap();
        assert_ne!(added, config_fingerprint(&scope), "editing it must bust");
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn config_fingerprint_on_missing_scope_is_stable() {
        // Nonexistent scope: read_dir fails (collect_conftests returns early) and
        // no config file hashes → the empty-hash fingerprint, computed twice equal.
        let scope = std::env::temp_dir().join(format!("rstest-nodir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        assert!(!scope.exists());
        assert_eq!(config_fingerprint(&scope), config_fingerprint(&scope));
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
        record(&scope, "cfg-A", green, lines);
        let b = load(&scope, "cfg-A");
        assert!(b.green.contains("t.py::test_a"));
        // The def line round-trips for restoring a cached entry next run.
        assert_eq!(b.test_lines.get("t.py::test_a"), Some(&7));
        // A config change (different fingerprint) disables skipping.
        assert!(load(&scope, "cfg-B").green.is_empty());
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
        };
        assert!(skippable_now(&idx, &baseline).contains(&id));
        // Editing the covered file busts it.
        std::fs::write(&modf, b"x = 2\n").unwrap();
        assert!(skippable_now(&idx, &baseline).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_conftests_prunes_and_descends() {
        // Real filesystem so classify_entry sees genuine dir/file FileTypes:
        // a conftest at root and in a nested source dir is collected; one under
        // a pruned dir (.venv) and a dot dir is not.
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
        let mut out = Vec::new();
        collect_conftests(&root, &mut out);
        let mut names: Vec<String> = out
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
        let _ = std::fs::remove_dir_all(&root);
    }
}
