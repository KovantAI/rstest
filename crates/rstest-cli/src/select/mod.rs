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

use std::path::PathBuf;

pub use coverage::{
    affected_with_coverage, CoverageFile, CoverageIndex, COVERAGE_INDEX_FILE, COVERAGE_INDEX_SCHEMA,
};
pub use git::{changed_files_from_git, changed_line_ranges, resolve_base_rev};
pub use graph::affected_tests;
// pub(crate) helpers reused elsewhere in the crate (not part of the public API).
pub(crate) use coverage::current_sha256;
pub(crate) use graph::imports_of;

/// Process-wide lock shared by every `select` unit test that mutates
/// process-global state (the CWD, or env vars git reads). The submodule test
/// suites live in separate files, so a per-module mutex wouldn't serialize a
/// CWD change in `git` against one in `coverage` — they must share this one.
#[cfg(test)]
pub(crate) static GLOBAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Why a full run is required instead of a selection.
pub enum Selection {
    /// Run only these test files (possibly empty: nothing affected).
    Tests(Vec<PathBuf>),
    /// A change defeats the graph (config/non-Python); run everything.
    FullRun(String),
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
