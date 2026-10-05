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
pub use git::{changed_files_from_git, changed_line_ranges, changed_new_lines, resolve_base_rev};
pub use graph::{affected_tests_cached, CollectionCache};
// pub(crate) helpers reused elsewhere in the crate (not part of the public API).
pub(crate) use coverage::current_sha256;
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
        let up = |s: &str| PathBuf::from("..").join(s).to_string_lossy().into_owned();
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
