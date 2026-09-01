//! Monorepo support, P0: discover subprojects (each with its own pytest
//! config) and run them as sequential session groups.
//!
//! pytest cannot run a repo of per-package configs from the root (one
//! rootdir/ini, colliding conftests). Here each project runs its own full
//! pool with cwd = project dir, so semantics and caches match pytest there.

use std::path::{Path, PathBuf};

use crate::config;

/// Directories never worth descending into.
fn pruned(name: &str, dir: &Path) -> bool {
    name.starts_with('.')
        || name == "__pycache__"
        || name == "node_modules"
        || name == "site-packages"
        || dir.join("pyvenv.cfg").exists()
}

/// Find subprojects under `root`: directories carrying their own pytest
/// config. Auto mode walks at most `MAX_DEPTH` levels and does not descend
/// into a found project. `[tool.rstest] projects` globs override the walk.
const MAX_DEPTH: usize = 4;

pub fn discover_projects(root: &Path, project_globs: Option<&[String]>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, 0, &mut found);
    found.sort();
    if let Some(globs) = project_globs {
        found.retain(|p| {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            globs.iter().any(|g| crate::collect::glob_match(g, &rel))
        });
    }
    found
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if pruned(name, &path) {
            continue;
        }
        if config::has_pytest_config(&path) {
            // A project owns its subtree: nested configs are its own.
            found.push(path);
        } else {
            walk(&path, depth + 1, found);
        }
    }
}

/// Filesystem-safe slug for per-project output files
/// (`junit.xml` -> `junit.libs-cli.xml`).
pub fn slug(root: &Path, project: &Path) -> String {
    project
        .strip_prefix(root)
        .unwrap_or(project)
        .to_string_lossy()
        .replace(['/', '\\'], "-")
}

/// Insert the project slug before the extension: `out/junit.xml` +
/// `libs-cli` -> `out/junit.libs-cli.xml`.
pub fn suffixed(path: &Path, slug: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    path.with_file_name(format!("{stem}.{slug}{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-mono-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mkproj(root: &Path, rel: &str, cfg: &str) {
        let d = root.join(rel);
        std::fs::create_dir_all(&d).unwrap();
        let (file, content) = match cfg {
            "ini" => ("pytest.ini", "[pytest]\n".to_string()),
            "pyproject" => (
                "pyproject.toml",
                "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n".to_string(),
            ),
            _ => unreachable!(),
        };
        std::fs::write(d.join(file), content).unwrap();
    }

    #[test]
    fn discovers_configured_dirs_only() {
        let root = tmp("disc");
        mkproj(&root, "libs/a", "ini");
        mkproj(&root, "libs/b", "pyproject");
        // pyproject WITHOUT pytest section: not a project
        std::fs::create_dir_all(root.join("libs/c")).unwrap();
        std::fs::write(
            root.join("libs/c/pyproject.toml"),
            "[project]\nname=\"c\"\n",
        )
        .unwrap();
        // venv-shaped dir with a config inside: pruned
        std::fs::create_dir_all(root.join(".venv/x")).unwrap();
        std::fs::write(root.join(".venv/x/pytest.ini"), "[pytest]\n").unwrap();
        let found = discover_projects(&root, None);
        let rels: Vec<String> = found.iter().map(|p| slug(&root, p)).collect();
        assert_eq!(rels, vec!["libs-a", "libs-b"]);
    }

    #[test]
    fn nested_configs_belong_to_their_project() {
        let root = tmp("nested");
        mkproj(&root, "pkg", "ini");
        mkproj(&root, "pkg/sub", "ini"); // inside a project: not separate
        let found = discover_projects(&root, None);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn globs_filter() {
        let root = tmp("globs");
        mkproj(&root, "libs/a", "ini");
        mkproj(&root, "libs/b", "ini");
        mkproj(&root, "services/x", "ini");
        let globs = vec!["libs/*".to_string()];
        let found = discover_projects(&root, Some(&globs));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn suffix_insertion() {
        assert_eq!(
            suffixed(Path::new("out/junit.xml"), "libs-a"),
            PathBuf::from("out/junit.libs-a.xml")
        );
        assert_eq!(
            suffixed(Path::new("report"), "x"),
            PathBuf::from("report.x")
        );
    }
}

/// Per-project worker shares honoring fixed pins: a project whose own
/// `[tool.rstest] numprocesses` is a number keeps it (clamped to budget;
/// 0/1 = single-worker exact mode). The rest split the remainder by weight.
pub fn plan_shares_with_fixed(
    costs: &[Option<f64>],
    fixed: &[Option<usize>],
    budget: usize,
) -> Vec<usize> {
    let n = costs.len();
    let fixed_spend: usize = fixed
        .iter()
        .flatten()
        // -n 0 still occupies one worker process
        .map(|&f| f.clamp(1, budget))
        .sum();
    let free_idx: Vec<usize> = (0..n).filter(|&i| fixed[i].is_none()).collect();
    let free_costs: Vec<Option<f64>> = free_idx.iter().map(|&i| costs[i]).collect();
    let free_budget = budget.saturating_sub(fixed_spend).max(free_idx.len());
    let free_shares = plan_shares(&free_costs, free_budget);
    let mut shares = vec![0usize; n];
    for (slot, &i) in free_idx.iter().enumerate() {
        shares[i] = free_shares[slot];
    }
    for i in 0..n {
        if let Some(f) = fixed[i] {
            shares[i] = f.min(budget);
        }
    }
    shares
}

/// A project's own `[tool.rstest] numprocesses`, when it is a NUMBER
/// ("auto" or absent leaves the planner in charge).
pub fn project_fixed_n(project: &Path) -> Option<usize> {
    let settings = crate::config::rstest_settings(project);
    settings.numprocesses.as_deref()?.parse().ok()
}

/// Per-project worker shares for concurrent groups: weighted by each
/// project's duration-cache total, minimum 1, summing to at most `budget`.
/// Unknown projects get the average known weight (first runs split evenly).
pub fn plan_shares(costs: &[Option<f64>], budget: usize) -> Vec<usize> {
    let n = costs.len();
    if n == 0 {
        return Vec::new();
    }
    let budget = budget.max(n); // every project gets at least one worker
    let known: Vec<f64> = costs.iter().flatten().copied().collect();
    let avg = if known.is_empty() {
        1.0
    } else {
        (known.iter().sum::<f64>() / known.len() as f64).max(0.001)
    };
    let weights: Vec<f64> = costs.iter().map(|c| c.unwrap_or(avg).max(0.001)).collect();
    let total: f64 = weights.iter().sum();
    // Reserve the 1-worker minimum for everyone, split the REST by weight:
    // floors can never overshoot the budget that way.
    let extra = budget - n;
    let mut shares: Vec<usize> = weights
        .iter()
        .map(|w| 1 + ((w / total) * extra as f64).floor() as usize)
        .collect();
    // Distribute the flooring remainder to the heaviest projects.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| weights[b].total_cmp(&weights[a]));
    let mut remaining = budget - shares.iter().sum::<usize>().min(budget);
    for &i in order.iter().cycle().take(n * budget) {
        if remaining == 0 {
            break;
        }
        shares[i] += 1;
        remaining -= 1;
    }
    shares
}

/// Sum of a project's duration cache (suite seconds last run), if any.
pub fn project_cost(project: &Path) -> Option<f64> {
    let bytes = std::fs::read(project.join(".rstest_cache/durations.json")).ok()?;
    let map: std::collections::HashMap<String, f64> = serde_json::from_slice(&bytes).ok()?;
    Some(map.values().sum())
}

#[cfg(test)]
mod share_tests {
    use super::plan_shares;

    #[test]
    fn cold_start_splits_evenly() {
        assert_eq!(plan_shares(&[None, None, None, None], 8), vec![2, 2, 2, 2]);
    }

    #[test]
    fn weighted_by_cost_with_minimum_one() {
        // 100s + 4 tiny projects on 14 workers: the heavy one dominates,
        // everyone still gets a worker.
        let shares = plan_shares(
            &[Some(100.0), Some(2.0), Some(1.0), Some(1.0), Some(1.0)],
            14,
        );
        assert_eq!(shares.iter().sum::<usize>(), 14);
        assert!(shares[0] >= 9, "{shares:?}");
        assert!(shares.iter().all(|&s| s >= 1), "{shares:?}");
    }

    #[test]
    fn unknown_projects_get_average_weight() {
        let shares = plan_shares(&[Some(10.0), None], 4);
        assert_eq!(shares.iter().sum::<usize>(), 4);
        assert_eq!(shares, vec![2, 2]); // unknown assumed average (=10)
    }

    #[test]
    fn more_projects_than_budget_still_one_each() {
        let shares = plan_shares(&[None, None, None], 2);
        assert!(shares.iter().all(|&s| s >= 1));
    }
}

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
mod fixed_tests {
    use super::plan_shares_with_fixed;

    #[test]
    fn pinned_projects_keep_their_n() {
        // project 1 pins -n 0 (single-worker exact mode)
        let shares =
            plan_shares_with_fixed(&[Some(50.0), Some(50.0), None], &[None, Some(0), None], 8);
        assert_eq!(shares[1], 0);
        // the others split the remaining budget
        assert_eq!(shares[0] + shares[2], 7);
        assert!(shares[0] >= 1 && shares[2] >= 1);
    }

    #[test]
    fn pin_clamps_to_budget() {
        let shares = plan_shares_with_fixed(&[None, None], &[Some(64), None], 4);
        assert_eq!(shares[0], 4);
        assert!(shares[1] >= 1);
    }

    #[test]
    fn all_pinned() {
        let shares = plan_shares_with_fixed(&[None, None], &[Some(2), Some(3)], 8);
        assert_eq!(shares, vec![2, 3]);
    }
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

/// Merge per-project report-json files into one document. Test keys become
/// ROOT-relative nodeids (as pytest would name them from the root);
/// `meta.projects` carries per-project exit/skip status. Additive to schema 2.
pub fn merge_reports(
    parts: &[(String, Option<std::path::PathBuf>, Option<i32>, bool)],
    run_meta: &crate::reporting::report::RunMeta,
    out: &Path,
) -> anyhow::Result<()> {
    let mut tests = serde_json::Map::new();
    let mut collect_errors: Vec<serde_json::Value> = Vec::new();
    let mut projects = serde_json::Map::new();
    let mut totals: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for (rel, path, status, skipped) in parts {
        let mut entry = serde_json::Map::new();
        if *skipped {
            entry.insert("skipped".into(), true.into());
        } else if let Some(code) = status {
            entry.insert("exitstatus".into(), (*code).into());
        }
        let mut part_doc: Option<serde_json::Value> = None;
        if let Some(path) = path {
            if let Ok(bytes) = std::fs::read(path) {
                part_doc = serde_json::from_slice(&bytes).ok();
            }
        }
        if let Some(doc) = &part_doc {
            if let Some(t) = doc.get("tests").and_then(|t| t.as_object()) {
                for (nodeid, v) in t {
                    tests.insert(format!("{rel}/{nodeid}"), v.clone());
                }
            }
            if let Some(errs) = doc.get("collect_errors").and_then(|e| e.as_array()) {
                for e in errs {
                    let prefixed = e
                        .as_str()
                        .map(|p| serde_json::Value::String(format!("{rel}/{p}")))
                        .unwrap_or_else(|| e.clone());
                    collect_errors.push(prefixed);
                }
            }
            // Per-project counts ride into meta.projects; grand totals
            // aggregate across projects.
            if let Some(counts) = doc
                .get("meta")
                .and_then(|m| m.get("counts"))
                .and_then(|c| c.as_object())
            {
                entry.insert("counts".into(), counts.clone().into());
                for (k, v) in counts {
                    *totals.entry(k.clone()).or_default() += v.as_u64().unwrap_or(0);
                }
            }
        }
        projects.insert(rel.clone(), entry.into());
    }
    let doc = serde_json::json!({
        "meta": {
            "runner": "rstest",
            "schema": 4,
            "exitstatus": run_meta.exitstatus,
            "counts": totals,
            "duration_seconds": (run_meta.duration_seconds * 100.0).round() / 100.0,
            "started_at_epoch": run_meta.started_at_epoch,
            "workers": run_meta.workers,
            "argv": run_meta.argv,
            "projects": projects,
        },
        "collect_errors": collect_errors,
        "tests": tests,
    });
    std::fs::write(out, serde_json::to_vec_pretty(&doc)?)?;
    Ok(())
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn merges_with_root_relative_keys() {
        let dir = std::env::temp_dir().join(format!("rstest-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.json");
        std::fs::write(
            &a,
            r#"{"meta":{"schema":3,"exitstatus":0,"counts":{"passed":1,"failed":0}},
               "collect_errors":[],
               "tests":{"tests/test_x.py::t1":{"call":"passed"}}}"#,
        )
        .unwrap();
        let b = dir.join("b.json");
        std::fs::write(
            &b,
            r#"{"meta":{"schema":3,"exitstatus":1,"counts":{"passed":0,"failed":1}},
               "collect_errors":["broken.py"],
               "tests":{"tests/test_y.py::t2":{"call":"failed"}}}"#,
        )
        .unwrap();
        let out = dir.join("merged.json");
        merge_reports(
            &[
                ("libs/a".into(), Some(a), Some(0), false),
                ("libs/b".into(), Some(b), Some(1), false),
                ("libs/c".into(), None, None, true), // skipped by --changed
            ],
            &crate::reporting::report::RunMeta {
                exitstatus: 1,
                duration_seconds: 12.345,
                started_at_epoch: 1_750_000_000,
                workers: 4,
                argv: vec!["rstest".into()],
            },
            &out,
        )
        .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(doc["meta"]["exitstatus"], 1);
        assert_eq!(doc["meta"]["schema"], 4);
        assert_eq!(doc["meta"]["counts"]["passed"], 1);
        assert_eq!(doc["meta"]["counts"]["failed"], 1);
        assert_eq!(doc["meta"]["projects"]["libs/b"]["counts"]["failed"], 1);
        assert_eq!(doc["meta"]["duration_seconds"], 12.35);
        assert_eq!(doc["meta"]["projects"]["libs/c"]["skipped"], true);
        assert_eq!(doc["meta"]["projects"]["libs/b"]["exitstatus"], 1);
        assert_eq!(doc["tests"]["libs/a/tests/test_x.py::t1"]["call"], "passed");
        assert_eq!(doc["tests"]["libs/b/tests/test_y.py::t2"]["call"], "failed");
        assert_eq!(doc["collect_errors"][0], "libs/b/broken.py");
    }
}
