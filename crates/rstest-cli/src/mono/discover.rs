//! Subproject discovery and per-project output-file naming.

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
