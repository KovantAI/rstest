//! Incremental testing: run only what changed since the last GREEN run.
//!
//! `--since-green` reuses `--changed`'s coverage-aware selection, but supplies
//! the base rev itself: the git commit of the last fully-passing run, stored in
//! the cache. The working tree is diffed against that commit, so only tests
//! affected by changes since then are selected; everything else is provably
//! unaffected and skipped. The baseline advances ONLY on a green run, so a
//! failing test keeps being selected until it passes.
//!
//! Soundness: like `--changed`, this reasons over FIRST-PARTY source tracked by
//! git. An environment change invisible to git — a dependency upgraded in place
//! in site-packages — is not detected. After such a change, bust the baseline
//! (delete the cache file, or do one explicit `--changed`/full run).

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache;

/// Filename of the last-green baseline within the cache dir.
pub const FILE: &str = "last_green.json";

const SCHEMA: u32 = 2;

/// Dependency manifests whose content is folded into the environment
/// fingerprint. A change to any of these (a `uv lock`, an edited
/// `requirements.txt`) shifts the fingerprint and busts the baseline.
const LOCKFILES: [&str; 4] = ["uv.lock", "poetry.lock", "pdm.lock", "requirements.txt"];

#[derive(Serialize, Deserialize)]
struct Baseline {
    schema: u32,
    /// Commit the last fully-green run was at.
    sha: String,
    /// Environment fingerprint at that run (see [`env_fingerprint`]); a change
    /// busts the baseline, since unchanged first-party source can behave
    /// differently under a new interpreter / dependency set.
    fingerprint: String,
}

/// The commit to diff against for `--since-green`, or `None` when the caller
/// should run everything: no green run recorded yet, a schema bump, or — the
/// point of `fingerprint` — an environment change since the baseline was set.
pub fn baseline(scope: &Path, fingerprint: &str) -> Option<String> {
    let bytes = std::fs::read(cache::file_in(scope, FILE)).ok()?;
    let b: Baseline = serde_json::from_slice(&bytes).ok()?;
    (b.schema == SCHEMA && !b.sha.is_empty() && b.fingerprint == fingerprint).then_some(b.sha)
}

/// Record `sha` as the new green baseline, stamped with the current
/// `fingerprint`. Best-effort: a cache-write failure never fails the run
/// (worst case, the next run re-selects more than needed).
pub fn record_green(scope: &Path, sha: &str, fingerprint: &str) {
    let doc = Baseline {
        schema: SCHEMA,
        sha: sha.to_string(),
        fingerprint: fingerprint.to_string(),
    };
    if let Ok(bytes) = serde_json::to_vec(&doc) {
        let _ = cache::write_atomic(&cache::file_in(scope, FILE), &bytes);
    }
}

/// A hash of the test environment that git can't see: the resolved interpreter
/// (path + mtime + size) and the content of any dependency manifests under
/// `scope`. `--changed`-style source selection is blind to an in-place
/// dependency upgrade; folding this into the baseline makes such a change bust
/// it (a full run re-establishes), instead of a sticky false green.
pub fn env_fingerprint(scope: &Path, python: &Path) -> String {
    let mut h = Sha256::new();
    h.update(python.to_string_lossy().as_bytes());
    if let Ok(md) = std::fs::metadata(python) {
        h.update(md.len().to_le_bytes());
        if let Ok(mtime) = md.modified() {
            let secs = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            h.update(secs.to_le_bytes());
        }
    }
    for name in LOCKFILES {
        if let Ok(bytes) = std::fs::read(scope.join(name)) {
            h.update(name.as_bytes());
            h.update((bytes.len() as u64).to_le_bytes());
            h.update(&bytes);
        }
    }
    format!("{:x}", h.finalize())
}

/// The current `HEAD` commit sha, or `None` outside a git repo (or on an
/// unborn branch). `--since-green` needs a namable commit to record; without
/// one it degrades to a full run and records nothing.
pub fn head_sha() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-incr-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const FP: &str = "fp-A";

    #[test]
    fn baseline_absent_before_any_green_run() {
        let scope = tmp("absent");
        assert_eq!(baseline(&scope, FP), None);
    }

    #[test]
    fn record_then_read_round_trips() {
        let scope = tmp("round");
        record_green(&scope, "abc123", FP);
        assert_eq!(baseline(&scope, FP).as_deref(), Some("abc123"));
        // A later green run overwrites the baseline.
        record_green(&scope, "def456", FP);
        assert_eq!(baseline(&scope, FP).as_deref(), Some("def456"));
    }

    #[test]
    fn fingerprint_mismatch_busts_the_baseline() {
        // An environment change (dependency upgrade) shifts the fingerprint;
        // the stored sha must NOT be reused — the caller runs everything.
        let scope = tmp("fp");
        record_green(&scope, "abc123", "fp-A");
        assert_eq!(baseline(&scope, "fp-A").as_deref(), Some("abc123"));
        assert_eq!(baseline(&scope, "fp-B"), None, "changed env must bust");
    }

    #[test]
    fn schema_mismatch_reads_as_absent() {
        let scope = tmp("schema");
        let path = cache::file_in(&scope, FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"schema":999,"sha":"abc","fingerprint":"fp-A"}"#).unwrap();
        assert_eq!(baseline(&scope, FP), None);
    }

    #[test]
    fn empty_sha_reads_as_absent() {
        let scope = tmp("empty");
        record_green(&scope, "", FP);
        assert_eq!(baseline(&scope, FP), None);
    }

    #[test]
    fn env_fingerprint_reflects_interpreter_and_lockfiles() {
        let scope = tmp("envfp");
        let py = scope.join("python");
        std::fs::write(&py, b"#!fake\n").unwrap();
        let base = env_fingerprint(&scope, &py);
        // Same inputs -> stable.
        assert_eq!(base, env_fingerprint(&scope, &py));
        // A lockfile change moves the fingerprint.
        std::fs::write(scope.join("uv.lock"), b"a = 1\n").unwrap();
        let with_lock = env_fingerprint(&scope, &py);
        assert_ne!(base, with_lock, "adding a lockfile must change the fp");
        std::fs::write(scope.join("uv.lock"), b"a = 2\n").unwrap();
        assert_ne!(
            with_lock,
            env_fingerprint(&scope, &py),
            "editing lock changes fp"
        );
    }
}
