//! Smart test selection: changed files -> import graph -> affected tests.
//!
//! Conservative by construction — every heuristic errs toward running MORE:
//! - ambiguous module names select every matching file (suffix matching);
//! - function-local and conditional imports count as edges;
//! - a changed conftest.py selects its whole subtree;
//! - a changed pytest config file (or any non-Python file) aborts selection
//!   entirely -> full run.
//!
//! Known gap (documented): dynamic imports (`importlib.import_module`,
//! `__import__`) produce no edges. Teams relying on them should not use
//! `--changed` for correctness-critical runs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::ProjectConfig;

/// Why a full run is required instead of a selection.
pub enum Selection {
    /// Run only these test files (possibly empty: nothing affected).
    Tests(Vec<PathBuf>),
    /// A change defeats the graph (config/non-Python); run everything.
    FullRun(String),
}

/// How a CI exposes the PR/MR base for the current job.
enum CiBase {
    /// A base branch NAME (GitHub/GitLab/Buildkite). Resolved against
    /// `origin/<name>` via merge-base — the PR fork point, not the base
    /// branch's post-fork commits.
    Branch { name: String, env: &'static str },
    /// An exact base SHA the CI already computed (GitLab's
    /// `CI_MERGE_REQUEST_DIFF_BASE_SHA` is the MR's diff base). Used
    /// directly — no merge-base call, and it survives a shallow clone.
    Sha { sha: String, env: &'static str },
}

/// Detect the PR/MR base from the CI environment, if any. Probed in a
/// fixed order; the first CI whose variable is set wins. Returns `None`
/// off-CI or outside a PR job (bare `--changed` keeps diffing vs HEAD).
fn detect_ci_base() -> Option<CiBase> {
    // GitHub Actions: pull_request jobs set the base branch name.
    if let Some(name) = nonempty("GITHUB_BASE_REF") {
        return Some(CiBase::Branch {
            name,
            env: "GITHUB_BASE_REF",
        });
    }
    // GitLab CI merge-request pipelines: prefer the exact diff-base SHA
    // GitLab already resolved (no merge-base call, shallow-clone safe),
    // falling back to the target branch name.
    if let Some(sha) = nonempty("CI_MERGE_REQUEST_DIFF_BASE_SHA") {
        return Some(CiBase::Sha {
            sha,
            env: "CI_MERGE_REQUEST_DIFF_BASE_SHA",
        });
    }
    if let Some(name) = nonempty("CI_MERGE_REQUEST_TARGET_BRANCH_NAME") {
        return Some(CiBase::Branch {
            name,
            env: "CI_MERGE_REQUEST_TARGET_BRANCH_NAME",
        });
    }
    // Buildkite: set only on PR builds; literal "false" when not a PR.
    if let Some(name) = nonempty("BUILDKITE_PULL_REQUEST_BASE_BRANCH") {
        return Some(CiBase::Branch {
            name,
            env: "BUILDKITE_PULL_REQUEST_BASE_BRANCH",
        });
    }
    None
}

/// A non-empty, non-`"false"` env var value (Buildkite writes the literal
/// `false` rather than clearing the variable off PR builds).
fn nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() && v != "false" => Some(v),
        _ => None,
    }
}

/// Resolve the `--changed` base rev, PR-aware.
///
/// Bare `--changed` diffs vs HEAD — on a clean CI checkout of a PR that
/// selects NOTHING (silent full skip). Every supported CI exposes the
/// PR/MR base branch on its pull-request jobs (GitHub `GITHUB_BASE_REF`,
/// GitLab `CI_MERGE_REQUEST_*`, Buildkite `BUILDKITE_PULL_REQUEST_BASE_BRANCH`);
/// when one is set and no explicit rev was given, diff vs the merge-base
/// with that base instead: exactly the PR's files, not post-branch commits
/// on the base. (TeamCity has no standard env var for the target branch —
/// expose one as a build parameter to opt in.)
///
/// An unresolvable base ref is an error, not a fallback — the default
/// checkout is shallow (fetch-depth: 1) and falling back to HEAD would
/// skip every test while looking green.
pub fn resolve_base_rev(rev: &str) -> Result<String> {
    if rev != "HEAD" {
        return Ok(rev.to_string());
    }
    let (target, env) = match detect_ci_base() {
        // A CI-provided exact SHA is used verbatim; verify it's present so
        // a shallow clone fails loudly rather than skipping the whole suite.
        Some(CiBase::Sha { sha, env }) => {
            let out = std::process::Command::new("git")
                .args([
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{sha}^{{commit}}"),
                ])
                .output()
                .context("running git rev-parse")?;
            if !out.status.success() {
                bail!(
                    "--changed: {env} is '{sha}' but that commit is not in the local \
                     clone — fetch it first (`git fetch origin {sha}`, or check out with \
                     full history)"
                );
            }
            eprintln!(
                "rstest: --changed auto-targets MR base {} ({env})",
                &sha[..sha.len().min(12)]
            );
            return Ok(sha);
        }
        Some(CiBase::Branch { name, env }) => (name, env),
        None => return Ok(rev.to_string()),
    };
    let remote = format!("origin/{target}");
    let out = std::process::Command::new("git")
        .args(["merge-base", &remote, "HEAD"])
        .output()
        .context("running git merge-base")?;
    if !out.status.success() {
        bail!(
            "--changed: {env} is '{target}' but `git merge-base {remote} HEAD` \
             failed — fetch the base branch first (actions/checkout: `fetch-depth: 0`, \
             or `git fetch origin {target}`): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    eprintln!(
        "rstest: --changed auto-targets PR base {remote} (merge-base {})",
        &sha[..sha.len().min(12)]
    );
    Ok(sha)
}

pub fn changed_files_from_git(rev: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    let diff_base = rev.unwrap_or("HEAD");
    let out = std::process::Command::new("git")
        // --relative: paths relative to the CWD and limited to its
        // subtree — running from a repo subdirectory (or a monorepo
        // project child) must see ITS files, not repo-rooted paths.
        .args(["diff", "--name-only", "--relative", diff_base])
        .output()
        .context("running git diff (is this a git repository?)")?;
    if !out.status.success() {
        bail!(
            "git diff --name-only {diff_base} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        files.insert(PathBuf::from(line));
    }
    // Untracked files are changes too.
    let out = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .context("running git ls-files")?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        files.insert(PathBuf::from(line));
    }
    // Runner artifacts churn on every run and must not defeat selection
    // (users usually gitignore them, but don't rely on it).
    Ok(files
        .into_iter()
        .filter(|f| is_selectable_path(f))
        .collect())
}

/// A changed path that must not defeat selection: runner artifacts
/// (coverage data, caches) churn every run and are never test inputs.
fn is_selectable_path(f: &Path) -> bool {
    !f.components().any(|c| {
        matches!(
            c.as_os_str().to_str().unwrap_or(""),
            ".pytest_cache" | ".rstest_cache" | "__pycache__" | "htmlcov" | ".git"
        )
    }) && !f
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".coverage") || n == "coverage.xml")
}

/// What changed in one file, from `git diff -U0`, in the shape the coverage
/// index lookup needs.
///
/// The coverage index is keyed by **pre-change (old-side)** line numbers — it
/// was built from a past run's source — so we look up the OLD lines a hunk
/// touched, not the new ones (new-side numbers drift after insertions).
/// `old_ranges` are the inclusive old-side spans of modified/deleted lines
/// (lines that may have had coverage). `has_new_code` is set when the change
/// introduces lines with no old-side counterpart (a pure insertion, `-a,0`) or
/// the file is untracked — brand-new code the index cannot vouch for, so the
/// file needs the conservative import-graph fallback.
#[derive(Debug, Default, PartialEq)]
pub struct FileChange {
    pub old_ranges: Vec<(u32, u32)>,
    pub has_new_code: bool,
}

pub type ChangedLines = BTreeMap<PathBuf, FileChange>;

pub fn changed_line_ranges(rev: Option<&str>) -> Result<ChangedLines> {
    let diff_base = rev.unwrap_or("HEAD");
    let out = std::process::Command::new("git")
        // -U0: zero context lines, so every hunk's new-side range is exactly
        // the changed lines. --relative: paths relative to CWD (monorepo child
        // safety), matching changed_files_from_git and the index keys.
        .args(["diff", "-U0", "--relative", diff_base])
        .output()
        .context("running git diff -U0 (is this a git repository?)")?;
    if !out.status.success() {
        bail!(
            "git diff -U0 {diff_base} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut map: ChangedLines = parse_diff_hunks(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .map(|(path, change)| (PathBuf::from(path), change))
        .collect();
    // Untracked files are all-new code with no old-side lines: mark
    // has_new_code so the caller falls back to import-graph for them.
    let out = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .context("running git ls-files")?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        map.entry(PathBuf::from(line)).or_insert(FileChange {
            old_ranges: Vec::new(),
            has_new_code: true,
        });
    }
    map.retain(|f, _| is_selectable_path(f));
    Ok(map)
}

/// Parse `git diff -U0` output into (new-side path, FileChange). `/dev/null`
/// targets (deleted files) are dropped. Pure function over the diff text so it
/// is unit-testable.
fn parse_diff_hunks(diff: &str) -> Vec<(String, FileChange)> {
    let mut out: Vec<(String, FileChange)> = Vec::new();
    let mut cur: Option<(String, FileChange)> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            // `+++ b/path` (or `+++ /dev/null` for a deleted file).
            let path = rest.strip_prefix("b/").unwrap_or(rest);
            cur = (path != "/dev/null").then(|| (path.to_string(), FileChange::default()));
        } else if line.starts_with("@@") {
            if let Some((_, change)) = cur.as_mut() {
                match parse_hunk_old_range(line) {
                    // Modified/deleted lines existed pre-change: look them up.
                    Some(Some(range)) => change.old_ranges.push(range),
                    // Pure insertion (`-a,0`): brand-new code, no old lines.
                    Some(None) => change.has_new_code = true,
                    None => {} // unparseable header — ignore
                }
            }
        }
    }
    if let Some(c) = cur.take() {
        out.push(c);
    }
    // Keep files that either touched old lines or added new code.
    out.into_iter()
        .filter(|(_, c)| !c.old_ranges.is_empty() || c.has_new_code)
        .collect()
}

/// From an `@@ -a,b +c,d @@` hunk header, the OLD-side `(start, end)` inclusive
/// range. `Some(Some(range))` = `b > 0` lines existed and were changed/removed;
/// `Some(None)` = `-a,0`, a pure insertion (no old lines); `None` = the header
/// didn't parse.
fn parse_hunk_old_range(hunk: &str) -> Option<Option<(u32, u32)>> {
    // token after "@@": "-a" or "-a,b"
    let minus = hunk
        .split_whitespace()
        .nth(1)
        .filter(|t| t.starts_with('-'))?;
    let mut nums = minus.trim_start_matches('-').split(',');
    let start: u32 = nums.next()?.parse().ok()?;
    let count: u32 = match nums.next() {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    if count == 0 {
        return Some(None); // pure insertion at this point — no old-side lines
    }
    Some(Some((start, start + count - 1)))
}

/// Rule 1: a changed config file or any non-Python file defeats the import
/// graph — return a full run. Shared by the graph and coverage selectors.
fn rule1_full_run(changed: &[PathBuf]) -> Option<Selection> {
    for c in changed {
        let name = c.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(
            name,
            "pyproject.toml" | "pytest.ini" | "setup.cfg" | "tox.ini" | ".coveragerc"
        ) {
            return Some(Selection::FullRun(format!("{name} changed")));
        }
        if c.extension().and_then(|e| e.to_str()) != Some("py") {
            return Some(Selection::FullRun(format!(
                "non-Python file changed: {}",
                c.display()
            )));
        }
    }
    None
}

const COVERAGE_INDEX_SCHEMA: u32 = 1;

#[derive(serde::Deserialize)]
struct CoverageIndex {
    #[serde(default)]
    schema: u32,
    /// relative file path -> (line number -> nodeids that covered it)
    #[serde(default)]
    files: HashMap<String, HashMap<u32, Vec<String>>>,
}

/// Load `.rstest_cache/coverage_index.json`, or `None` when it is missing,
/// unreadable, corrupt, or a schema this build doesn't understand — every
/// "None" path makes the caller fall back to import-graph selection.
fn load_coverage_index() -> Option<CoverageIndex> {
    let bytes = std::fs::read(".rstest_cache/coverage_index.json").ok()?;
    let idx: CoverageIndex = serde_json::from_slice(&bytes).ok()?;
    (idx.schema == COVERAGE_INDEX_SCHEMA).then_some(idx)
}

/// Coverage-aware selection: consult the line->test index to pick the exact
/// tests whose recorded coverage executed the changed lines, falling back to
/// import-graph selection for anything the index can't vouch for (brand-new
/// code, files it never measured, untracked files) and to a full run for
/// non-Python/config changes. With no index (cold cache) it is byte-identical
/// to `affected_tests`. Over-selection is safe; the rails never under-select
/// against unknown code — but the index is trusted for lines it *did* record,
/// so keep it warm (rebuild on `--cov-context=test` runs).
pub fn affected_with_coverage(
    rootdir: &Path,
    project: &ProjectConfig,
    changes: &ChangedLines,
    strict: bool,
) -> Result<Selection> {
    let files: Vec<PathBuf> = changes.keys().cloned().collect();
    if let Some(full) = rule1_full_run(&files) {
        return Ok(full);
    }
    let Some(index) = load_coverage_index() else {
        // Cold cache: identical to the import-graph selector.
        return affected_tests(rootdir, project, &files, strict);
    };

    let mut nodeids: BTreeSet<String> = BTreeSet::new();
    let mut fallback: Vec<PathBuf> = Vec::new();
    let mut direct_tests: BTreeSet<PathBuf> = BTreeSet::new();

    for (file, change) in changes {
        // A changed test file always runs its own tests — its assertions or
        // fixtures may have changed regardless of what covers its lines.
        if crate::collect::is_test_file(&rootdir.join(file), project) {
            direct_tests.insert(file.clone());
            continue;
        }
        // conftest.py subtree semantics live in the graph path (its Rule 2).
        if file.file_name().and_then(|n| n.to_str()) == Some("conftest.py") {
            fallback.push(file.clone());
            continue;
        }
        // Look up the OLD-side changed lines (index is keyed pre-change).
        let key = file.to_string_lossy().replace('\\', "/");
        if let Some(lines) = index.files.get(&key) {
            for &(start, end) in &change.old_ranges {
                for line in start..=end {
                    if let Some(ids) = lines.get(&line) {
                        nodeids.extend(ids.iter().cloned());
                    }
                }
            }
        }
        // Brand-new code, or a file the index never measured, needs the graph.
        if change.has_new_code || !index.files.contains_key(&key) {
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

    let mut selected: BTreeSet<PathBuf> = BTreeSet::new();
    selected.extend(nodeids.into_iter().map(PathBuf::from));
    selected.extend(graph_tests);
    selected.extend(direct_tests);
    Ok(Selection::Tests(selected.into_iter().collect()))
}

/// Map changed files to the affected test files.
///
/// `strict`: refuse to skip on unprovable safety — any changed source
/// file whose reverse import reach contains NO test file (dynamic-import
/// target, unused module, or a file outside the graph) falls back to a
/// full run instead of silently selecting nothing for it.
pub fn affected_tests(
    rootdir: &Path,
    project: &ProjectConfig,
    changed: &[PathBuf],
    strict: bool,
) -> Result<Selection> {
    // Rule 1: anything that isn't a Python file defeats the graph.
    if let Some(full) = rule1_full_run(changed) {
        return Ok(full);
    }

    let index = ProjectIndex::build(rootdir)?;

    let mut affected: HashSet<PathBuf> = HashSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    for c in changed {
        let abs = rootdir.join(c);
        let canon = abs.canonicalize().unwrap_or(abs);
        // Rule 2: conftest.py affects every test below its directory.
        if canon.file_name().and_then(|n| n.to_str()) == Some("conftest.py") {
            if let Some(dir) = canon.parent() {
                for f in &index.files {
                    if f.starts_with(dir) {
                        affected.insert(f.clone());
                    }
                }
            }
            continue;
        }
        queue.push_back(canon);
    }

    // Strict: every changed SOURCE file must provably reach a test.
    // (Tests select themselves; conftest covers its subtree by rule 2.)
    if strict {
        for file in &queue {
            if crate::collect::is_test_file(file, project) {
                continue;
            }
            let mut reach: VecDeque<&PathBuf> = VecDeque::from([file]);
            let mut seen: HashSet<&PathBuf> = HashSet::from([file]);
            let mut covered = false;
            while let Some(f) = reach.pop_front() {
                if crate::collect::is_test_file(f, project) {
                    covered = true;
                    break;
                }
                if let Some(importers) = index.reverse.get(f) {
                    for imp in importers {
                        if seen.insert(imp) {
                            reach.push_back(imp);
                        }
                    }
                }
            }
            if !covered {
                return Ok(Selection::FullRun(format!(
                    "--changed-strict: {} reaches no tests via the import \
                     graph (dynamic import target, unused module, or deleted \
                     file) — running everything instead of risking a false skip",
                    file.strip_prefix(rootdir).unwrap_or(file).display()
                )));
            }
        }
    }

    // Reverse BFS over the import graph.
    let mut seen: HashSet<PathBuf> = queue.iter().cloned().collect();
    while let Some(file) = queue.pop_front() {
        affected.insert(file.clone());
        if let Some(importers) = index.reverse.get(&file) {
            for imp in importers {
                if seen.insert(imp.clone()) {
                    queue.push_back(imp.clone());
                }
            }
        }
    }

    let tests: Vec<PathBuf> = affected
        .into_iter()
        .filter(|f| crate::collect::is_test_file(f, project))
        .collect();
    let mut tests: Vec<PathBuf> = tests
        .into_iter()
        .map(|f| f.strip_prefix(rootdir).map(PathBuf::from).unwrap_or(f))
        .collect();
    tests.sort();
    Ok(Selection::Tests(tests))
}

struct ProjectIndex {
    files: Vec<PathBuf>,
    /// imported file -> files importing it
    reverse: HashMap<PathBuf, Vec<PathBuf>>,
}

impl ProjectIndex {
    fn build(rootdir: &Path) -> Result<Self> {
        // All project .py files (same pruning as the test-file walker).
        let mut files: Vec<PathBuf> = Vec::new();
        let walker = ignore::WalkBuilder::new(rootdir)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .filter_entry(|e| {
                let n = e.file_name().to_str().unwrap_or("");
                n != "__pycache__" && n != ".git" && !e.path().join("pyvenv.cfg").exists()
            })
            .build();
        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|t| t.is_file())
                && entry.path().extension().and_then(|e| e.to_str()) == Some("py")
            {
                files.push(
                    entry
                        .path()
                        .canonicalize()
                        .unwrap_or_else(|_| entry.into_path()),
                );
            }
        }

        // Module index: dotted path (relative to rootdir) -> file.
        // Lookup is by suffix, so src/ layouts and ambiguous short names
        // resolve to every candidate (over-selection is safe).
        let rootdir = rootdir
            .canonicalize()
            .unwrap_or_else(|_| rootdir.to_path_buf());
        let mut dotted: Vec<(String, PathBuf)> = Vec::new();
        for f in &files {
            let rel = f.strip_prefix(&rootdir).unwrap_or(f);
            let mut parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            if let Some(last) = parts.last_mut() {
                *last = last.trim_end_matches(".py").to_string();
            }
            if parts.last().map(String::as_str) == Some("__init__") {
                parts.pop();
            }
            dotted.push((parts.join("."), f.clone()));
        }
        let resolve = |module: &str| -> Vec<PathBuf> {
            let suffix = format!(".{module}");
            dotted
                .iter()
                .filter(|(d, _)| d == module || d.ends_with(&suffix))
                .map(|(_, f)| f.clone())
                .collect()
        };

        // Scan imports, build reverse edges.
        let mut reverse: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for f in &files {
            let Ok(src) = std::fs::read_to_string(f) else {
                continue;
            };
            let importer_dotted = dotted
                .iter()
                .find(|(_, p)| p == f)
                .map(|(d, _)| d.clone())
                .unwrap_or_default();
            for module in imports_of(&src, &importer_dotted) {
                for target in resolve(&module) {
                    if &target != f {
                        reverse.entry(target).or_default().push(f.clone());
                    }
                }
            }
        }
        Ok(Self { files, reverse })
    }
}

/// Modules imported by `src`. Includes indented (function-local /
/// conditional) imports — extra edges only ever widen the selection.
pub(crate) fn imports_of(src: &str, importer_dotted: &str) -> Vec<String> {
    let mut modules = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("import ") {
            for part in rest.split(',') {
                let m = part.split_whitespace().next().unwrap_or("");
                if !m.is_empty() {
                    modules.push(m.to_string());
                }
            }
        } else if let Some(rest) = t.strip_prefix("from ") {
            let Some((module_part, names)) = rest.split_once(" import ") else {
                continue;
            };
            let module_part = module_part.trim();
            let level = module_part.chars().take_while(|&c| c == '.').count();
            let named = &module_part[level..];
            let base = if level > 0 {
                // Relative: resolve against the importer's package.
                let mut pkg: Vec<&str> = importer_dotted.split('.').collect();
                for _ in 0..level {
                    pkg.pop();
                }
                let mut base = pkg.join(".");
                if !named.is_empty() {
                    if !base.is_empty() {
                        base.push('.');
                    }
                    base.push_str(named);
                }
                base
            } else {
                named.to_string()
            };
            if !base.is_empty() {
                modules.push(base.clone());
            }
            // `from pkg import x` may import the submodule pkg.x: add a
            // candidate per imported name (misses just don't resolve).
            for name in names.trim_start_matches('(').split(',') {
                let n = name
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(')');
                if !n.is_empty() && n != "*" && !base.is_empty() {
                    modules.push(format!("{base}.{n}"));
                }
            }
        }
    }
    modules
}

#[cfg(test)]
mod tests {
    use super::{imports_of, parse_diff_hunks, parse_hunk_old_range, FileChange};

    #[test]
    fn hunk_old_range_parsing() {
        // modification: old lines 1..2 changed
        assert_eq!(parse_hunk_old_range("@@ -1,2 +3,4 @@"), Some(Some((1, 2))));
        // single old line (no count)
        assert_eq!(
            parse_hunk_old_range("@@ -5 +5 @@ def foo():"),
            Some(Some((5, 5)))
        );
        // pure insertion (-a,0): no old-side lines
        assert_eq!(parse_hunk_old_range("@@ -0,0 +1,3 @@"), Some(None));
        // deletion: old lines 10..12 removed
        assert_eq!(
            parse_hunk_old_range("@@ -10,3 +9,0 @@"),
            Some(Some((10, 12)))
        );
    }

    #[test]
    fn diff_hunks_use_old_side_and_flag_insertions() {
        // Line 2 modified (old-side (2,2)); two lines inserted after b (-10,0
        // => pure insertion => has_new_code, no old range).
        let diff = "\
diff --git a/pkg/mod.py b/pkg/mod.py
index e69..abc 100644
--- a/pkg/mod.py
+++ b/pkg/mod.py
@@ -2 +2 @@ def a():
-    return 1
+    return 2
@@ -10,0 +11,2 @@ def b():
+    x = 1
+    return x
";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].0, "pkg/mod.py");
        assert_eq!(
            hunks[0].1,
            FileChange {
                old_ranges: vec![(2, 2)],
                has_new_code: true
            }
        );
    }

    #[test]
    fn diff_hunks_deletions_kept_dev_null_dropped() {
        let diff = "\
--- a/gone.py
+++ /dev/null
@@ -1,3 +0,0 @@
-a
-b
-c
--- a/keep.py
+++ b/keep.py
@@ -5,2 +5,0 @@
-old
-lines
";
        // gone.py -> /dev/null is dropped; keep.py deleted old lines 5..6, which
        // had coverage, so it's kept for an old-side index lookup.
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].0, "keep.py");
        assert_eq!(
            hunks[0].1,
            FileChange {
                old_ranges: vec![(5, 6)],
                has_new_code: false
            }
        );
    }

    #[test]
    fn plain_and_comma_imports() {
        let mods = imports_of("import os, mypkg.utils\nimport json as j\n", "tests.test_x");
        assert!(mods.contains(&"os".to_string()));
        assert!(mods.contains(&"mypkg.utils".to_string()));
        assert!(mods.contains(&"json".to_string()));
    }

    #[test]
    fn from_imports_add_submodule_candidates() {
        let mods = imports_of("from mypkg import utils, helpers\n", "tests.test_x");
        // the package itself AND each name as a possible submodule
        assert!(mods.contains(&"mypkg".to_string()));
        assert!(mods.contains(&"mypkg.utils".to_string()));
        assert!(mods.contains(&"mypkg.helpers".to_string()));
    }

    #[test]
    fn relative_imports_resolve_against_importer_package() {
        // tests/sub/test_a.py doing `from ..core import thing`
        let mods = imports_of("from ..core import thing\n", "tests.sub.test_a");
        assert!(mods.contains(&"tests.core".to_string()), "{mods:?}");
        assert!(mods.contains(&"tests.core.thing".to_string()));
        // `from . import sibling`
        let mods = imports_of("from . import sibling\n", "tests.sub.test_a");
        assert!(mods.contains(&"tests.sub.sibling".to_string()), "{mods:?}");
    }

    #[test]
    fn star_imports_keep_base_only() {
        let mods = imports_of("from mypkg.core import *\n", "tests.test_x");
        assert_eq!(mods, vec!["mypkg.core".to_string()]);
    }

    #[test]
    fn indented_imports_inside_functions_count() {
        // over-selection by design: function-local imports still create edges
        let mods = imports_of("def test_x():\n    import lazy_dep\n", "tests.test_x");
        assert!(mods.contains(&"lazy_dep".to_string()));
    }
}
