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

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache;
use crate::select::{current_sha256, CoverageFile, CoverageIndex, COVERAGE_INDEX_FILE};

/// Filename of the per-test outcome store within the cache dir.
pub const FILE: &str = "incremental_outcomes.json";

const SCHEMA: u32 = 1;

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
}

/// The recorded green baseline: which tests passed and the content hashes of
/// their source files, both gated by the config fingerprint.
#[derive(Default)]
pub struct Baseline {
    pub green: HashSet<String>,
    pub test_file_hashes: HashMap<String, String>,
}

/// Hash the current content of the project's config files (order-stable), so a
/// change to any of them can bust the skip decision.
pub fn config_fingerprint(scope: &Path) -> String {
    let mut h = Sha256::new();
    for name in CONFIG_FILES {
        if let Some(sha) = current_sha256(&scope.join(name)) {
            h.update(name.as_bytes());
            h.update(sha.as_bytes());
        }
    }
    format!("{:x}", h.finalize())
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
        })
        .unwrap_or_default()
}

/// Persist the green set + per-test-file hashes + config fingerprint after a
/// run. Best-effort: a cache-write failure never fails the run.
pub fn record(scope: &Path, config_fp: &str, green: HashSet<String>) {
    let test_file_hashes = test_file_hashes(&green);
    let doc = Outcomes {
        schema: SCHEMA,
        config_fp: config_fp.to_string(),
        green,
        test_file_hashes,
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
    fn record_and_read_round_trip_with_config_gate() {
        let scope = std::env::temp_dir().join(format!("rstest-covskip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(&scope).unwrap();
        let green: HashSet<String> = ["t.py::test_a".to_string()].into_iter().collect();
        record(&scope, "cfg-A", green);
        assert!(load(&scope, "cfg-A").green.contains("t.py::test_a"));
        // A config change (different fingerprint) disables skipping.
        assert!(load(&scope, "cfg-B").green.is_empty());
    }
}
