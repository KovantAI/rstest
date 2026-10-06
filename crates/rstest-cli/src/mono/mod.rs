//! Monorepo support, P0: discover subprojects (each with its own pytest
//! config) and run them as sequential session groups.
//!
//! pytest cannot run a repo of per-package configs from the root (one
//! rootdir/ini, colliding conftests). Here each project runs its own full
//! pool with cwd = project dir, so semantics and caches match pytest there.
//!
//! Split by concern:
//! - [`discover`] — find subprojects and name their per-project output files.
//! - [`plan`] — split the worker budget across concurrently-running projects.
//! - [`changes`] — classify which projects `--changed` must run (direct edits,
//!   dependents, unaffected) from declared + scanned inter-project edges.
//! - [`merge`] — fold per-project report-json into one root-relative document.

mod changes;
mod discover;
mod merge;
mod plan;

pub use changes::{classify_changes, project_python, ChangeImpact};
pub use discover::{discover_projects, slug, suffixed};
pub use merge::merge_reports;
pub use plan::{plan_shares_with_fixed, project_cost, project_fixed_n};
