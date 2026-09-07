//! Coverage-aware selection: a line->test index (built during a warm run) lets
//! `--changed` pick the exact tests whose coverage hit the changed lines,
//! falling back to the import graph for anything the index can't vouch for.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::git::ChangedLines;
use super::graph::affected_tests;
use super::{rule1_full_run, Selection};
use crate::cache;
use crate::config::ProjectConfig;

pub const COVERAGE_INDEX_SCHEMA: u32 = 2;
/// Filename of the coverage index within the cache dir (`cache::file`).
pub const COVERAGE_INDEX_FILE: &str = "coverage_index.json";

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Default, PartialEq)]
pub struct CoverageFile {
    /// SHA-256 of the file's content when the index was built. The line map is
    /// only valid for a base whose content still hashes to this - see
    /// `old_side_sha256` and the drift check in `affected_with_coverage`.
    #[serde(default)]
    pub hash: String,
    /// line number -> nodeids that covered it
    #[serde(default)]
    pub lines: HashMap<u32, Vec<String>>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Default, PartialEq)]
pub struct CoverageIndex {
    #[serde(default)]
    pub schema: u32,
    /// relative file path -> per-file coverage entry (hash + line map)
    #[serde(default)]
    pub files: HashMap<String, CoverageFile>,
}

/// Load the coverage index from the cache dir (honors `RSTEST_CACHE`), or `None`
/// when missing, unreadable, corrupt, or an unrecognized schema - every `None`
/// makes the caller fall back to import-graph selection. A v1 (pre-hash) index
/// fails the schema check as cold.
fn load_coverage_index() -> Option<CoverageIndex> {
    let bytes = std::fs::read(cache::file(COVERAGE_INDEX_FILE)).ok()?;
    let idx: CoverageIndex = serde_json::from_slice(&bytes).ok()?;
    (idx.schema == COVERAGE_INDEX_SCHEMA).then_some(idx)
}

/// Strip the CR from every CRLF so a CRLF working tree and its LF blob hash equal
/// under git's autocrlf/text filters (the drift hashes must agree, but a real
/// content edit still changes the hash). Exotic clean/smudge filters just fall back.
fn normalize_newlines(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1; // drop the CR, keep the following LF
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Hex SHA-256 of `rel`'s content at the diff `base` (`git show base:./rel`), or
/// `None` if absent. The `./` prefix resolves relative to CWD (monorepo safety);
/// newlines are normalized so it matches the indexer's hash for the drift check.
fn old_side_sha256(base: &str, rel: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let spec = format!("{base}:./{}", rel.to_string_lossy().replace('\\', "/"));
    let out = std::process::Command::new("git")
        .args(["show", &spec])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut h = Sha256::new();
    h.update(normalize_newlines(&out.stdout));
    Some(crate::incremental::hex_encode(&h.finalize()))
}

/// Hex SHA-256 of `rel`'s CURRENT working-tree content, normalized the same way
/// as the indexer's stored hash, or `None` if the file is unreadable/absent. Lets
/// the incremental skip cache compare a covered file's live content against the
/// hash the coverage index recorded, with no git dependency.
pub(crate) fn current_sha256(rel: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(rel).ok()?;
    let mut h = Sha256::new();
    h.update(normalize_newlines(&bytes));
    Some(crate::incremental::hex_encode(&h.finalize()))
}

/// The single commit `git diff <rev>` uses as its OLD side, which the drift hash
/// is keyed to. `git show` needs one commit but `--changed` accepts ranges: `A..B`
/// reduces to `A`, `A...B` to `merge-base(A, B)`; a bare ref is its own old side.
fn diff_old_side(rev: Option<&str>) -> String {
    let rev = rev.unwrap_or("HEAD");
    // `...` must be checked before `..` (the latter is a prefix of the former).
    if let Some((left, right)) = rev.split_once("...") {
        let left = if left.is_empty() { "HEAD" } else { left };
        let right = if right.is_empty() { "HEAD" } else { right };
        if let Ok(out) = std::process::Command::new("git")
            .args(["merge-base", left, right])
            .output()
        {
            if out.status.success() {
                let mb = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !mb.is_empty() {
                    return mb;
                }
            }
        }
        // merge-base unavailable - fall back to the left side; a mismatched
        // drift hash just routes the file to the import graph (safe).
        return left.to_string();
    }
    if let Some((left, _right)) = rev.split_once("..") {
        return if left.is_empty() { "HEAD" } else { left }.to_string();
    }
    rev.to_string()
}

/// Coverage-aware selection: consult the line->test index to pick the exact tests
/// whose coverage hit the changed lines, falling back to the import graph for what
/// it can't vouch for (new code, unmeasured/drifted files) and full run for config.
pub fn affected_with_coverage(
    rootdir: &Path,
    project: &ProjectConfig,
    changes: &ChangedLines,
    strict: bool,
    rev: Option<&str>,
) -> Result<Selection> {
    let files: Vec<PathBuf> = changes.keys().cloned().collect();
    if let Some(full) = rule1_full_run(&files) {
        return Ok(full);
    }
    let Some(index) = load_coverage_index() else {
        // Cold cache: identical to the import-graph selector.
        return affected_tests(rootdir, project, &files, strict);
    };
    // Only reached with a warm index. The index's line numbers are keyed to the
    // warmed source, the diff old-side to this base; they align only when the file's
    // base content still matches (per-file drift check), so a range rev is reduced.
    let base = diff_old_side(rev);
    // Changed-file keys/index nodeids are CWD-relative (git `--relative`); graph
    // fallback results are ROOTDIR-relative. Resolve each against its own base so
    // existence checks and the dedup compare real paths when rootdir != cwd.
    let cwd = std::env::current_dir().unwrap_or_else(|_| rootdir.to_path_buf());

    let mut nodeids: BTreeSet<String> = BTreeSet::new();
    let mut fallback: Vec<PathBuf> = Vec::new();
    let mut direct_tests: BTreeSet<PathBuf> = BTreeSet::new();

    for (file, change) in changes {
        // A changed test file always runs its own tests (its assertions/fixtures
        // may have changed). A DELETED test file (name matches, no file on disk)
        // is skipped rather than handed to pytest as a missing path.
        if crate::collect::is_test_file(&rootdir.join(file), project) {
            // `file` is cwd-relative - resolve existence against cwd, not
            // rootdir, so a deleted test isn't misjudged when rootdir != cwd.
            if cwd.join(file).exists() {
                direct_tests.insert(file.clone());
            }
            continue;
        }
        // conftest.py subtree semantics live in the graph path (its Rule 2).
        if file.file_name().and_then(|n| n.to_str()) == Some("conftest.py") {
            fallback.push(file.clone());
            continue;
        }
        // Look up the OLD-side changed lines (index is keyed pre-change).
        let key = file.to_string_lossy().replace('\\', "/");
        // Drift guard: the index's line numbers are valid only if the base
        // content still hashes to what the index was built from; on mismatch (or
        // unreadable base) treat the entry as absent and fall back to the graph.
        let indexed = index
            .files
            .get(&key)
            .filter(|e| old_side_sha256(&base, file).as_deref() == Some(e.hash.as_str()));
        // A changed old-side line the index has no nodeid for (import-time
        // def/decorator line dropped from the empty context, or a blank/comment)
        // would select ZERO tests: route such a file to the graph instead.
        let mut uncovered_line = false;
        if let Some(entry) = indexed {
            for &(start, end) in &change.old_ranges {
                for line in start..=end {
                    match entry.lines.get(&line) {
                        Some(ids) => {
                            for id in ids {
                                // A stale nodeid (test renamed/deleted since warm)
                                // would error pytest or skip real coverage, so treat
                                // it as uncovered and fall the file back to the graph.
                                let file_part = crate::text::nodeid_file(id);
                                if cwd.join(file_part).exists() {
                                    nodeids.insert(id.clone());
                                } else {
                                    uncovered_line = true;
                                }
                            }
                        }
                        None => uncovered_line = true,
                    }
                }
            }
        }
        // Brand-new code, a file the index never measured, or a changed line
        // the index can't account for needs the conservative graph.
        if change.has_new_code || indexed.is_none() || uncovered_line {
            fallback.push(file.clone());
        }
    }

    let graph_tests = if fallback.is_empty() {
        Vec::new()
    } else {
        match affected_tests(rootdir, project, &fallback, strict)? {
            Selection::Tests(t) => t,
            // A strict fallback that can't prove reachability => full run.
            full @ Selection::FullRun(_) => return Ok(full),
        }
    };

    // Whole-file selections run every test in a file, so a `file::test` nodeid for
    // the same file is redundant (pytest would collect it twice). Compare on
    // absolute paths (graph_tests are rootdir-relative, nodeids cwd-relative).
    let abs = |root: &Path, p: &Path| -> PathBuf {
        let joined = root.join(p);
        joined.canonicalize().unwrap_or(joined)
    };
    let whole_files: BTreeSet<PathBuf> = graph_tests
        .iter()
        .map(|p| abs(rootdir, p))
        .chain(direct_tests.iter().map(|p| abs(&cwd, p)))
        .collect();
    let mut selected: BTreeSet<PathBuf> = BTreeSet::new();
    // nodeids were already checked for existence as they were collected; any
    // stale entry demoted its file to the graph fallback above.
    for id in nodeids {
        let file_part = crate::text::nodeid_file(&id);
        if !whole_files.contains(&abs(&cwd, Path::new(file_part))) {
            selected.insert(PathBuf::from(id));
        }
    }
    selected.extend(graph_tests);
    selected.extend(direct_tests);
    Ok(Selection::Tests(selected.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::{
        affected_with_coverage, diff_old_side, normalize_newlines, old_side_sha256, CoverageFile,
        CoverageIndex, COVERAGE_INDEX_FILE, COVERAGE_INDEX_SCHEMA,
    };
    use super::{ChangedLines, Selection};
    use crate::config::ProjectConfig;
    use crate::select::git::FileChange;
    use crate::select::GLOBAL_TEST_LOCK as GLOBAL;
    use std::path::{Path, PathBuf};

    struct Cwd {
        orig: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for Cwd {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.orig);
        }
    }
    fn enter(dir: &Path) -> Cwd {
        let lock = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        Cwd { orig, _lock: lock }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn init_repo(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-cov-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        git(&d, &["init", "-q"]);
        git(&d, &["config", "user.email", "t@example.com"]);
        git(&d, &["config", "user.name", "t"]);
        git(&d, &["config", "commit.gpgsign", "false"]);
        d
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn old_side_sha256_hashes_committed_content_and_none_when_absent() {
        let repo = init_repo("oldside");
        write(&repo, "a.py", "x = 1\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let _cwd = enter(&repo);
        // Present at HEAD: a hash matching the working-tree content (unchanged).
        let h = old_side_sha256("HEAD", Path::new("a.py")).expect("committed file hashes");
        assert_eq!(h.len(), 64);
        assert_eq!(
            super::current_sha256(Path::new("a.py")).as_deref(),
            Some(&*h)
        );
        // Absent at HEAD: git show fails -> None.
        assert_eq!(old_side_sha256("HEAD", Path::new("nope.py")), None);
    }

    #[test]
    fn diff_old_side_resolves_triple_dot_via_merge_base() {
        let repo = init_repo("tripledot");
        write(&repo, "a.py", "x = 1\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let _cwd = enter(&repo);
        let head = {
            let out = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        // `HEAD...HEAD` reduces to merge-base(HEAD, HEAD) == HEAD.
        assert_eq!(diff_old_side(Some("HEAD...HEAD")), head);
        // merge-base of two bogus refs fails -> fall back to the left side.
        assert_eq!(diff_old_side(Some("bad1...bad2")), "bad1");
        // Empty left side of a `...` range means HEAD before the merge-base call.
        assert_eq!(diff_old_side(Some("...HEAD")), head);
    }

    #[test]
    fn conftest_change_falls_back_to_the_graph_with_a_warm_index() {
        let _lock = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("rstest-cov-conftest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Canonical rootdir so graph-fallback results come back rootdir-relative.
        let root = root.canonicalize().unwrap();
        write(&root, "pkg/conftest.py", "");
        write(&root, "pkg/test_a.py", "def test_a():\n    pass\n");

        // Warm (schema-current) index in a private cache dir.
        let cache_dir = root.join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let index = CoverageIndex {
            schema: COVERAGE_INDEX_SCHEMA,
            files: Default::default(),
        };
        std::fs::write(
            cache_dir.join(COVERAGE_INDEX_FILE),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        let saved = std::env::var("RSTEST_CACHE").ok();
        std::env::set_var("RSTEST_CACHE", &cache_dir);

        let mut changes: ChangedLines = ChangedLines::new();
        changes.insert(PathBuf::from("pkg/conftest.py"), FileChange::default());
        let sel = affected_with_coverage(&root, &ProjectConfig::default(), &changes, false, None);

        match saved {
            Some(v) => std::env::set_var("RSTEST_CACHE", v),
            None => std::env::remove_var("RSTEST_CACHE"),
        }

        // A changed conftest routes to the graph (Rule 2), selecting its subtree.
        match sel.unwrap() {
            Selection::Tests(tests) => {
                assert!(tests.contains(&PathBuf::from("pkg/test_a.py")), "{tests:?}");
            }
            Selection::FullRun(r) => panic!("unexpected full run: {r}"),
        }
        // Keep the `_` binding to prove the warm-index CoverageFile type is wired.
        let _ = CoverageFile::default();
    }

    #[test]
    fn diff_old_side_reduces_ranges_to_a_single_commit() {
        // Bare ref (and None => HEAD) is already its own old side - no shell-out.
        assert_eq!(diff_old_side(None), "HEAD");
        assert_eq!(diff_old_side(Some("origin/main")), "origin/main");
        assert_eq!(diff_old_side(Some("HEAD~3")), "HEAD~3");
        // `A..B` diffs against A (the left side).
        assert_eq!(diff_old_side(Some("origin/main..HEAD")), "origin/main");
        assert_eq!(diff_old_side(Some("HEAD~2..HEAD")), "HEAD~2");
        // Empty left side of a range means HEAD.
        assert_eq!(diff_old_side(Some("..HEAD")), "HEAD");
        // `...` is matched before `..`, so it is never mis-split into "" / ".B".
        // (The symmetric case resolves via `git merge-base`, exercised in the
        // integration tests; here we only assert `..` doesn't steal it.)
    }

    #[test]
    fn newline_normalization_makes_crlf_and_lf_hash_equal() {
        // The whole point: CRLF working tree and LF blob must normalize equal.
        assert_eq!(normalize_newlines(b"a\r\nb\r\n"), b"a\nb\n");
        assert_eq!(normalize_newlines(b"a\nb\n"), b"a\nb\n");
        // A lone CR (old-Mac, or mid-line) is NOT a line ending git rewrites,
        // so it is preserved - only CR immediately before LF is dropped.
        assert_eq!(normalize_newlines(b"a\rb"), b"a\rb");
        assert_eq!(normalize_newlines(b"trailing\r"), b"trailing\r");
        // Content difference still survives normalization.
        assert_ne!(normalize_newlines(b"x\r\n"), normalize_newlines(b"y\r\n"));
    }
}
