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
        let (args, mode) = match plan_rerun(&changed, &project, &cwd, base_args) {
            Plan::Skip => {
                eprintln!("[watch] change affects no tests; waiting");
                continue;
            }
            Plan::Run { args, mode } => (args, mode),
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

/// What a change set should trigger. `Skip` = nothing runnable (deleted test
/// files only, or a source change the import graph maps to no tests).
enum Plan {
    Skip,
    Run {
        args: Vec<String>,
        mode: &'static str,
    },
}

/// Decide what to rerun for a change set. Pure over the filesystem + import
/// graph so it's testable without the watcher/executor:
/// - only test files touched -> rerun exactly those (`changed files`)
/// - a source change -> import-graph selection (`affected tests`)
/// - graph can't answer (config/non-Python change) -> `full selection`
fn plan_rerun(
    changed: &[PathBuf],
    project: &config::ProjectConfig,
    cwd: &Path,
    base_args: &[String],
) -> Plan {
    let only_tests = changed.iter().all(|p| collect::is_test_file(p, project));
    if only_tests {
        let mut args: Vec<String> = changed
            .iter()
            .filter(|p| p.exists())
            .map(|p| rel(p, cwd))
            .collect();
        if args.is_empty() {
            return Plan::Skip; // deleted test files only - nothing to run
        }
        args.extend(flags_only(base_args));
        return Plan::Run {
            args,
            mode: "changed files",
        };
    }
    match select::affected_tests(&project.rootdir, project, changed, false) {
        Ok(select::Selection::Tests(tests)) if tests.is_empty() => Plan::Skip,
        Ok(select::Selection::Tests(tests)) => {
            let mut args: Vec<String> = tests.iter().map(|t| t.display().to_string()).collect();
            args.extend(flags_only(base_args));
            Plan::Run {
                args,
                mode: "affected tests",
            }
        }
        _ => Plan::Run {
            args: base_args.to_vec(),
            mode: "full selection",
        },
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

    fn fresh_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-watch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn project_at(root: &Path) -> config::ProjectConfig {
        config::ProjectConfig {
            rootdir: root.to_path_buf(),
            ..Default::default()
        }
    }

    #[test]
    fn plan_only_test_files_reruns_exactly_those() {
        // A change set of only test files reruns those files, relative to cwd,
        // with the user's flags appended.
        let cwd = fresh_dir("only");
        let t1 = cwd.join("test_a.py");
        let t2 = cwd.join("test_b.py");
        std::fs::write(&t1, "def test_x(): pass\n").unwrap();
        std::fs::write(&t2, "def test_y(): pass\n").unwrap();
        let base = vec!["-k".to_string(), "smoke".to_string()];
        match plan_rerun(&[t1, t2], &project_at(&cwd), &cwd, &base) {
            Plan::Run { args, mode } => {
                assert_eq!(mode, "changed files");
                assert!(args.contains(&"test_a.py".to_string()), "{args:?}");
                assert!(args.contains(&"test_b.py".to_string()), "{args:?}");
                assert!(args.contains(&"-k".to_string()));
                assert!(args.contains(&"smoke".to_string()));
            }
            Plan::Skip => panic!("expected a run for changed test files"),
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn plan_only_deleted_test_files_skips() {
        // Test files matched by name but gone from disk -> nothing to run.
        let cwd = fresh_dir("deleted");
        let gone = cwd.join("test_gone.py"); // never created
        assert!(matches!(
            plan_rerun(&[gone], &project_at(&cwd), &cwd, &[]),
            Plan::Skip
        ));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn plan_config_change_forces_full_selection() {
        // A non-Python (config) change defeats the import graph -> full rerun
        // with the base args passed through verbatim (flags NOT filtered).
        let cwd = fresh_dir("config");
        let cfg = cwd.join("pyproject.toml");
        std::fs::write(&cfg, "[tool.pytest.ini_options]\n").unwrap();
        let base = vec!["-x".to_string()];
        match plan_rerun(&[cfg], &project_at(&cwd), &cwd, &base) {
            Plan::Run { args, mode } => {
                assert_eq!(mode, "full selection");
                assert_eq!(args, base, "full selection reruns with base args verbatim");
            }
            Plan::Skip => panic!("config change should force a full rerun"),
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn plan_source_change_selects_affected_tests() {
        // A source-file change routes through the import graph: the test that
        // imports it is selected, and the user's flags are appended.
        let cwd = fresh_dir("source");
        std::fs::write(cwd.join("mymod.py"), "VALUE = 1\n").unwrap();
        std::fs::write(
            cwd.join("test_uses.py"),
            "import mymod\ndef test_v():\n    assert mymod.VALUE == 1\n",
        )
        .unwrap();
        let base = vec!["-q".to_string()];
        match plan_rerun(&[cwd.join("mymod.py")], &project_at(&cwd), &cwd, &base) {
            Plan::Run { args, mode } => {
                assert_eq!(mode, "affected tests");
                assert!(
                    args.iter().any(|a| a.contains("test_uses.py")),
                    "the importing test should be selected: {args:?}"
                );
                assert!(args.contains(&"-q".to_string()));
            }
            Plan::Skip => panic!("a source change reaching a test must run it"),
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
