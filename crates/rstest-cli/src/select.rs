//! Smart test selection: changed files -> import graph -> affected tests.
//! Conservative by construction - every heuristic errs toward running MORE.
//! Known gap: dynamic imports (`importlib.import_module`, `__import__`) make no edges.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::cache;
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
    /// `origin/<name>` via merge-base - the PR fork point, not the base
    /// branch's post-fork commits.
    Branch { name: String, env: &'static str },
    /// An exact base SHA the CI already computed (GitLab's
    /// `CI_MERGE_REQUEST_DIFF_BASE_SHA` is the MR's diff base). Used
    /// directly - no merge-base call, and it survives a shallow clone.
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

/// Resolve the `--changed` base rev, PR-aware. Bare `--changed` diffs vs HEAD,
/// which silently skips everything on a CI PR checkout; auto-target the merge-base
/// with the detected PR base. An unresolvable base is an error, not a HEAD fallback.
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
        // --relative: paths relative to the CWD and limited to its subtree -
        // running from a repo subdirectory (or a monorepo project child) must
        // see ITS files, not repo-rooted paths.
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
    if !out.status.success() {
        bail!(
            "git ls-files --others failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
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

/// What changed in one file, from `git diff -U0`. The coverage index is keyed by
/// pre-change (old-side) line numbers, so `old_ranges` holds old-side spans of
/// modified/deleted lines; `has_new_code` flags code the index can't vouch for.
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
    // `git diff -U0` emits no hunks for files without a line-diff (deletions,
    // renames, binary, mode-only), which still affect selection. Union the
    // authoritative `--name-only` set; hunk-parsed keys win, the rest fall back.
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", "--relative", diff_base])
        .output()
        .context("running git diff --name-only")?;
    if !out.status.success() {
        bail!(
            "git diff --name-only {diff_base} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        map.entry(PathBuf::from(line)).or_insert(FileChange {
            old_ranges: Vec::new(),
            has_new_code: true,
        });
    }
    // Untracked files are all-new code with no old-side lines: mark
    // has_new_code so the caller falls back to import-graph for them.
    let out = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .context("running git ls-files")?;
    if !out.status.success() {
        bail!(
            "git ls-files --others failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
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
    // Only the header block before a file's first hunk names a file: under -U0 a
    // body line starting "++ " shows as "+++ " and must not be read as a header.
    // `diff --git` opens the header; `@@` sets `in_hunk`, only `diff --git` clears it.
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
        } else if !in_hunk {
            if let Some(rest) = line.strip_prefix("+++ ") {
                if let Some(c) = cur.take() {
                    out.push(c);
                }
                // `+++ b/path` (or `+++ /dev/null` for a deleted file).
                let path = rest.strip_prefix("b/").unwrap_or(rest);
                cur = (path != "/dev/null").then(|| (path.to_string(), FileChange::default()));
                continue;
            }
        }
        // A bare `@@` only ever starts a real hunk header - content lines are
        // prefixed with +/-/space, so this never collides with source.
        if line.starts_with("@@") {
            in_hunk = true;
            if let Some((_, change)) = cur.as_mut() {
                match parse_hunk_old_range(line) {
                    // Modified/deleted lines existed pre-change: look them up.
                    Some(Some(range)) => change.old_ranges.push(range),
                    // Pure insertion (`-a,0`): brand-new code, no old lines.
                    Some(None) => change.has_new_code = true,
                    None => {} // unparseable header - ignore
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
/// range. `Some(Some(range))` = lines changed/removed; `Some(None)` = `-a,0` pure
/// insertion (no old lines); `None` = the header didn't parse.
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
        return Some(None); // pure insertion at this point - no old-side lines
    }
    Some(Some((start, start + count - 1)))
}

/// Rule 1: a changed config file or any non-Python file defeats the import
/// graph - return a full run. Shared by the graph and coverage selectors.
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
    Some(format!("{:x}", h.finalize()))
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
    Some(format!("{:x}", h.finalize()))
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
                                let file_part = id.split("::").next().unwrap_or(id);
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
        let file_part = id.split("::").next().unwrap_or(&id);
        if !whole_files.contains(&abs(&cwd, Path::new(file_part))) {
            selected.insert(PathBuf::from(id));
        }
    }
    selected.extend(graph_tests);
    selected.extend(direct_tests);
    Ok(Selection::Tests(selected.into_iter().collect()))
}

/// Map changed files to the affected test files. `strict`: any changed source
/// file whose reverse import reach contains NO test file falls back to a full run
/// instead of silently selecting nothing (dynamic-import target, unused module).
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
        // is_test_file is a name-pattern match, so a DELETED test file still
        // matches; drop anything no longer on disk so pytest is never handed a
        // missing path (importers of a deleted source still exist and remain).
        .filter(|f| crate::collect::is_test_file(f, project) && f.exists())
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
/// conditional) imports - extra edges only ever widen the selection.
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
    use super::{
        diff_old_side, imports_of, normalize_newlines, parse_diff_hunks, parse_hunk_old_range,
        FileChange,
    };

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
diff --git a/gone.py b/gone.py
--- a/gone.py
+++ /dev/null
@@ -1,3 +0,0 @@
-a
-b
-c
diff --git a/keep.py b/keep.py
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
    fn diff_hunks_content_line_starting_with_plusplus_is_not_a_header() {
        // Source line "++x" (e.g. a C-ish idiom, or literal text) shows up as
        // "+++x" under -U0. Inside a hunk body it must NOT be read as a "+++ "
        // file header - the file stays pkg/mod.py, its one old line is recorded.
        let diff = "\
diff --git a/pkg/mod.py b/pkg/mod.py
--- a/pkg/mod.py
+++ b/pkg/mod.py
@@ -3 +3,2 @@ def f():
-    old
+++ not_a_file
+    new
";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].0, "pkg/mod.py");
        assert_eq!(
            hunks[0].1,
            FileChange {
                old_ranges: vec![(3, 3)],
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
