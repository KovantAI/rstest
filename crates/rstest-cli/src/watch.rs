//! `rstest --watch`: rerun on file change.
//!
//! A change set of only test files reruns exactly those; any other .py
//! change goes through import-graph selection (`select::affected_tests`),
//! full selection when affected tests can't be resolved.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};

use crate::{collect, config, execute, select, Cli};

const DEBOUNCE: Duration = Duration::from_millis(300);

pub fn watch_loop(cli: &Cli, base_args: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project = config::discover(&cwd);

    let (tx, rx) = mpsc::channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            for path in event.paths {
                let _ = tx.send(path);
            }
        }
    })?;
    watcher.watch(&cwd, RecursiveMode::Recursive)?;

    let mut status = execute(cli, base_args)?;
    loop {
        // Discard events the run itself produced and anything queued while
        // it executed. Otherwise a slow run's prior-cycle events (e.g. the
        // initial collection) survive and coalesce with the next edit.
        while rx.try_recv().is_ok() {}

        eprintln!("\n[watch] waiting for changes... (Ctrl+C to quit, last exit: {status})");

        // Block for the first relevant change, then drain the burst.
        let mut changed: Vec<PathBuf> = Vec::new();
        loop {
            let path = rx.recv()?; // watcher thread lives as long as we do
            if relevant(&path) {
                changed.push(path);
                break;
            }
        }
        std::thread::sleep(DEBOUNCE);
        while let Ok(path) = rx.try_recv() {
            if relevant(&path) {
                changed.push(path);
            }
        }
        changed.sort();
        changed.dedup();

        // Only test files touched -> rerun just those. Source changes go
        // through the import graph; full rerun only when the graph can't
        // answer (config change etc.).
        let only_tests = changed.iter().all(|p| collect::is_test_file(p, &project));
        let mut mode = "full selection";
        let args: Vec<String> = if only_tests {
            let mut args: Vec<String> = changed
                .iter()
                .filter(|p| p.exists())
                .map(|p| rel(p, &cwd))
                .collect();
            if args.is_empty() {
                continue; // deleted test files only - nothing to run
            }
            args.extend(flags_only(base_args));
            mode = "changed files";
            args
        } else {
            match select::affected_tests(&project.rootdir, &project, &changed, false) {
                Ok(select::Selection::Tests(tests)) if tests.is_empty() => {
                    eprintln!("[watch] change affects no tests; waiting");
                    continue;
                }
                Ok(select::Selection::Tests(tests)) => {
                    mode = "affected tests";
                    let mut args: Vec<String> =
                        tests.iter().map(|t| t.display().to_string()).collect();
                    args.extend(flags_only(base_args));
                    args
                }
                _ => base_args.to_vec(),
            }
        };

        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            print!("\x1b[2J\x1b[H"); // clear screen, home cursor
        }
        eprintln!(
            "[watch] {} changed; rerunning {}",
            changed
                .iter()
                .map(|p| rel(p, &cwd))
                .collect::<Vec<_>>()
                .join(", "),
            mode
        );
        status = execute(cli, &args)?;
    }
}

/// Worth a rerun? Python sources and config files; never caches/VCS/venvs.
fn relevant(path: &Path) -> bool {
    let ignored = path.components().any(|c| {
        matches!(
            c.as_os_str().to_str().unwrap_or(""),
            ".git"
                | "__pycache__"
                | ".pytest_cache"
                | ".rstest_cache"
                | ".venv"
                | ".gate-venv"
                | "node_modules"
                | "target"
        )
    });
    if ignored {
        return false;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => true,
        Some("toml" | "ini" | "cfg") => {
            path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                matches!(n, "pyproject.toml" | "pytest.ini" | "tox.ini" | "setup.cfg")
            })
        }
        _ => false,
    }
}

/// The user's non-path args (flags and their values), for targeted reruns.
/// Heuristic: keep everything that isn't an existing path argument.
fn flags_only(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| a.starts_with('-') || !Path::new(a).exists())
        .cloned()
        .collect()
}

fn rel(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevant_accepts_python_and_pytest_config_files() {
        assert!(relevant(Path::new("src/test_x.py")));
        assert!(relevant(Path::new("conftest.py")));
        // Only pytest's config files count among toml/ini/cfg.
        assert!(relevant(Path::new("pyproject.toml")));
        assert!(relevant(Path::new("pytest.ini")));
        assert!(relevant(Path::new("tox.ini")));
        assert!(relevant(Path::new("setup.cfg")));
        // Unrelated toml/ini/cfg and other extensions are ignored.
        assert!(!relevant(Path::new("Cargo.toml")));
        assert!(!relevant(Path::new("mypy.ini")));
        assert!(!relevant(Path::new("notes.txt")));
        assert!(!relevant(Path::new("Makefile")));
    }

    #[test]
    fn relevant_ignores_cache_vcs_and_env_dirs() {
        // A .py under any ignored directory never triggers a rerun.
        for dir in [
            ".git",
            "__pycache__",
            ".pytest_cache",
            ".rstest_cache",
            ".venv",
            ".gate-venv",
            "node_modules",
            "target",
        ] {
            let p = PathBuf::from(dir).join("pkg").join("mod.py");
            assert!(!relevant(&p), "{dir} should be ignored");
        }
        // Same filename outside an ignored dir is relevant.
        assert!(relevant(Path::new("pkg/mod.py")));
    }

    #[test]
    fn flags_only_keeps_flags_and_drops_existing_paths() {
        // current_exe exists on disk, so it's treated as a path arg and dropped;
        // flags and non-existent path-like args are kept for the targeted rerun.
        let exe = std::env::current_exe().unwrap().display().to_string();
        let args = vec![
            "-k".to_string(),
            "smoke".to_string(),
            "-x".to_string(),
            exe.clone(),
            "does/not/exist.py".to_string(),
        ];
        let kept = flags_only(&args);
        assert!(kept.contains(&"-k".to_string()));
        assert!(kept.contains(&"smoke".to_string()));
        assert!(kept.contains(&"-x".to_string()));
        assert!(kept.contains(&"does/not/exist.py".to_string()));
        assert!(!kept.contains(&exe), "an existing path arg must be dropped");
    }

    #[test]
    fn rel_strips_cwd_prefix_else_returns_path() {
        let cwd = Path::new("/home/u/proj");
        assert_eq!(rel(&cwd.join("tests/test_a.py"), cwd), "tests/test_a.py");
        // A path outside cwd is returned unchanged.
        assert_eq!(rel(Path::new("/other/x.py"), cwd), "/other/x.py");
    }
}
