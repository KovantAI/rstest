//! `--verify-vendor`: prove the installed vendored pytest tree is intact.
//!
//! Delegates to the worker package's offline verifier
//! (`rstest_worker._internal.verify_vendor`), run under the resolved
//! interpreter so it checks the `_vendor/` tree that is actually installed
//! alongside the worker (not a repo checkout). The Python side owns the file
//! set and hashing rules — one source of truth.

use std::path::Path;

use anyhow::Result;

/// Run the vendored-tree integrity check via the worker's Python module.
/// Returns the child's exit code (0 = intact, non-zero = drift or error).
pub fn run_verify(python: &Path) -> Result<i32> {
    let status = std::process::Command::new(python)
        .args(["-m", "rstest_worker._internal.verify_vendor"])
        .status()?;
    Ok(status.code().unwrap_or(1))
}
