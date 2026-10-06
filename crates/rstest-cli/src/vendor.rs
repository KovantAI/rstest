//! `verify-vendor`: prove the installed vendored pytest tree is intact.
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
    // Same PYTHONPATH resolution as every other worker spawn (probe, pool):
    // otherwise `-m rstest_worker...` fails to import under the dev/
    // RSTEST_WORKER_PATH layout where the package is not pip-installed.
    let status = verify_command(python).status()?;
    Ok(status.code().unwrap_or(1))
}

/// The verify-vendor child, built without spawning it. It runs the project's
/// interpreter (whose startup executes the environment's `.pth` files), so it
/// gets the same secret scrubbing as a test worker.
fn verify_command(python: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(python);
    cmd.args(["-m", "rstest_worker._internal.verify_vendor"])
        .env("PYTHONPATH", crate::scheduling::worker::worker_pythonpath());
    crate::scheduling::worker::scrub_secrets(&mut cmd);
    cmd
}

#[cfg(test)]
mod tests {
    use super::verify_command;

    #[test]
    fn verify_command_scrubs_the_remote_cache_token() {
        let cmd = verify_command(std::path::Path::new("python"));
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == "RSTEST_CACHE_REMOTE_TOKEN" && v.is_none()),
            "verify-vendor inherits RSTEST_CACHE_REMOTE_TOKEN"
        );
    }
}
