//! Git-side inputs to selection: PR/MR base detection and the changed-file /
//! changed-line diff extraction (plus the pure `git diff -U0` hunk parser).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::reporting::sink::Sink;

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

/// Run `git <args>` and return its stdout on success. A spawn failure is
/// contextualized; a non-zero exit is an error carrying the argv and git's
/// stderr. Callers wanting a bespoke hint wrap the error with `.with_context`.
pub(crate) fn git_stdout(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolve the `--changed` base rev, PR-aware. Bare `--changed` diffs vs HEAD,
/// which silently skips everything on a CI PR checkout; auto-target the merge-base
/// with the detected PR base. An unresolvable base is an error, not a HEAD fallback.
pub fn resolve_base_rev(rev: &str, sink: &mut Sink) -> Result<String> {
    if rev != "HEAD" {
        return Ok(rev.to_string());
    }
    let (target, env) = match detect_ci_base() {
        // A CI-provided exact SHA is used verbatim; verify it's present so
        // a shallow clone fails loudly rather than skipping the whole suite.
        Some(CiBase::Sha { sha, env }) => {
            git_stdout(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{sha}^{{commit}}"),
            ])
            .with_context(|| {
                format!(
                    "--changed: {env} is '{sha}' but that commit is not in the local \
                         clone — fetch it first (`git fetch origin {sha}`, or check out with \
                         full history)"
                )
            })?;
            sink.warn(&format!(
                "rstest: --changed auto-targets MR base {} ({env})",
                &sha[..sha.len().min(12)]
            ));
            return Ok(sha);
        }
        Some(CiBase::Branch { name, env }) => (name, env),
        None => return Ok(rev.to_string()),
    };
    let remote = format!("origin/{target}");
    let sha = git_stdout(&["merge-base", &remote, "HEAD"]).with_context(|| {
        format!(
            "--changed: {env} is '{target}' but `git merge-base {remote} HEAD` \
             failed — fetch the base branch first (actions/checkout: `fetch-depth: 0`, \
             or `git fetch origin {target}`)"
        )
    })?;
    let sha = sha.trim().to_string();
    sink.warn(&format!(
        "rstest: --changed auto-targets PR base {remote} (merge-base {})",
        &sha[..sha.len().min(12)]
    ));
    Ok(sha)
}

pub fn changed_files_from_git(rev: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    let diff_base = rev.unwrap_or("HEAD");
    // --relative: paths relative to the CWD and limited to its subtree -
    // running from a repo subdirectory (or a monorepo project child) must
    // see ITS files, not repo-rooted paths.
    let out = git_stdout(&["diff", "--name-only", "--relative", diff_base])?;
    for line in out.lines() {
        files.insert(PathBuf::from(line));
    }
    // Untracked files are changes too.
    let out = git_stdout(&["ls-files", "--others", "--exclude-standard"])?;
    for line in out.lines() {
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
    // -U0: zero context lines, so every hunk's new-side range is exactly
    // the changed lines. --relative: paths relative to CWD (monorepo child
    // safety), matching changed_files_from_git and the index keys.
    let out = git_stdout(&["diff", "-U0", "--relative", diff_base])?;
    let mut map: ChangedLines = parse_diff_hunks(&out)
        .into_iter()
        .map(|(path, change)| (PathBuf::from(path), change))
        .collect();
    // `git diff -U0` emits no hunks for files without a line-diff (deletions,
    // renames, binary, mode-only), which still affect selection. Union the
    // authoritative `--name-only` set; hunk-parsed keys win, the rest fall back.
    let out = git_stdout(&["diff", "--name-only", "--relative", diff_base])?;
    for line in out.lines() {
        map.entry(PathBuf::from(line)).or_insert(FileChange {
            old_ranges: Vec::new(),
            has_new_code: true,
        });
    }
    // Untracked files are all-new code with no old-side lines: mark
    // has_new_code so the caller falls back to import-graph for them.
    let out = git_stdout(&["ls-files", "--others", "--exclude-standard"])?;
    for line in out.lines() {
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
    use super::{
        changed_files_from_git, changed_line_ranges, detect_ci_base, nonempty, parse_diff_hunks,
        parse_hunk_old_range, CiBase, FileChange,
    };
    use crate::select::GLOBAL_TEST_LOCK as GLOBAL;
    use std::path::{Path, PathBuf};

    /// RAII: restore the original CWD (and hold the global lock) on drop, so a
    /// panicking test can't leave the process in a temp dir for its siblings.
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
        let d = std::env::temp_dir().join(format!("rstest-git-{name}-{}", std::process::id()));
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
    fn ci_base_priority_and_gitlab_target_branch() {
        let _lock = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        let keys = [
            "GITHUB_BASE_REF",
            "CI_MERGE_REQUEST_DIFF_BASE_SHA",
            "CI_MERGE_REQUEST_TARGET_BRANCH_NAME",
            "BUILDKITE_PULL_REQUEST_BASE_BRANCH",
        ];
        let saved: Vec<Option<String>> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        for k in keys {
            std::env::remove_var(k);
        }

        // Nothing set off-CI.
        assert!(detect_ci_base().is_none());

        // GitLab MR without the exact SHA falls to the target-branch name.
        std::env::set_var("CI_MERGE_REQUEST_TARGET_BRANCH_NAME", "main");
        match detect_ci_base() {
            Some(CiBase::Branch { name, env }) => {
                assert_eq!(name, "main");
                assert_eq!(env, "CI_MERGE_REQUEST_TARGET_BRANCH_NAME");
            }
            _ => panic!("expected target-branch base"),
        }

        // The exact diff-base SHA wins over the branch name.
        std::env::set_var("CI_MERGE_REQUEST_DIFF_BASE_SHA", "abc123");
        assert!(matches!(detect_ci_base(), Some(CiBase::Sha { .. })));

        // GITHUB_BASE_REF has top priority.
        std::env::set_var("GITHUB_BASE_REF", "trunk");
        match detect_ci_base() {
            Some(CiBase::Branch { name, env }) => {
                assert_eq!(name, "trunk");
                assert_eq!(env, "GITHUB_BASE_REF");
            }
            _ => panic!("expected GITHUB_BASE_REF branch"),
        }

        for (k, v) in keys.iter().zip(saved) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn nonempty_rejects_empty_and_literal_false() {
        let _lock = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        let key = "RSTEST_TEST_NONEMPTY";
        let saved = std::env::var(key).ok();
        std::env::set_var(key, "");
        assert_eq!(nonempty(key), None);
        std::env::set_var(key, "false");
        assert_eq!(nonempty(key), None);
        std::env::set_var(key, "main");
        assert_eq!(nonempty(key).as_deref(), Some("main"));
        match saved {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn changed_files_bails_on_unknown_rev() {
        let repo = init_repo("files-bail");
        write(&repo, "a.py", "x = 1\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let _cwd = enter(&repo);
        let err = changed_files_from_git(Some("no-such-ref-xyz")).unwrap_err();
        assert!(err.to_string().contains("git diff"), "{err}");
    }

    #[test]
    fn changed_line_ranges_bails_on_unknown_rev() {
        let repo = init_repo("lines-bail");
        write(&repo, "a.py", "x = 1\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let _cwd = enter(&repo);
        let err = changed_line_ranges(Some("no-such-ref-xyz")).unwrap_err();
        assert!(err.to_string().contains("git diff -U0"), "{err}");
    }

    /// Absolute path to the real `git`, found before we shadow it on PATH.
    #[cfg(unix)]
    fn real_git() -> PathBuf {
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let cand = dir.join("git");
            if cand.is_file() {
                return cand;
            }
        }
        panic!("no git on PATH");
    }

    /// A `bin/` dir holding a `git` shim that exits non-zero for any invocation
    /// whose argv contains `fail_arg`, and otherwise execs the real git. Used to
    /// force a SPECIFIC git subcommand to fail while its siblings still succeed.
    #[cfg(unix)]
    fn shim_git(dir: &Path, fail_arg: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let git = bin.join("git");
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = \"{fail_arg}\" ]; then\n    echo \"shim: forced failure on {fail_arg}\" >&2\n    exit 1\n  fi\ndone\nexec \"{real}\" \"$@\"\n",
            real = real_git().display(),
        );
        std::fs::write(&git, script).unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    #[test]
    fn bails_when_ls_files_fails_after_diff_succeeds() {
        let repo = init_repo("lsfiles-fail");
        write(&repo, "a.py", "x = 1\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let bin = shim_git(&repo, "ls-files");
        let _cwd = enter(&repo);
        let saved = std::env::var_os("PATH");
        let mut path = bin.clone().into_os_string();
        path.push(":");
        path.push(saved.clone().unwrap_or_default());
        std::env::set_var("PATH", &path);

        // diff succeeds (real git), ls-files is forced to fail.
        let e1 = changed_files_from_git(None).unwrap_err();
        // both diffs succeed, ls-files (the 3rd command) is forced to fail.
        let e2 = changed_line_ranges(None).unwrap_err();

        match saved {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        assert!(e1.to_string().contains("git ls-files --others"), "{e1}");
        assert!(e2.to_string().contains("git ls-files --others"), "{e2}");
    }

    #[cfg(unix)]
    #[test]
    fn changed_line_ranges_bails_when_name_only_diff_fails() {
        let repo = init_repo("nameonly-fail");
        write(&repo, "a.py", "x = 1\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let bin = shim_git(&repo, "--name-only");
        let _cwd = enter(&repo);
        let saved = std::env::var_os("PATH");
        let mut path = bin.clone().into_os_string();
        path.push(":");
        path.push(saved.clone().unwrap_or_default());
        std::env::set_var("PATH", &path);

        // diff -U0 succeeds; the follow-up `diff --name-only` is forced to fail.
        let err = changed_line_ranges(None).unwrap_err();

        match saved {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        assert!(err.to_string().contains("git diff --name-only"), "{err}");
    }

    #[test]
    fn changed_files_and_ranges_over_a_real_repo() {
        let repo = init_repo("real");
        write(&repo, "a.py", "def a():\n    return 1\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        // Modify a tracked file, add an untracked one, plus artifacts that must
        // be filtered out (is_selectable_path).
        write(&repo, "a.py", "def a():\n    return 2\n");
        write(&repo, "b.py", "def b():\n    return 3\n");
        write(&repo, ".coverage", "junk\n");
        write(&repo, "htmlcov/index.html", "<html>\n");

        let _cwd = enter(&repo);

        let files = changed_files_from_git(None).unwrap();
        assert!(files.contains(&PathBuf::from("a.py")), "{files:?}");
        assert!(files.contains(&PathBuf::from("b.py")), "{files:?}");
        assert!(!files.contains(&PathBuf::from(".coverage")), "{files:?}");
        assert!(!files.iter().any(|f| f.starts_with("htmlcov")), "{files:?}");

        let ranges = changed_line_ranges(None).unwrap();
        // Tracked modification: an old-side range recorded.
        let a = ranges.get(Path::new("a.py")).expect("a.py present");
        assert!(!a.old_ranges.is_empty(), "{a:?}");
        // Untracked file: all-new code, no old-side lines.
        let b = ranges.get(Path::new("b.py")).expect("b.py present");
        assert!(b.has_new_code && b.old_ranges.is_empty(), "{b:?}");
        assert!(!ranges.contains_key(Path::new(".coverage")), "{ranges:?}");
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
    fn diff_hunks_unparseable_header_is_ignored() {
        // A malformed `@@` header (no `-old` token) parses to None and is
        // silently ignored - the file is still tracked, just with no old range.
        // The trailing real hunk proves parsing recovers afterwards.
        let diff = "\
diff --git a/pkg/mod.py b/pkg/mod.py
--- a/pkg/mod.py
+++ b/pkg/mod.py
@@ +3 @@ garbage header
@@ -7,2 +7,2 @@ def f():
-old
+new
";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].0, "pkg/mod.py");
        // Only the well-formed second hunk contributed an old-side range.
        assert_eq!(hunks[0].1.old_ranges, vec![(7, 8)]);
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
