//! Guards against drift between the docs and a cheap, committed source of
//! truth. Two checks, both pure file scans — no python, no binary, always run:
//!
//! - `docs_version_matches_package_version`: every `rstest X.Y.Z` header in the
//!   docs/README must match `CARGO_PKG_VERSION` (the workspace version), so a
//!   release bump can't silently leave stale version strings in sample output.
//! - `corpus_project_count_matches_docs`: the "N well-known projects" claim in
//!   compatibility.md must match the standalone-suite count in
//!   `corpus/suites.toml` (total tables minus the monorepo entry).
//!
//! Test-RUN counts (pandas' 193,627 etc.) are deliberately NOT guarded: they
//! have no cheap committed source — deriving them means running the full
//! corpus, and freezing a baseline just moves the drift. See the docs review.

use std::path::{Path, PathBuf};

/// The version every doc sample must agree with.
const EXPECTED: &str = env!("CARGO_PKG_VERSION");

/// Workspace root, derived from this crate's manifest dir (`crates/rstest-cli`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Collect every `.md` file under `dir`, recursively.
fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

/// Find every version string immediately following a literal `rstest ` token,
/// i.e. the digits in `rstest 0.0.5`. Returns (line_number, version).
fn rstest_versions(text: &str) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let mut rest = line;
        while let Some(idx) = rest.find("rstest ") {
            let after = &rest[idx + "rstest ".len()..];
            let ver: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            // Require a dotted numeric like `1.2.3` (≥ two dots) to avoid
            // matching prose such as "rstest 8 workers".
            if ver.matches('.').count() >= 2 {
                hits.push((lineno + 1, ver));
            }
            rest = after;
        }
    }
    hits
}

#[test]
fn docs_version_matches_package_version() {
    let root = workspace_root();

    let mut files = Vec::new();
    markdown_files(&root.join("docs"), &mut files);
    let readme = root.join("README.md");
    if readme.exists() {
        files.push(readme);
    }
    assert!(!files.is_empty(), "no markdown files found under {root:?}");

    let mut mismatches = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for (lineno, ver) in rstest_versions(&text) {
            if ver != EXPECTED {
                let rel = file.strip_prefix(&root).unwrap_or(file);
                mismatches.push(format!("{}:{} — `rstest {ver}`", rel.display(), lineno));
            }
        }
    }

    assert!(
        mismatches.is_empty(),
        "doc samples reference a stale rstest version (expected `{EXPECTED}`):\n  {}\n\
         bump these to match Cargo.toml, or update the test if the format changed.",
        mismatches.join("\n  ")
    );
}

/// Count standalone suites in `suites.toml`: every top-level `[table]` header,
/// minus monorepo entries (those carry `mode = "mono"` and are counted
/// separately in the docs). Hand-parsed to avoid a toml dev-dependency — the
/// file is flat (no `[[arrays]]`, no nested tables).
fn standalone_suite_count(toml: &str) -> usize {
    let mut tables = 0usize;
    let mut mono = 0usize;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') && !t.starts_with("[[") {
            tables += 1;
        } else if t.starts_with("mode") {
            // `mode = "mono"` — strip whitespace/quotes and compare.
            if let Some((_, val)) = t.split_once('=') {
                if val.trim().trim_matches('"') == "mono" {
                    mono += 1;
                }
            }
        }
    }
    tables - mono
}

/// Pull the integer N from `... N well-known projects` in the given text.
/// Anchored on the `well-known projects` phrase (specific to this claim) and
/// reading the integer immediately preceding it, so unrelated prose containing
/// the common word "against" can't shift the match.
fn documented_project_count(text: &str) -> Option<usize> {
    let anchor = text.find("well-known projects")?;
    let before = text[..anchor].trim_end();
    let digits: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().ok()
}

#[test]
fn corpus_project_count_matches_docs() {
    let root = workspace_root();

    let suites = std::fs::read_to_string(root.join("corpus").join("suites.toml")).unwrap();
    let expected = standalone_suite_count(&suites);

    let compat =
        std::fs::read_to_string(root.join("docs").join("concepts").join("compatibility.md"))
            .unwrap();
    let documented = documented_project_count(&compat)
        .expect("compatibility.md should say `against N well-known projects`");

    assert_eq!(
        documented, expected,
        "compatibility.md claims {documented} well-known projects, but suites.toml \
         has {expected} standalone suites (total tables minus monorepo entries). \
         Update the doc count, or the parsing here if suites.toml's shape changed."
    );
}
