//! Smart test selection: changed files -> import graph -> affected tests.
//! Conservative by construction - every heuristic errs toward running MORE.
//! Known gap: dynamic imports (`importlib.import_module`, `__import__`) make no edges.
//!
//! Split by concern: [`git`] extracts the changed files / lines from git,
//! [`graph`] does reverse-import-graph selection, [`coverage`] does
//! line->test coverage-index selection (falling back to the graph). The
//! shared [`Selection`] result and Rule 1 (`rule1_full_run`) live here.

mod coverage;
mod git;
mod graph;

use std::path::{Path, PathBuf};

pub use coverage::{
    affected_with_coverage, load_coverage_index, mapped_test_count, CoverageFile, CoverageIndex,
    COVERAGE_INDEX_FILE, COVERAGE_INDEX_SCHEMA,
};
pub use git::{
    changed_files_from_git, changed_line_ranges, changed_new_lines, git_toplevel, resolve_base_rev,
};
pub use graph::{affected_tests_cached, CollectionCache};
// pub(crate) helpers reused elsewhere in the crate (not part of the public API).
pub(crate) use coverage::current_sha256;
pub(crate) use git::{git_stdout, git_stdout_in};
pub(crate) use graph::{import_closures, imports_of};

/// Why a full run is required instead of a selection.
pub enum Selection {
    /// Run only these test files (possibly empty: nothing affected).
    Tests(Vec<PathBuf>),
    /// A change defeats the graph (config/non-Python); run everything.
    FullRun(String),
}

/// Selected targets (rootdir-relative files or `file::test` nodeids) as
/// pytest arguments for a session started in `cwd`: pytest resolves path
/// arguments against its invocation dir, which is a subdirectory of the
/// rootdir when rstest is started from one. Unchanged when `cwd` is the
/// rootdir.
pub fn targets_as_args(rootdir: &Path, cwd: &Path, targets: &[PathBuf]) -> Vec<String> {
    let canon = |p: &Path| crate::text::strip_verbatim(p.canonicalize().unwrap_or(p.to_path_buf()));
    let (root, cwd) = (canon(rootdir), canon(cwd));
    targets
        .iter()
        .map(|t| {
            let t = t.to_string_lossy();
            let file = crate::text::nodeid_file(&t);
            let rest = &t[file.len()..];
            if root == cwd {
                return t.into_owned();
            }
            let abs = root.join(file);
            let rel = crate::scheduling::durations::relative_to(&abs, &cwd).unwrap_or(abs);
            format!("{}{rest}", rel.display())
        })
        .collect()
}

/// [`near_changes`] tiers, closest first: a test file edited in the working
/// tree, a test file changed since the base, a test that reaches a changed
/// source file (coverage index, else import graph).
pub const NEAR_DIRTY_TEST: u8 = 0;
pub const NEAR_TEST: u8 = 1;
pub const NEAR_AFFECTED: u8 = 2;

/// How close each test is to the code that changed since `base` (`None` =
/// HEAD; the working tree is always included), for `--order fail-fast`. Keys
/// are rootdir-relative test files, or `file::test` nodeids where the coverage
/// index is that precise; values are the `NEAR_*` tiers. This ranks, it never
/// selects: non-Python changes are ignored rather than forcing a full run, and
/// any failure (no git, a base that no longer exists) yields an empty map.
pub fn near_changes(
    rootdir: &Path,
    project: &crate::config::ProjectConfig,
    base: Option<&str>,
) -> std::collections::HashMap<String, u8> {
    let mut near = std::collections::HashMap::new();
    let Ok(mut changes) = changed_line_ranges(rootdir, base) else {
        return near;
    };
    changes.retain(|f, _| f.extension().and_then(|e| e.to_str()) == Some("py"));
    if changes.is_empty() {
        return near;
    }
    let key = |p: &Path| p.to_string_lossy().replace('\\', "/");
    if let Ok(Selection::Tests(tests)) =
        affected_with_coverage(rootdir, project, &changes, false, base)
    {
        near.extend(tests.iter().map(|t| (key(t), NEAR_AFFECTED)));
    }
    // Against HEAD every change is a working-tree change.
    let dirty: std::collections::HashSet<PathBuf> = match base {
        Some(_) => changed_files_from_git(rootdir, None)
            .unwrap_or_default()
            .into_iter()
            .collect(),
        None => changes.keys().cloned().collect(),
    };
    for f in changes.keys() {
        let abs = rootdir.join(f);
        if crate::collect::is_test_file(&abs, project) && abs.exists() {
            let tier = if dirty.contains(f) {
                NEAR_DIRTY_TEST
            } else {
                NEAR_TEST
            };
            near.insert(key(f), tier);
        }
    }
    near
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

#[cfg(test)]
mod tests {
    use super::targets_as_args;
    use std::path::PathBuf;

    #[test]
    fn near_changes_tiers_edited_tests_and_affected_tests() {
        use super::{near_changes, NEAR_AFFECTED, NEAR_DIRTY_TEST, NEAR_TEST};
        use std::collections::HashMap;
        let held = crate::test_env::lock();
        let root = std::env::temp_dir().join(format!("rstest-near-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tests")).unwrap();
        let root = root.canonicalize().unwrap();
        // Cold coverage index: graph selection.
        let _cache = crate::test_env::set_var(&held, "RSTEST_CACHE", root.join(".cache"));
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        let write = |rel: &str, body: &str| std::fs::write(root.join(rel), body).unwrap();
        write("pytest.ini", "[pytest]\n");
        write(".gitignore", ".cache/\n");
        write("core.py", "X = 1\n");
        write("tests/test_core.py", "import core\ndef test_c(): pass\n");
        write("tests/test_edit.py", "def test_e(): pass\n");
        write("tests/test_other.py", "def test_o(): pass\n");
        write("README.md", "hi\n");
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let project = crate::config::discover(&root, &mut std::io::sink());
        let near = |base| near_changes(&root, &project, base);

        // Working tree vs HEAD. A README edit must not void the ranking the
        // way it forces --changed into a full run.
        write("core.py", "X = 2\n");
        write("tests/test_edit.py", "def test_e(): assert 1\n");
        write("README.md", "bye\n");
        assert_eq!(
            near(None),
            HashMap::from([
                ("tests/test_core.py".to_string(), NEAR_AFFECTED),
                ("tests/test_edit.py".to_string(), NEAR_DIRTY_TEST),
            ])
        );

        // Against an older base: committed test edits rank below the one
        // being edited now.
        git(&["commit", "-qam", "edits"]);
        write("tests/test_other.py", "def test_o(): assert 1\n");
        assert_eq!(
            near(Some("HEAD~1")),
            HashMap::from([
                ("tests/test_core.py".to_string(), NEAR_AFFECTED),
                ("tests/test_edit.py".to_string(), NEAR_TEST),
                ("tests/test_other.py".to_string(), NEAR_DIRTY_TEST),
            ])
        );

        // A base that no longer resolves is no signal, not an error.
        assert!(near(Some("0000000000000000000000000000000000000000")).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn targets_are_re_anchored_at_the_cwd() {
        let root = std::env::temp_dir().join(format!("rstest-select-args-{}", std::process::id()));
        std::fs::create_dir_all(root.join("q")).unwrap();
        let root = root.canonicalize().unwrap();
        let targets = vec![
            PathBuf::from("tests/test_q.py"),
            PathBuf::from("tests/test_q.py::test_a[x::y]"),
        ];
        // From the rootdir: unchanged.
        assert_eq!(
            targets_as_args(&root, &root, &targets),
            vec!["tests/test_q.py", "tests/test_q.py::test_a[x::y]"]
        );
        // From a subdirectory: relative to it, the nodeid suffix kept whole.
        // Join per component so the expected separators are native on Windows.
        let up = |s: &str| {
            s.split('/')
                .fold(PathBuf::from(".."), |p, c| p.join(c))
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(
            targets_as_args(&root, &root.join("q"), &targets),
            vec![
                up("tests/test_q.py"),
                format!("{}::test_a[x::y]", up("tests/test_q.py"))
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
