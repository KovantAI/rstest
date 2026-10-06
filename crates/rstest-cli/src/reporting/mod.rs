//! Reporting: turning the stream of worker reports into human- and
//! machine-readable output. The run/result model and JSON (`report`), merged
//! junitxml (`junit`), live progress line (`progress`), per-worker status
//! footer (`status`), the ANSI palette (`color`), cross-run flake history
//! (`flakes`), CI-platform annotations (`ci`: GitHub/Azure/Buildkite), and the
//! self-contained HTML report (`html`).

pub mod ci;
pub mod color;
pub mod flakes;
pub mod html;
pub mod junit;
pub mod progress;
pub mod report;
pub mod sink;
pub mod status;

use std::path::Path;

use anyhow::Context;

/// Write a user-requested output file (`--junitxml`, `--report-json`, `--html`,
/// the doctor/migrate JSON docs). Creates missing parent directories the way
/// pytest does for `--junitxml`, so `test-results/junit.xml` works on a fresh
/// checkout, and names the path when the write still fails.
pub fn write_output(path: &Path, contents: impl AsRef<[u8]>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::write_output;

    #[test]
    fn write_output_creates_missing_parent_dirs() {
        let root = std::env::temp_dir().join(format!("rstest-write-output-{}", std::process::id()));
        let path = root.join("test-results").join("junit.xml");
        write_output(&path, b"<x/>").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"<x/>");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_output_error_names_the_path() {
        let blocker =
            std::env::temp_dir().join(format!("rstest-write-output-file-{}", std::process::id()));
        std::fs::write(&blocker, b"").unwrap();
        let path = blocker.join("report.json");
        let err = format!("{:#}", write_output(&path, b"{}").unwrap_err());
        assert!(err.contains(&blocker.display().to_string()), "got: {err}");
        let _ = std::fs::remove_file(&blocker);
    }
}
