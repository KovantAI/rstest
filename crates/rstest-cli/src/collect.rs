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
    let mut files = Vec::new();
    walk_py_files(paths, cfg, |path| {
        if is_test_file(path, cfg) {
            files.push(path.to_path_buf());
        }
        false
    })?;
    files.sort();
    files.dedup();
    Ok(files)
}

/// Whether the walk `collect_test_files` does sees any `.py` file at all,
/// test-named or not (a `--doctest-modules` suite has no `test_*.py`).
/// Stops at the first hit.
pub fn has_python_files(paths: &[PathBuf], cfg: &ProjectConfig) -> Result<bool> {
    let mut found = false;
    walk_py_files(paths, cfg, |_| {
        found = true;
        true
    })?;
    Ok(found)
}

/// Visit every `.py` file under the collection roots (explicit paths, else
/// `testpaths`, else rootdir). `visit` returns true to stop the walk early.
fn walk_py_files(
    paths: &[PathBuf],
    cfg: &ProjectConfig,
    mut visit: impl FnMut(&Path) -> bool,
) -> Result<()> {
    let roots: Vec<PathBuf> = if !paths.is_empty() {
        paths.to_vec()
    } else {
        // pytest globs each `testpaths` entry (recursive `**` too) and falls
        // back to the rootdir when none matches anything.
        let expanded: Vec<PathBuf> = cfg
            .testpaths
            .iter()
            .flat_map(|t| expand_glob(&cfg.rootdir, t))
            .collect();
        if expanded.is_empty() {
            vec![cfg.rootdir.clone()]
        } else {
            expanded
        }
    };
    let norecurse = norecursedirs(cfg);

    for root in &roots {
        if root.is_file() {
            if is_py(root) && visit(root) {
                return Ok(());
            }
            continue;
        }
        // pytest's own recursion rules, not ripgrep's: no hidden-file,
        // `.ignore` or `.gitignore` filtering. Directories below a root are
        // pruned by `norecursedirs` (whose default `.*` covers hidden dirs);
        // a root itself is never pruned, as pytest collects explicit args.
        let norecurse = norecurse.clone();
        let walker = WalkBuilder::new(root)
            .standard_filters(false)
            .filter_entry(move |e| {
                if e.file_name() == "__pycache__" {
                    return false;
                }
                if e.depth() == 0 || !e.file_type().is_some_and(|t| t.is_dir()) {
                    return true;
                }
                !is_virtualenv(e.path()) && !norecurse.iter().any(|p| fnmatch_ex(p, e.path()))
            })
            .build();
        for entry in walker {
            let entry = entry?;
            if entry.file_type().is_some_and(|t| t.is_file())
                && is_py(entry.path())
                && visit(entry.path())
            {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// pytest's `norecursedirs` ini value, or its built-in default list.
fn norecursedirs(cfg: &ProjectConfig) -> Vec<String> {
    cfg.extra
        .iter()
        .find(|(k, _)| k == "norecursedirs")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| {
            [
                "*.egg",
                ".*",
                "_darcs",
                "build",
                "CVS",
                "dist",
                "node_modules",
                "venv",
                "{arch}",
            ]
            .map(String::from)
            .to_vec()
        })
}

/// pytest's `fnmatch_ex`: a pattern without a path separator matches the
/// base name; one with a separator matches the whole path (`*/`-anchored).
fn fnmatch_ex(pattern: &str, path: &Path) -> bool {
    let pattern = pattern.replace('\\', "/");
    if !pattern.contains('/') {
        return path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| glob_match(&pattern, n));
    }
    let full = path.to_string_lossy().replace('\\', "/");
    let pattern = if path.is_absolute() && !pattern.starts_with('/') {
        format!("*/{pattern}")
    } else {
        pattern
    };
    glob_match(&pattern, &full)
}

/// Python's `glob.iglob(pattern, recursive=True)` relative to `base`, sorted:
/// `*`/`?` per path component (never matching a leading `.` unless the
/// component starts with one), `**` for zero or more directories. A pattern
/// without wildcards yields itself only when it exists.
fn expand_glob(base: &Path, pattern: &str) -> Vec<PathBuf> {
    use std::path::Component;
    let mut start = base.to_path_buf();
    let mut parts: Vec<&str> = Vec::new();
    for c in Path::new(pattern).components() {
        match c {
            // Absolute: drop `base` (joining an absolute path replaces it).
            Component::Prefix(_) | Component::RootDir => start.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => parts.push(".."),
            Component::Normal(p) => parts.extend(p.to_str()),
        }
    }
    let mut out = Vec::new();
    glob_rec(&start, &parts, &mut out);
    out.sort();
    out.dedup();
    out
}

fn glob_rec(dir: &Path, parts: &[&str], out: &mut Vec<PathBuf>) {
    let Some((part, rest)) = parts.split_first() else {
        if dir.exists() {
            out.push(dir.to_path_buf());
        }
        return;
    };
    if !part.contains(['*', '?']) {
        return glob_rec(&dir.join(part), rest, out);
    }
    let mut children: Vec<(String, bool)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            Some((name, e.path().is_dir()))
        })
        .filter(|(name, _)| !name.starts_with('.') || part.starts_with('.'))
        .collect();
    children.sort();
    if *part == "**" {
        // Zero directories, then each subdirectory recursively.
        glob_rec(dir, rest, out);
        for (name, is_dir) in children {
            if is_dir {
                glob_rec(&dir.join(name), parts, out);
            }
        }
        return;
    }
    for (name, is_dir) in children {
        if glob_match(part, &name) && (rest.is_empty() || is_dir) {
            glob_rec(&dir.join(name), rest, out);
        }
    }
}

fn is_py(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "py")
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
    use super::{collect_test_files, expand_glob, fnmatch_ex, glob_match, has_python_files};
    use crate::config::ProjectConfig;

    fn project(name: &str) -> ProjectConfig {
        let root =
            std::env::temp_dir().join(format!("rstest-collect-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        ProjectConfig {
            rootdir: root,
            ..ProjectConfig::default()
        }
    }

    #[test]
    fn has_python_files_sees_any_py_but_not_virtualenvs() {
        let cfg = project("haspy");
        assert!(!has_python_files(&[], &cfg).unwrap(), "empty folder");
        std::fs::write(cfg.rootdir.join("README.md"), "").unwrap();
        let venv = cfg.rootdir.join(".venv/lib");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(cfg.rootdir.join(".venv/pyvenv.cfg"), "").unwrap();
        std::fs::write(venv.join("site.py"), "").unwrap();
        assert!(
            !has_python_files(&[], &cfg).unwrap(),
            "only a venv and non-py files"
        );
        // Not test-named: a --doctest-modules suite still counts.
        std::fs::write(cfg.rootdir.join("mod.py"), "").unwrap();
        assert!(has_python_files(&[], &cfg).unwrap());
        let _ = std::fs::remove_dir_all(&cfg.rootdir);
    }

    fn touch(cfg: &ProjectConfig, rel: &str) {
        let p = cfg.rootdir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    fn walked(cfg: &ProjectConfig) -> Vec<String> {
        collect_test_files(&[], cfg)
            .unwrap()
            .iter()
            .map(|p| {
                p.strip_prefix(&cfg.rootdir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn walk_follows_pytest_recursion_not_ripgrep_filters() {
        let mut cfg = project("norecurse");
        touch(&cfg, "tests/test_a.py");
        touch(&cfg, "tests/.hidden/test_h.py");
        touch(&cfg, "legacy/test_old.py");
        touch(&cfg, "node_modules/test_n.py");
        touch(&cfg, "b/test_b.py");
        std::fs::write(cfg.rootdir.join(".ignore"), "b/\n").unwrap();
        std::fs::write(cfg.rootdir.join(".gitignore"), "b/\n").unwrap();
        // pytest's default norecursedirs prunes `.*` and `node_modules`; a
        // `.ignore` / `.gitignore` means nothing to pytest.
        assert_eq!(
            walked(&cfg),
            ["b/test_b.py", "legacy/test_old.py", "tests/test_a.py"]
        );
        // Overriding norecursedirs replaces the defaults (hidden dirs recurse).
        cfg.extra
            .push(("norecursedirs".into(), vec!["legacy".into()]));
        assert_eq!(
            walked(&cfg),
            [
                "b/test_b.py",
                "node_modules/test_n.py",
                "tests/.hidden/test_h.py",
                "tests/test_a.py"
            ]
        );
        let _ = std::fs::remove_dir_all(&cfg.rootdir);
    }

    #[test]
    fn testpaths_are_globbed_like_pytest() {
        let mut cfg = project("globpaths");
        touch(&cfg, "pkgs/one/tests/test_1.py");
        touch(&cfg, "pkgs/two/tests/test_2.py");
        touch(&cfg, "pkgs/.dot/tests/test_d.py");
        touch(&cfg, "other/test_x.py");
        cfg.testpaths = vec!["pkgs/*/tests".into(), "missing".into()];
        assert_eq!(
            walked(&cfg),
            ["pkgs/one/tests/test_1.py", "pkgs/two/tests/test_2.py"]
        );
        let one = cfg.rootdir.join("pkgs/one/tests");
        assert_eq!(expand_glob(&cfg.rootdir, "pkgs/**/tests")[0], one);
        // Nothing matches: pytest falls back to the rootdir.
        cfg.testpaths = vec!["nope/*".into()];
        assert_eq!(walked(&cfg).len(), 3);
        let _ = std::fs::remove_dir_all(&cfg.rootdir);
    }

    #[test]
    fn fnmatch_ex_matches_basename_or_anchored_path() {
        use std::path::Path;
        assert!(fnmatch_ex(".*", Path::new("/r/tests/.hidden")));
        assert!(!fnmatch_ex(".*", Path::new("/r/.x/tests")));
        assert!(fnmatch_ex("*.egg", Path::new("/r/foo.egg")));
        assert!(fnmatch_ex("tests/data", Path::new("/r/tests/data")));
        assert!(!fnmatch_ex("tests/data", Path::new("/r/tests/datum")));
    }

    #[test]
    fn globs() {
        assert!(glob_match("test_*.py", "test_foo.py"));
        assert!(glob_match("*_test.py", "foo_test.py"));
        assert!(glob_match("tests.py", "tests.py"));
        assert!(!glob_match("test_*.py", "foo_test.py"));
        assert!(!glob_match("tests.py", "tests_extra.py"));
    }
}
