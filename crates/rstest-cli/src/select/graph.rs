//! Import-graph selection: build a reverse-import index of the project's
//! `.py` files, then BFS from the changed files to the test files that reach
//! them. Conservative by construction (over-selection is always safe).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{rule1_full_run, Selection};
use crate::config::ProjectConfig;

/// Map changed files to the affected test files. `strict`: any changed source
/// file whose reverse import reach contains NO test file falls back to a full run
/// instead of silently selecting nothing (dynamic-import target, unused module).
pub fn affected_tests(
    rootdir: &Path,
    project: &ProjectConfig,
    changed: &[PathBuf],
    strict: bool,
) -> Result<Selection> {
    // Rule 1: anything that isn't a Python file defeats the graph.
    if let Some(full) = rule1_full_run(changed) {
        return Ok(full);
    }

    let index = ProjectIndex::build(rootdir)?;

    let mut affected: HashSet<PathBuf> = HashSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    for c in changed {
        let abs = rootdir.join(c);
        let canon = abs.canonicalize().unwrap_or(abs);
        // Rule 2: conftest.py affects every test below its directory.
        if canon.file_name().and_then(|n| n.to_str()) == Some("conftest.py") {
            if let Some(dir) = canon.parent() {
                for f in &index.files {
                    if f.starts_with(dir) {
                        affected.insert(f.clone());
                    }
                }
            }
            continue;
        }
        queue.push_back(canon);
    }

    // Strict: every changed SOURCE file must provably reach a test.
    // (Tests select themselves; conftest covers its subtree by rule 2.)
    if strict {
        for file in &queue {
            if crate::collect::is_test_file(file, project) {
                continue;
            }
            let mut reach: VecDeque<&PathBuf> = VecDeque::from([file]);
            let mut seen: HashSet<&PathBuf> = HashSet::from([file]);
            let mut covered = false;
            while let Some(f) = reach.pop_front() {
                if crate::collect::is_test_file(f, project) {
                    covered = true;
                    break;
                }
                if let Some(importers) = index.reverse.get(f) {
                    for imp in importers {
                        if seen.insert(imp) {
                            reach.push_back(imp);
                        }
                    }
                }
            }
            if !covered {
                return Ok(Selection::FullRun(format!(
                    "--changed-strict: {} reaches no tests via the import \
                     graph (dynamic import target, unused module, or deleted \
                     file) — running everything instead of risking a false skip",
                    file.strip_prefix(rootdir).unwrap_or(file).display()
                )));
            }
        }
    }

    // Reverse BFS over the import graph.
    let mut seen: HashSet<PathBuf> = queue.iter().cloned().collect();
    while let Some(file) = queue.pop_front() {
        affected.insert(file.clone());
        if let Some(importers) = index.reverse.get(&file) {
            for imp in importers {
                if seen.insert(imp.clone()) {
                    queue.push_back(imp.clone());
                }
            }
        }
    }

    let tests: Vec<PathBuf> = affected
        .into_iter()
        // is_test_file is a name-pattern match, so a DELETED test file still
        // matches; drop anything no longer on disk so pytest is never handed a
        // missing path (importers of a deleted source still exist and remain).
        .filter(|f| crate::collect::is_test_file(f, project) && f.exists())
        .collect();
    let mut tests: Vec<PathBuf> = tests
        .into_iter()
        .map(|f| f.strip_prefix(rootdir).map(PathBuf::from).unwrap_or(f))
        .collect();
    tests.sort();
    Ok(Selection::Tests(tests))
}

struct ProjectIndex {
    files: Vec<PathBuf>,
    /// imported file -> files importing it
    reverse: HashMap<PathBuf, Vec<PathBuf>>,
}

impl ProjectIndex {
    fn build(rootdir: &Path) -> Result<Self> {
        // All project .py files (same pruning as the test-file walker).
        let mut files: Vec<PathBuf> = Vec::new();
        let walker = ignore::WalkBuilder::new(rootdir)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .filter_entry(|e| {
                let n = e.file_name().to_str().unwrap_or("");
                n != "__pycache__" && n != ".git" && !e.path().join("pyvenv.cfg").exists()
            })
            .build();
        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|t| t.is_file())
                && entry.path().extension().and_then(|e| e.to_str()) == Some("py")
            {
                files.push(
                    entry
                        .path()
                        .canonicalize()
                        .unwrap_or_else(|_| entry.into_path()),
                );
            }
        }

        // Module index: dotted path (relative to rootdir) -> file.
        // Lookup is by suffix, so src/ layouts and ambiguous short names
        // resolve to every candidate (over-selection is safe).
        let rootdir = rootdir
            .canonicalize()
            .unwrap_or_else(|_| rootdir.to_path_buf());
        let mut dotted: Vec<(String, PathBuf)> = Vec::new();
        for f in &files {
            let rel = f.strip_prefix(&rootdir).unwrap_or(f);
            let mut parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            if let Some(last) = parts.last_mut() {
                *last = last.trim_end_matches(".py").to_string();
            }
            if parts.last().map(String::as_str) == Some("__init__") {
                parts.pop();
            }
            dotted.push((parts.join("."), f.clone()));
        }
        let resolve = |module: &str| -> Vec<PathBuf> {
            let suffix = format!(".{module}");
            dotted
                .iter()
                .filter(|(d, _)| d == module || d.ends_with(&suffix))
                .map(|(_, f)| f.clone())
                .collect()
        };

        // Scan imports, build reverse edges.
        let mut reverse: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for f in &files {
            let Ok(src) = std::fs::read_to_string(f) else {
                continue;
            };
            let importer_dotted = dotted
                .iter()
                .find(|(_, p)| p == f)
                .map(|(d, _)| d.clone())
                .unwrap_or_default();
            for module in imports_of(&src, &importer_dotted) {
                for target in resolve(&module) {
                    if &target != f {
                        reverse.entry(target).or_default().push(f.clone());
                    }
                }
            }
        }
        Ok(Self { files, reverse })
    }
}

/// Modules imported by `src`. Includes indented (function-local /
/// conditional) imports - extra edges only ever widen the selection.
pub(crate) fn imports_of(src: &str, importer_dotted: &str) -> Vec<String> {
    let mut modules = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("import ") {
            for part in rest.split(',') {
                let m = part.split_whitespace().next().unwrap_or("");
                if !m.is_empty() {
                    modules.push(m.to_string());
                }
            }
        } else if let Some(rest) = t.strip_prefix("from ") {
            let Some((module_part, names)) = rest.split_once(" import ") else {
                continue;
            };
            let module_part = module_part.trim();
            let level = module_part.chars().take_while(|&c| c == '.').count();
            let named = &module_part[level..];
            let base = if level > 0 {
                // Relative: resolve against the importer's package.
                let mut pkg: Vec<&str> = importer_dotted.split('.').collect();
                for _ in 0..level {
                    pkg.pop();
                }
                let mut base = pkg.join(".");
                if !named.is_empty() {
                    if !base.is_empty() {
                        base.push('.');
                    }
                    base.push_str(named);
                }
                base
            } else {
                named.to_string()
            };
            if !base.is_empty() {
                modules.push(base.clone());
            }
            // `from pkg import x` may import the submodule pkg.x: add a
            // candidate per imported name (misses just don't resolve).
            for name in names.trim_start_matches('(').split(',') {
                let n = name
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(')');
                if !n.is_empty() && n != "*" && !base.is_empty() {
                    modules.push(format!("{base}.{n}"));
                }
            }
        }
    }
    modules
}

#[cfg(test)]
mod tests {
    use super::{affected_tests, imports_of, Selection};
    use crate::config::ProjectConfig;
    use std::path::{Path, PathBuf};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-graph-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // Canonicalize: the walker canonicalizes indexed files, so the rootdir
        // must too or strip_prefix can't make results rootdir-relative.
        d.canonicalize().unwrap()
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn plain_and_comma_imports() {
        let mods = imports_of("import os, mypkg.utils\nimport json as j\n", "tests.test_x");
        assert!(mods.contains(&"os".to_string()));
        assert!(mods.contains(&"mypkg.utils".to_string()));
        assert!(mods.contains(&"json".to_string()));
    }

    #[test]
    fn from_imports_add_submodule_candidates() {
        let mods = imports_of("from mypkg import utils, helpers\n", "tests.test_x");
        // the package itself AND each name as a possible submodule
        assert!(mods.contains(&"mypkg".to_string()));
        assert!(mods.contains(&"mypkg.utils".to_string()));
        assert!(mods.contains(&"mypkg.helpers".to_string()));
    }

    #[test]
    fn relative_imports_resolve_against_importer_package() {
        // tests/sub/test_a.py doing `from ..core import thing`
        let mods = imports_of("from ..core import thing\n", "tests.sub.test_a");
        assert!(mods.contains(&"tests.core".to_string()), "{mods:?}");
        assert!(mods.contains(&"tests.core.thing".to_string()));
        // `from . import sibling`
        let mods = imports_of("from . import sibling\n", "tests.sub.test_a");
        assert!(mods.contains(&"tests.sub.sibling".to_string()), "{mods:?}");
    }

    #[test]
    fn from_without_import_keyword_is_skipped() {
        // `from x` with no ` import ` clause is incomplete: it contributes no
        // module and must not panic (the split_once returns None -> continue).
        let mods = imports_of("from x\nfrom y import z\n", "tests.test_x");
        assert!(!mods.iter().any(|m| m == "x"), "{mods:?}");
        assert!(mods.contains(&"y".to_string()), "{mods:?}");
    }

    #[test]
    fn conftest_change_selects_every_test_in_its_subtree() {
        // Rule 2: a changed conftest.py affects all tests below its directory,
        // with no import edge needed (the conftest branch's `continue`).
        let root = tmp("conftest");
        write(&root, "pkg/conftest.py", "");
        write(&root, "pkg/test_a.py", "def test_a():\n    pass\n");
        write(&root, "pkg/sub/test_b.py", "def test_b():\n    pass\n");
        write(&root, "other/test_c.py", "def test_c():\n    pass\n");

        let sel = affected_tests(
            &root,
            &ProjectConfig::default(),
            &[PathBuf::from("pkg/conftest.py")],
            false,
        )
        .unwrap();
        match sel {
            Selection::Tests(tests) => {
                assert!(tests.contains(&PathBuf::from("pkg/test_a.py")), "{tests:?}");
                assert!(
                    tests.contains(&PathBuf::from("pkg/sub/test_b.py")),
                    "{tests:?}"
                );
                // A test outside the conftest's subtree is not pulled in.
                assert!(
                    !tests.contains(&PathBuf::from("other/test_c.py")),
                    "{tests:?}"
                );
            }
            Selection::FullRun(r) => panic!("unexpected full run: {r}"),
        }
    }

    // An unreadable .py file must be walked over (read_to_string errors ->
    // `continue`) rather than aborting the whole index build.
    #[cfg(unix)]
    #[test]
    fn unreadable_source_file_is_skipped_during_build() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("unreadable");
        write(&root, "pkg/mod.py", "x = 1\n");
        write(&root, "pkg/test_a.py", "from pkg import mod\n");
        let bad = root.join("pkg/bad.py");
        std::fs::write(&bad, "import pkg.mod\n").unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();

        let sel = affected_tests(
            &root,
            &ProjectConfig::default(),
            &[PathBuf::from("pkg/mod.py")],
            false,
        );
        // Restore perms so the temp dir can be cleaned up regardless.
        let _ = std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644));
        let sel = sel.unwrap();
        match sel {
            Selection::Tests(tests) => {
                assert!(tests.contains(&PathBuf::from("pkg/test_a.py")), "{tests:?}");
            }
            Selection::FullRun(r) => panic!("unexpected full run: {r}"),
        }
    }

    #[test]
    fn star_imports_keep_base_only() {
        let mods = imports_of("from mypkg.core import *\n", "tests.test_x");
        assert_eq!(mods, vec!["mypkg.core".to_string()]);
    }

    #[test]
    fn indented_imports_inside_functions_count() {
        // over-selection by design: function-local imports still create edges
        let mods = imports_of("def test_x():\n    import lazy_dep\n", "tests.test_x");
        assert!(mods.contains(&"lazy_dep".to_string()));
    }
}
