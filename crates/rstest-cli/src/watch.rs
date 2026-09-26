//! `rstest --watch`: rerun on file change.
//!
//! A change set of only test files reruns exactly those; any other .py
//! change goes through import-graph selection (`select::affected_tests`),
//! full selection when affected tests can't be resolved. `q` + Enter on stdin
//! ends the session cleanly; Ctrl+C still works.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};

use crate::reporting::color::Palette;
use crate::reporting::sink::Sink;
use crate::{collect, config, execute, select, Cli};

const DEBOUNCE: Duration = Duration::from_millis(300);

/// What the watch loop wakes up for: a filesystem change, or a quit request
/// read from stdin.
enum Event {
    Changed(PathBuf),
    Quit,
}

pub fn watch_loop(cli: &Cli, base_args: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mut sink = Sink::stdio(Palette::detect(base_args));
    let project = config::discover(&cwd, sink.err());
    // Incremental collection: the import-graph index is rebuilt on every source
    // change to find affected tests, and its per-file import scan dominates that
    // latency on large suites. This cache, held for the watch session, lets each
    // reselection re-read only the files whose stamp moved (see
    // `select::CollectionCache`).
    let mut collect_cache = select::CollectionCache::new();

    let (tx, rx) = mpsc::channel::<Event>();
    let quit_by_q = stdin_quit_enabled(cli, base_args, stdin_is_background_tty());
    if quit_by_q {
        let quit_tx = tx.clone();
        // Detached: it blocks on stdin for the whole session and dies with the
        // process. A clean return (rather than a kill) also lets instrumented
        // builds flush their coverage profile.
        std::thread::spawn(move || listen_for_quit(std::io::stdin().lock(), &quit_tx));
    }
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            for path in event.paths {
                let _ = tx.send(Event::Changed(path));
            }
        }
    })?;
    watcher.watch(&cwd, RecursiveMode::Recursive)?;

    let mut status = execute(cli, base_args)?;
    loop {
        let waiting = || sink.warn(&waiting_message(quit_by_q, status));
        // Paths that don't join the change set (edits that landed mid-run,
        // directory events) still reach the graph: a created file can't be
        // found by re-statting known ones.
        let note = |paths: Vec<PathBuf>| collect_cache.note_changed(&project.rootdir, &paths);
        let Some(changed) = next_change_set(&rx, DEBOUNCE, note, waiting)? else {
            return Ok(());
        };

        // Only test files touched -> rerun just those. Source changes go
        // through the import graph; full rerun only when the graph can't
        // answer (config change etc.).
        let (args, mode) = match plan_rerun(&changed, &project, &cwd, base_args, &mut collect_cache)
        {
            Plan::Skip => {
                sink.warn("[watch] change affects no tests; waiting");
                continue;
            }
            Plan::Run { args, mode } => (args, mode),
        };

        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            sink.out_inline("\x1b[2J\x1b[H"); // clear screen, home cursor
        }
        sink.warn(&format!(
            "[watch] {} changed; rerunning {}",
            changed
                .iter()
                .map(|p| rel(p, &cwd))
                .collect::<Vec<_>>()
                .join(", "),
            mode
        ));
        status = execute(cli, &args)?;
    }
}

/// The between-runs prompt. Offers `q` only when the stdin listener runs:
/// elsewhere nothing reads it, and a typed `q` would sit in stdin until the
/// next run's pdb prompt or `input()` consumed it.
fn waiting_message(quit_by_q: bool, status: i32) -> String {
    let quit = if quit_by_q {
        "q + Enter or Ctrl+C"
    } else {
        "Ctrl+C"
    };
    format!("\n[watch] waiting for changes... ({quit} to quit, last exit: {status})")
}

/// Whether `q` + Enter on stdin may end the session. Not in passthrough modes
/// (`--pdb`, `--trace`, `-s`, `--debug`, ...): the worker inherits stdin there
/// and owns it (a debugger prompt, `input()`), so a listener would steal its
/// lines, and pdb's own `q` would end the whole watch session. Not from a
/// background job on a terminal (`rstest --watch &`) either: reading the tty
/// there raises SIGTTIN, which stops the whole process, watcher included.
fn stdin_quit_enabled(cli: &Cli, base_args: &[String], background_tty: bool) -> bool {
    !background_tty && !crate::cli::needs_passthrough_io(base_args) && cli.debug.is_none()
}

/// Stdin is a terminal whose foreground process group is not ours, i.e. we
/// were started as a background job. A pipe or file (CI, `< /dev/null`, the e2e
/// gate) is never "background": reading it cannot stop the process. Checked
/// once at startup; a session later moved with Ctrl+Z + `bg` can still be
/// stopped by its pending read (bring it back with `fg`).
#[cfg(unix)]
fn stdin_is_background_tty() -> bool {
    // SAFETY: isatty/tcgetpgrp/getpgrp take no pointers and only query state;
    // tcgetpgrp returns -1 on error, which (conservatively) reads as background.
    unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1
            && libc::tcgetpgrp(libc::STDIN_FILENO) != libc::getpgrp()
    }
}

/// No job-control stop on read outside Unix.
#[cfg(not(unix))]
fn stdin_is_background_tty() -> bool {
    false
}

/// Read `input` line by line and send [`Event::Quit`] on a `q` / `quit` line.
/// EOF (or a read error) just ends the listener: a watch started with stdin
/// closed or redirected from `/dev/null` (`nohup`, `< /dev/null`) must keep
/// watching, not exit on its first read.
fn listen_for_quit(input: impl BufRead, tx: &mpsc::Sender<Event>) {
    for line in input.lines() {
        let Ok(line) = line else { return };
        if matches!(line.trim(), "q" | "quit") {
            let _ = tx.send(Event::Quit);
            return;
        }
    }
}

/// One wait cycle: drain stale events, announce the wait, then block for the
/// next change set. Stale = events the run itself produced and anything queued
/// while it executed; they must not trigger a rerun (a slow run's prior-cycle
/// events, e.g. the initial collection, would coalesce with the next edit), so
/// their graph-relevant paths go to `note` instead. So do directory events
/// seen while collecting (see [`graph_dir`]). A quit typed during the run is
/// honored, not discarded. `None` = quit.
fn next_change_set(
    rx: &mpsc::Receiver<Event>,
    debounce: Duration,
    mut note: impl FnMut(Vec<PathBuf>),
    on_wait: impl FnOnce(),
) -> Result<Option<Vec<PathBuf>>> {
    let (quit, stale) = drain_stale(rx);
    if quit {
        return Ok(None);
    }
    note(stale);
    on_wait();
    let mut dirs = Vec::new();
    let changed = collect_changes(rx, debounce, &mut dirs)?;
    note(dirs);
    Ok(changed)
}

/// Empty the queue: whether a quit request was in it, and the paths the graph
/// should hear about (relevant files and [`graph_dir`]s).
fn drain_stale(rx: &mpsc::Receiver<Event>) -> (bool, Vec<PathBuf>) {
    let mut quit = false;
    let mut stale = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            Event::Quit => quit = true,
            Event::Changed(path) if relevant(&path) || graph_dir(&path) => stale.push(path),
            Event::Changed(_) => {}
        }
    }
    (quit, stale)
}

/// A directory event the import graph must hear about: moving or copying a
/// package in (`mv`, `cp -r`) often reports only the directory, never the
/// `.py` files inside it. Not a rerun trigger on its own.
fn graph_dir(path: &Path) -> bool {
    !ignored(path) && path.is_dir()
}

/// Block for the first relevant change, sleep `debounce` to let the edit's
/// burst arrive, then drain and return the deduped, sorted set. Irrelevant
/// events (caches, non-Python) are filtered out; a change set is only returned
/// once at least one relevant path has landed. [`graph_dir`] events seen on
/// the way are pushed to `dirs`. `None` when a quit request arrives, before or
/// during the debounce window.
fn collect_changes(
    rx: &mpsc::Receiver<Event>,
    debounce: Duration,
    dirs: &mut Vec<PathBuf>,
) -> Result<Option<Vec<PathBuf>>> {
    let mut changed: Vec<PathBuf> = Vec::new();
    loop {
        // watcher thread lives as long as we do
        match rx.recv()? {
            Event::Quit => return Ok(None),
            Event::Changed(path) if relevant(&path) => {
                changed.push(path);
                break;
            }
            Event::Changed(path) if graph_dir(&path) => dirs.push(path),
            Event::Changed(_) => {}
        }
    }
    std::thread::sleep(debounce);
    while let Ok(event) = rx.try_recv() {
        match event {
            Event::Quit => return Ok(None),
            Event::Changed(path) if relevant(&path) => changed.push(path),
            Event::Changed(path) if graph_dir(&path) => dirs.push(path),
            Event::Changed(_) => {}
        }
    }
    changed.sort();
    changed.dedup();
    Ok(Some(changed))
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
    collect_cache: &mut select::CollectionCache,
) -> Plan {
    let only_tests = changed.iter().all(|p| collect::is_test_file(p, project));
    if only_tests {
        // Bypasses the graph, so tell it: a new or re-importing test file must
        // be in the index the next source edit is selected from.
        collect_cache.note_changed(&project.rootdir, changed);
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
    match select::affected_tests_cached(&project.rootdir, project, changed, false, collect_cache) {
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
    if ignored(path) {
        return false;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => true,
        Some("toml" | "ini" | "cfg") => path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| crate::config::CONFIG_NAMES.contains(&n)),
        _ => false,
    }
}

/// Under a VCS, cache, or virtualenv directory: never watched.
fn ignored(path: &Path) -> bool {
    path.components().any(|c| {
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
    })
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

    const NO_DEBOUNCE: Duration = Duration::from_millis(0);

    #[test]
    fn collect_changes_filters_sorts_and_dedups() {
        // Irrelevant events are dropped; relevant ones come back sorted and
        // deduplicated.
        let (tx, rx) = mpsc::channel();
        for p in [
            "__pycache__/x.py", // ignored dir
            "b/test_b.py",
            "notes.txt", // wrong extension
            "a/test_a.py",
            "b/test_b.py", // duplicate
        ] {
            tx.send(Event::Changed(PathBuf::from(p))).unwrap();
        }
        drop(tx); // close so a final try_recv can't block
        let got = collect_changes(&rx, NO_DEBOUNCE, &mut Vec::new())
            .unwrap()
            .unwrap();
        assert_eq!(
            got,
            vec![PathBuf::from("a/test_a.py"), PathBuf::from("b/test_b.py")]
        );
    }

    #[test]
    fn collect_changes_blocks_past_irrelevant_until_first_relevant() {
        // Leading irrelevant events don't end the wait; the first relevant one
        // does, and it is included.
        let (tx, rx) = mpsc::channel();
        tx.send(Event::Changed(PathBuf::from(".git/HEAD"))).unwrap();
        tx.send(Event::Changed(PathBuf::from("Cargo.toml")))
            .unwrap();
        tx.send(Event::Changed(PathBuf::from("src/test_real.py")))
            .unwrap();
        drop(tx);
        let got = collect_changes(&rx, NO_DEBOUNCE, &mut Vec::new())
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![PathBuf::from("src/test_real.py")]);
    }

    #[test]
    fn collect_changes_errors_when_channel_closes_before_any_relevant() {
        // All senders gone with no relevant path -> recv() errors out rather
        // than looping forever.
        let (tx, rx) = mpsc::channel();
        tx.send(Event::Changed(PathBuf::from("README.md"))).unwrap();
        drop(tx);
        assert!(collect_changes(&rx, NO_DEBOUNCE, &mut Vec::new()).is_err());
    }

    #[test]
    fn collect_changes_returns_none_on_quit_before_a_change() {
        let (tx, rx) = mpsc::channel();
        tx.send(Event::Changed(PathBuf::from("notes.txt"))).unwrap();
        tx.send(Event::Quit).unwrap();
        assert!(collect_changes(&rx, NO_DEBOUNCE, &mut Vec::new())
            .unwrap()
            .is_none());
    }

    #[test]
    fn collect_changes_returns_none_on_quit_during_debounce() {
        // A quit landing in the debounce burst wins over the pending change.
        let (tx, rx) = mpsc::channel();
        tx.send(Event::Changed(PathBuf::from("test_a.py"))).unwrap();
        tx.send(Event::Changed(PathBuf::from("notes.txt"))).unwrap();
        tx.send(Event::Quit).unwrap();
        assert!(collect_changes(&rx, NO_DEBOUNCE, &mut Vec::new())
            .unwrap()
            .is_none());
    }

    #[test]
    fn drain_stale_empties_queue_keeping_relevant_paths_and_quit() {
        let (tx, rx) = mpsc::channel();
        tx.send(Event::Changed(PathBuf::from("test_a.py"))).unwrap();
        tx.send(Event::Changed(PathBuf::from("notes.txt"))).unwrap();
        let (quit, stale) = drain_stale(&rx);
        assert!(!quit, "plain changes are not a quit");
        assert_eq!(stale, vec![PathBuf::from("test_a.py")]);
        assert!(rx.try_recv().is_err(), "drain must empty the queue");

        tx.send(Event::Quit).unwrap();
        tx.send(Event::Changed(PathBuf::from("test_b.py"))).unwrap();
        assert!(drain_stale(&rx).0, "a quit queued during a run is kept");
    }

    #[test]
    fn next_change_set_hands_stale_paths_off_before_waiting() {
        // Mid-run events don't join the next change set, but they are handed
        // to `on_stale` (the graph cache) rather than lost.
        let (tx, rx) = mpsc::channel();
        tx.send(Event::Changed(PathBuf::from("test_stale.py")))
            .unwrap();
        let mut stale_seen = Vec::new();
        let mut waited = false;
        let feeder = tx.clone();
        let got = next_change_set(
            &rx,
            NO_DEBOUNCE,
            |paths| stale_seen.extend(paths),
            || {
                waited = true;
                feeder
                    .send(Event::Changed(PathBuf::from("test_fresh.py")))
                    .unwrap();
            },
        )
        .unwrap();
        assert!(waited);
        assert_eq!(stale_seen, vec![PathBuf::from("test_stale.py")]);
        assert_eq!(got, Some(vec![PathBuf::from("test_fresh.py")]));
    }

    #[test]
    fn next_change_set_quits_without_waiting_on_a_queued_quit() {
        let (tx, rx) = mpsc::channel();
        tx.send(Event::Quit).unwrap();
        let mut waited = false;
        let got = next_change_set(&rx, NO_DEBOUNCE, |_| {}, || waited = true).unwrap();
        assert!(got.is_none());
        assert!(!waited, "a quit during the run must not announce a wait");
    }

    #[test]
    fn waiting_message_offers_q_only_when_it_works() {
        let on = waiting_message(true, 1);
        assert!(
            on.contains("(q + Enter or Ctrl+C to quit, last exit: 1)"),
            "{on}"
        );
        let off = waiting_message(false, 0);
        assert!(off.contains("(Ctrl+C to quit, last exit: 0)"), "{off}");
        assert!(!off.contains("q +"), "{off}");
    }

    #[test]
    fn stdin_quit_is_off_when_the_worker_owns_stdin() {
        use clap::Parser;
        let cli = Cli::parse_from(["rstest"]);
        assert!(stdin_quit_enabled(&cli, &[], false));
        assert!(stdin_quit_enabled(&cli, &["-k".into(), "x".into()], false));
        for flag in ["--pdb", "--trace", "-s", "--capture=no"] {
            assert!(!stdin_quit_enabled(&cli, &[flag.into()], false), "{flag}");
        }
        let debug = Cli::parse_from(["rstest", "--debug"]);
        assert!(!stdin_quit_enabled(&debug, &[], false), "--debug");
    }

    #[test]
    fn stdin_quit_is_off_for_a_background_tty_job() {
        // `rstest --watch &`: a tty read would SIGTTIN-stop the whole process.
        use clap::Parser;
        let cli = Cli::parse_from(["rstest"]);
        assert!(!stdin_quit_enabled(&cli, &[], true));
        // Whatever this test's own stdin is, the probe must not stop or panic.
        let _ = stdin_is_background_tty();
    }

    #[test]
    fn test_only_rerun_still_updates_the_graph() {
        // A test-only change set bypasses the graph; the cache must still learn
        // about it, or the next source edit selects from a stale index.
        let cwd = fresh_dir("test-only-graph");
        std::fs::write(cwd.join("mymod.py"), "X = 1\n").unwrap();
        std::fs::write(
            cwd.join("test_uses.py"),
            "import mymod\ndef test_a(): assert mymod.X\n",
        )
        .unwrap();
        let project = project_at(&cwd);
        let mut cache = select::CollectionCache::new();
        let src = [cwd.join("mymod.py")];
        assert!(matches!(
            plan_rerun(&src, &project, &cwd, &[], &mut cache),
            Plan::Run { .. }
        ));

        // A new test importing mymod arrives as a test-only change set.
        let new_test = cwd.join("test_new.py");
        std::fs::write(&new_test, "import mymod\ndef test_b(): assert mymod.X\n").unwrap();
        assert!(matches!(
            plan_rerun(&[new_test], &project, &cwd, &[], &mut cache),
            Plan::Run {
                mode: "changed files",
                ..
            }
        ));

        match plan_rerun(&src, &project, &cwd, &[], &mut cache) {
            Plan::Run { args, mode } => {
                assert_eq!(mode, "affected tests");
                assert!(args.iter().any(|a| a.ends_with("test_new.py")), "{args:?}");
            }
            Plan::Skip => panic!("source edit must reach the new test"),
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn listen_for_quit_sends_quit_on_q_line() {
        for input in ["q\n", "  quit  \n", "hello\nq\nignored\n"] {
            let (tx, rx) = mpsc::channel();
            listen_for_quit(std::io::Cursor::new(input), &tx);
            assert!(matches!(rx.try_recv(), Ok(Event::Quit)), "{input:?}");
            assert!(rx.try_recv().is_err(), "one quit only: {input:?}");
        }
    }

    #[test]
    fn listen_for_quit_ignores_eof_and_other_input() {
        // EOF (stdin closed / /dev/null) must not end the watch session.
        for input in ["", "qq\nexit\n"] {
            let (tx, rx) = mpsc::channel();
            listen_for_quit(std::io::Cursor::new(input), &tx);
            assert!(rx.try_recv().is_err(), "{input:?}");
        }
    }

    #[test]
    fn listen_for_quit_stops_on_read_error() {
        // Non-UTF-8 input is a read error for `lines()`: stop listening quietly.
        let (tx, rx) = mpsc::channel();
        listen_for_quit(std::io::Cursor::new(b"\xff\xfe\nq\n".to_vec()), &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn directory_events_reach_the_graph_but_do_not_trigger_a_rerun() {
        let cwd = fresh_dir("dir-events");
        let pkg = cwd.join("newpkg");
        let cache_dir = cwd.join("__pycache__");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();
        let (tx, rx) = mpsc::channel();
        // Queued mid-run: a new package dir, an ignored dir, a deleted path.
        tx.send(Event::Changed(pkg.clone())).unwrap();
        tx.send(Event::Changed(cache_dir.clone())).unwrap();
        tx.send(Event::Changed(cwd.join("gone"))).unwrap();
        let mut noted = Vec::new();
        let feeder = tx.clone();
        let (pkg2, py) = (pkg.clone(), cwd.join("mod.py"));
        let got = next_change_set(
            &rx,
            NO_DEBOUNCE,
            |paths| noted.extend(paths),
            || {
                // While collecting: another dir event, then a real change.
                feeder.send(Event::Changed(pkg2)).unwrap();
                feeder.send(Event::Changed(py)).unwrap();
            },
        )
        .unwrap();
        assert_eq!(got, Some(vec![cwd.join("mod.py")]), "dirs never rerun");
        assert_eq!(noted, vec![pkg.clone(), pkg], "only unignored live dirs");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn relevant_accepts_python_and_pytest_config_files() {
        assert!(relevant(Path::new("src/test_x.py")));
        assert!(relevant(Path::new("conftest.py")));
        // Only pytest's config files count among toml/ini/cfg.
        assert!(relevant(Path::new("pyproject.toml")));
        assert!(relevant(Path::new("pytest.ini")));
        assert!(relevant(Path::new("tox.ini")));
        assert!(relevant(Path::new("setup.cfg")));
        assert!(relevant(Path::new("pytest.toml")));
        assert!(relevant(Path::new(".pytest.toml")));
        assert!(relevant(Path::new(".pytest.ini")));
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
        match plan_rerun(
            &[t1, t2],
            &project_at(&cwd),
            &cwd,
            &base,
            &mut select::CollectionCache::new(),
        ) {
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
            plan_rerun(
                &[gone],
                &project_at(&cwd),
                &cwd,
                &[],
                &mut select::CollectionCache::new()
            ),
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
        match plan_rerun(
            &[cfg],
            &project_at(&cwd),
            &cwd,
            &base,
            &mut select::CollectionCache::new(),
        ) {
            Plan::Run { args, mode } => {
                assert_eq!(mode, "full selection");
                assert_eq!(args, base, "full selection reruns with base args verbatim");
            }
            Plan::Skip => panic!("config change should force a full rerun"),
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn plan_orphan_source_change_skips() {
        // A source file no test imports resolves to an empty affected set ->
        // nothing to rerun.
        let cwd = fresh_dir("orphan");
        std::fs::write(cwd.join("orphan.py"), "VALUE = 1\n").unwrap();
        // A test that imports something else, so the graph maps orphan.py to no test.
        std::fs::write(
            cwd.join("test_other.py"),
            "def test_ok():\n    assert True\n",
        )
        .unwrap();
        assert!(
            matches!(
                plan_rerun(
                    &[cwd.join("orphan.py")],
                    &project_at(&cwd),
                    &cwd,
                    &[],
                    &mut select::CollectionCache::new()
                ),
                Plan::Skip
            ),
            "an orphan source change must skip"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn plan_mixed_test_and_source_routes_through_graph() {
        // A change set mixing a test file and a source file is NOT "only tests",
        // so it goes through import-graph selection rather than a direct rerun.
        let cwd = fresh_dir("mixed");
        std::fs::write(cwd.join("mymod.py"), "VALUE = 1\n").unwrap();
        std::fs::write(
            cwd.join("test_uses.py"),
            "import mymod\ndef test_v():\n    assert mymod.VALUE == 1\n",
        )
        .unwrap();
        let changed = vec![cwd.join("mymod.py"), cwd.join("test_uses.py")];
        match plan_rerun(
            &changed,
            &project_at(&cwd),
            &cwd,
            &[],
            &mut select::CollectionCache::new(),
        ) {
            Plan::Run { mode, .. } => {
                assert_eq!(mode, "affected tests", "mixed set must use the graph");
            }
            Plan::Skip => panic!("mixed change reaching a test must run it"),
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn plan_partial_deleted_test_files_runs_survivors() {
        // Only-test change set where some files are gone: the deleted ones are
        // filtered out and the survivors still run.
        let cwd = fresh_dir("partial");
        let live = cwd.join("test_live.py");
        let gone = cwd.join("test_gone.py"); // never created
        std::fs::write(&live, "def test_x(): pass\n").unwrap();
        match plan_rerun(
            &[gone, live],
            &project_at(&cwd),
            &cwd,
            &[],
            &mut select::CollectionCache::new(),
        ) {
            Plan::Run { args, mode } => {
                assert_eq!(mode, "changed files");
                assert!(args.contains(&"test_live.py".to_string()), "{args:?}");
                assert!(
                    !args.contains(&"test_gone.py".to_string()),
                    "deleted file must be dropped: {args:?}"
                );
            }
            Plan::Skip => panic!("a surviving test file must still run"),
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
        match plan_rerun(
            &[cwd.join("mymod.py")],
            &project_at(&cwd),
            &cwd,
            &base,
            &mut select::CollectionCache::new(),
        ) {
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
