use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

use crate::config::ProjectConfig;

/// Walk `paths` for test files per the project's `python_files` patterns.
///
/// Used for partitioning across workers (and `--collect-only`); semantic
/// collection (which tests live inside each file) stays with the vendored
/// core in the worker. Rule fidelity spec: research spike 1.
pub fn collect_test_files(paths: &[PathBuf], cfg: &ProjectConfig) -> Result<Vec<PathBuf>> {
    let roots: Vec<PathBuf> = if !paths.is_empty() {
        paths.to_vec()
    } else if !cfg.testpaths.is_empty() {
        cfg.testpaths.iter().map(|t| cfg.rootdir.join(t)).collect()
    } else {
        vec![cfg.rootdir.clone()]
    };

    let mut files = Vec::new();
    for root in &roots {
        if root.is_file() {
            if is_test_file(root, cfg) {
                files.push(root.clone());
            }
            continue;
        }
        // ignore's walker skips hidden dirs by default (matches pytest's `.*`
        // pruning); gitignore semantics are NOT pytest's, so disable them.
        let walker = WalkBuilder::new(root)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .filter_entry(|e| e.file_name() != "__pycache__" && !is_virtualenv(e.path()))
            .build();
        for entry in walker {
            let entry = entry?;
            if entry.file_type().is_some_and(|t| t.is_file()) && is_test_file(entry.path(), cfg) {
                files.push(entry.into_path());
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

pub fn is_test_file(path: &Path, cfg: &ProjectConfig) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".py") && cfg.python_files.iter().any(|pat| glob_match(pat, name))
}

/// fnmatch subset: `*` and `?` (pytest's python_files patterns use no more).
pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    fn rec(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => rec(&p[1..], n) || (!n.is_empty() && rec(p, &n[1..])),
            (Some('?'), Some(_)) => rec(&p[1..], &n[1..]),
            (Some(c), Some(d)) if c == d => rec(&p[1..], &n[1..]),
            _ => false,
        }
    }
    rec(&p, &n)
}

fn is_virtualenv(path: &Path) -> bool {
    path.join("pyvenv.cfg").exists()
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn globs() {
        assert!(glob_match("test_*.py", "test_foo.py"));
        assert!(glob_match("*_test.py", "foo_test.py"));
        assert!(glob_match("tests.py", "tests.py"));
        assert!(!glob_match("test_*.py", "foo_test.py"));
        assert!(!glob_match("tests.py", "tests_extra.py"));
    }
}
