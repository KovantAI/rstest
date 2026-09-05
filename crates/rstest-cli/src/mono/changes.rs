//! Which projects `--changed` must run: classify direct edits, dependents
//! (via declared + scanned inter-project edges), and the unaffected rest.

use std::path::{Path, PathBuf};

/// `[project].name` from a project's pyproject, PEP-503-normalized
/// (lowercase, runs of `-_.` collapse to `-`) so dependency strings and
/// project names compare reliably.
pub fn project_name(project: &Path) -> Option<String> {
    let text = std::fs::read_to_string(project.join("pyproject.toml")).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    let name = doc.get("project")?.get("name")?.as_str()?;
    Some(normalize(name))
}

/// Names this project depends on ([project].dependencies +
/// [project.optional-dependencies] + [dependency-groups]), normalized. Only
/// sibling names matter to the caller; over-collecting third-party is fine.
pub fn project_deps(project: &Path) -> Vec<String> {
    let Some(text) = std::fs::read_to_string(project.join("pyproject.toml")).ok() else {
        return Vec::new();
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return Vec::new();
    };
    let mut deps = Vec::new();
    let mut push_reqs = |v: &toml::Value| {
        if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(req) = item.as_str() {
                    deps.push(req_name(req));
                }
            }
        }
    };
    if let Some(proj) = doc.get("project") {
        if let Some(v) = proj.get("dependencies") {
            push_reqs(v);
        }
        if let Some(t) = proj.get("optional-dependencies").and_then(|t| t.as_table()) {
            for v in t.values() {
                push_reqs(v);
            }
        }
    }
    if let Some(t) = doc.get("dependency-groups").and_then(|t| t.as_table()) {
        for v in t.values() {
            push_reqs(v); // include-group entries aren't strings; skipped
        }
    }
    deps
}

/// Leading package name of a PEP 508 requirement string.
fn req_name(req: &str) -> String {
    let end = req
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .unwrap_or(req.len());
    normalize(&req[..end])
}

fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !prev_sep {
                out.push('-');
            }
            prev_sep = true;
        } else {
            out.push(c.to_ascii_lowercase());
            prev_sep = false;
        }
    }
    out
}

/// Why each project runs (or doesn't) under `--changed`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ChangeImpact {
    /// Files changed inside the project: run with --changed (the child
    /// narrows further via its own import graph).
    Direct,
    /// A sibling it depends on (transitively) changed: run the FULL suite,
    /// since its own files didn't change and child-local selection would
    /// find nothing.
    Dependent,
    Unaffected,
}

/// Classify projects against a changed-file list. Changes outside every
/// project (root configs, shared scripts) run EVERYTHING as the
/// conservative fallback.
pub fn classify_changes(
    root: &Path,
    projects: &[PathBuf],
    changed: &[PathBuf],
    strict: bool,
) -> Vec<ChangeImpact> {
    let n = projects.len();
    let mut impact = vec![ChangeImpact::Unaffected; n];
    let mut orphan_changes = false;
    for file in changed {
        let abs = root.join(file);
        match projects.iter().position(|p| abs.starts_with(p)) {
            Some(i) => impact[i] = ChangeImpact::Direct,
            None => orphan_changes = true,
        }
    }
    if orphan_changes {
        // A change OUTSIDE every project is invisible to project-local
        // import graphs (forwarding --changed narrows to zero), so run every
        // project FULL, matching single-project rstest's config-change fallback.
        return vec![ChangeImpact::Dependent; n];
    }
    // Reverse dependency closure: dependents of changed projects run too.
    // Edges come from DECLARED metadata; under strict, scanned imports
    // (undeclared siblings in a shared venv) are unioned in.
    let names: Vec<Option<String>> = projects.iter().map(|p| project_name(p)).collect();
    let deps: Vec<Vec<String>> = projects.iter().map(|p| project_deps(p)).collect();
    let mut edges: Vec<Vec<bool>> = vec![vec![false; n]; n];
    for i in 0..n {
        for j in 0..n {
            if i != j
                && names[j]
                    .as_ref()
                    .is_some_and(|nj| deps[i].iter().any(|d| d == nj))
            {
                edges[i][j] = true;
            }
        }
    }
    if strict {
        for (i, sibs) in scanned_sibling_edges(projects).into_iter().enumerate() {
            for j in sibs {
                if !edges[i][j] {
                    eprintln!(
                        "rstest: --changed-strict: {} imports code provided by {} \
                         without declaring it; counting the edge",
                        projects[i].display(),
                        projects[j].display()
                    );
                    edges[i][j] = true;
                }
            }
        }
    }
    let mut grew = true;
    while grew {
        grew = false;
        for i in 0..n {
            if impact[i] != ChangeImpact::Unaffected {
                continue;
            }
            if (0..n).any(|j| edges[i][j] && impact[j] != ChangeImpact::Unaffected) {
                impact[i] = ChangeImpact::Dependent;
                grew = true;
            }
        }
    }
    impact
}

/// The interpreter a project's children should use: an explicit setting
/// wins; otherwise a project-local venv if present.
pub fn project_python(project: &Path) -> Option<PathBuf> {
    for rel in [".venv/bin/python", ".venv/Scripts/python.exe"] {
        let p = project.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Top-level importable names a project provides: package dirs and modules
/// at the project root and under `src/` (bare namespace-package dirs count;
/// tests and hidden dirs don't).
pub fn top_level_modules(project: &Path) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for base in [project.to_path_buf(), project.join("src")] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') || name == "tests" || name == "test" {
                continue;
            }
            if path.is_dir() {
                // A dir with any .py inside is importable (incl. namespace
                // packages without __init__.py).
                let has_py = std::fs::read_dir(&path)
                    .map(|mut it| {
                        it.any(|e| {
                            e.ok().is_some_and(|e| {
                                e.path().extension().and_then(|x| x.to_str()) == Some("py")
                                    || e.path().is_dir()
                            })
                        })
                    })
                    .unwrap_or(false);
                if has_py {
                    names.insert(name.to_string());
                }
            } else if let Some(stem) = name.strip_suffix(".py") {
                if stem != "setup" && stem != "conftest" && stem != "noxfile" {
                    names.insert(stem.to_string());
                }
            }
        }
    }
    names
}

/// Scan each project's Python files for imports resolving to a SIBLING
/// project's top-level modules: the undeclared-dependency detector behind
/// --changed-strict. Over-connects the safe way (extra runs, never a skip).
pub fn scanned_sibling_edges(projects: &[PathBuf]) -> Vec<Vec<usize>> {
    let n = projects.len();
    let provides: Vec<std::collections::HashSet<String>> =
        projects.iter().map(|p| top_level_modules(p)).collect();
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        let mut first_segments: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let walker = ignore::WalkBuilder::new(&projects[i])
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .filter_entry(|e| {
                let name = e.file_name().to_str().unwrap_or("");
                !name.starts_with('.') && name != "__pycache__" && name != "node_modules"
            })
            .build();
        for entry in walker.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("py") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            for module in crate::select::imports_of(&src, "") {
                let first = module.split('.').next().unwrap_or(&module);
                first_segments.insert(first.to_string());
            }
        }
        for (j, prov) in provides.iter().enumerate() {
            if i != j && first_segments.iter().any(|seg| prov.contains(seg)) {
                edges[i].push(j);
            }
        }
    }
    edges
}

#[cfg(test)]
mod change_tests {
    use super::*;

    fn proj(root: &Path, rel: &str, name: &str, deps: &[&str]) -> PathBuf {
        let d = root.join(rel);
        std::fs::create_dir_all(&d).unwrap();
        let deps_toml: Vec<String> = deps.iter().map(|d| format!("\"{d}\"")).collect();
        std::fs::write(
            d.join("pyproject.toml"),
            format!(
                "[project]\nname = \"{name}\"\ndependencies = [{}]\n\n[tool.pytest.ini_options]\n",
                deps_toml.join(", ")
            ),
        )
        .unwrap();
        d
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-chg-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn req_names_normalize() {
        assert_eq!(
            req_name("langgraph-checkpoint>=2.0,<3"),
            "langgraph-checkpoint"
        );
        assert_eq!(req_name("Foo_Bar.baz[extra]==1.0"), "foo-bar-baz");
        assert_eq!(req_name("simple"), "simple");
    }

    #[test]
    fn classify_direct_dependent_unaffected() {
        let root = tmp("cls");
        let a = proj(&root, "libs/a", "pkg-a", &[]);
        let b = proj(&root, "libs/b", "pkg-b", &["pkg-a>=1"]);
        let c = proj(&root, "libs/c", "pkg-c", &["requests"]);
        // transitive: d -> b -> a
        let d = proj(&root, "libs/d", "pkg-d", &["pkg-b"]);
        let projects = vec![a, b, c, d];
        let changed = vec![PathBuf::from("libs/a/src/x.py")];
        let impacts = classify_changes(&root, &projects, &changed, false);
        assert_eq!(
            impacts,
            vec![
                ChangeImpact::Direct,
                ChangeImpact::Dependent,
                ChangeImpact::Unaffected,
                ChangeImpact::Dependent,
            ]
        );
    }

    #[test]
    fn orphan_changes_run_everything() {
        let root = tmp("orphan");
        let a = proj(&root, "libs/a", "pkg-a", &[]);
        let b = proj(&root, "libs/b", "pkg-b", &[]);
        let impacts = classify_changes(&root, &[a, b], &[PathBuf::from("shared/util.py")], false);
        // full runs, no narrowing: the change is invisible to
        // project-local import graphs
        assert!(impacts.iter().all(|i| *i == ChangeImpact::Dependent));
    }

    #[test]
    fn no_changes_no_projects_run() {
        let root = tmp("none");
        let a = proj(&root, "libs/a", "pkg-a", &[]);
        let impacts = classify_changes(&root, &[a], &[], false);
        assert_eq!(impacts, vec![ChangeImpact::Unaffected]);
    }
}

#[cfg(test)]
mod strict_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-strict-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn proj_with_code(root: &Path, rel: &str, name: &str, modname: &str, code: &str) -> PathBuf {
        let d = root.join(rel);
        std::fs::create_dir_all(d.join(modname)).unwrap();
        std::fs::write(
            d.join("pyproject.toml"),
            format!(
                "[project]\nname = \"{name}\"\ndependencies = []\n\n[tool.pytest.ini_options]\n"
            ),
        )
        .unwrap();
        std::fs::write(d.join(modname).join("__init__.py"), "").unwrap();
        std::fs::write(d.join("test_x.py"), code).unwrap();
        d
    }

    #[test]
    fn top_level_module_detection() {
        let root = tmp("tlm");
        let p = proj_with_code(&root, "libs/a", "pkg-a", "pkg_a", "");
        let mods = top_level_modules(&p);
        assert!(mods.contains("pkg_a"), "{mods:?}");
        assert!(!mods.contains("tests"));
    }

    #[test]
    fn undeclared_import_becomes_edge_under_strict() {
        let root = tmp("edge");
        let a = proj_with_code(&root, "libs/a", "pkg-a", "pkg_a", "");
        // b imports pkg_a WITHOUT declaring it
        let b = proj_with_code(&root, "libs/b", "pkg-b", "pkg_b", "import pkg_a\n");
        let projects = vec![a, b];
        let changed = vec![PathBuf::from("libs/a/pkg_a/__init__.py")];
        let lax = classify_changes(&root, &projects, &changed, false);
        assert_eq!(
            lax[1],
            ChangeImpact::Unaffected,
            "lax misses undeclared import"
        );
        let strict = classify_changes(&root, &projects, &changed, true);
        assert_eq!(strict[1], ChangeImpact::Dependent, "strict catches it");
    }
}
