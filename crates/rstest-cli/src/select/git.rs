//! Git-side inputs to selection: PR/MR base detection and the changed-file /
//! changed-line diff extraction (plus the pure `git diff -U0` hunk parser).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

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

#[cfg(test)]
mod tests {
    use super::{parse_diff_hunks, parse_hunk_old_range, FileChange};

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
}
