//! `rstest bisect <nodeid>`: find the *polluter* — the earlier test(s) whose
//! state leak makes the target fail only when run after them.
//!
//! Order-dependency is the hardest flake class: "it only fails when run after
//! some other test." We delta-debug the predecessor set at `-n 0` (serial, so
//! this isolates ORDERING, not concurrency): repeatedly run the victim preceded
//! by a subset of the earlier tests, minimizing (Zeller/Hildebrandt `ddmin`)
//! toward the 1-minimal set of predecessors that still reproduces the failure.
//!
//! Reuses the shared child-session runner ([`run_session`] at `-n 0`) and the
//! collection-order id list ([`collect_session`]); the minimization and the
//! reproduce primitive are the new logic here. Where the suite is rooted comes
//! from pytest itself (the worker reports `config.rootpath`), never guessed.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use super::{collect_session, file_of, run_session, run_session_seq, Collected, Outcomes, Phase};
use crate::config;
use crate::reporting::sink::Sink;
use crate::text::strip_verbatim;

/// Max child `-n 0` runs a bisect will spend before reporting the smallest
/// reproducing set found so far. ddmin is worst-case quadratic; this bounds a
/// pathological suite to a predictable wall-time.
const DEFAULT_BUDGET: u32 = 80;

/// bisect's model is "predecessors in collection order, victim last". A
/// reordering plugin breaks it: pytest-randomly shuffles every session by
/// default, so a real polluter could run after the victim and look harmless.
/// Disabled in the collection, every child run and the reproduce command
/// (`-p no:` of an uninstalled plugin is a no-op).
const ORDER_PINS: [&str; 2] = ["-p", "no:randomly"];

/// The pins every bisect session gets, the collection and each child run:
/// `ORDER_PINS`, plus a private cache of its own, new and empty for every
/// session. pytest's cacheprovider reorders or filters by the cache
/// (`--ff`/`--lf`, often set in `addopts`): with an empty cache there is
/// nothing to act on, a child's own failure can't move the victim ahead in the
/// next child, and the user's `.pytest_cache` is never rewritten. A fresh
/// directory rather than `--cache-clear`, which only exists while the
/// cacheprovider is loaded. With it disabled (`-p no:cacheprovider`) the
/// `cache_dir` override is inert: pytest flags the unknown option (an end-of-
/// session error under `--strict-config`) but still collects and runs, and the
/// outcomes are all bisect reads.
struct Pins {
    cache_root: PathBuf,
}

impl Pins {
    /// The session pins, then `args`.
    fn session(&self, args: &[String]) -> Vec<String> {
        let dir = self.cache_root.join(format!("s{}", run_session_seq()));
        let mut out: Vec<String> = ORDER_PINS.iter().map(|a| a.to_string()).collect();
        out.push("-o".into());
        out.push(format!("cache_dir={}", dir.display()));
        out.extend_from_slice(args);
        out
    }
}

/// Order flags no command-line option can switch back off: `--nf` sorts every
/// item by file mtime, and stepwise stops at the first failure. bisect refuses
/// them rather than report a verdict about a different order.
const BLOCKING_ORDER_FLAGS: [&str; 3] = ["--nf", "--sw", "--sw-skip"];

/// Order flags whose effect comes from the cache (`--ff`, `--lf`): harmless in
/// bisect's own runs (private empty cache), but a pasted reproduce command
/// would read the user's cache, so it gets a fresh one too.
fn needs_fresh_cache(order_flags: &[String]) -> bool {
    order_flags.iter().any(|f| f == "--ff" || f == "--lf")
}

/// Pin a child run to the collection's rootdir, config file and conftest
/// cutoff. A child re-derives all three from its own selection, so a subset
/// under a nested config (`pkg/pytest.ini`) would key its results off `pkg/`
/// (the victim then reads as "did not run") and load a different config.
/// `-c` also moves pytest's `confcutdir` to the config file's directory, so it
/// is pinned back to what the collection had: the loaded config's directory,
/// else the rootdir (never the stand-in's temp dir, which would let conftest
/// lookup wander the whole temp tree, seconds per run).
pub(super) fn child_pins(rootdir: &Path, inifile: &Path, confcutdir: &Path) -> Vec<String> {
    vec![
        "--rootdir".into(),
        rootdir.display().to_string(),
        "-c".into(),
        inifile.display().to_string(),
        "--confcutdir".into(),
        confcutdir.display().to_string(),
    ]
}

/// Lifts `-x`/`--maxfail`, which would stop a run at an unrelated earlier
/// failure before the victim runs. The last `--maxfail` wins, so this goes
/// after `addopts` and after the user's own args alike.
pub(super) const MAXFAIL_LIFT: &str = "--maxfail=0";

/// A temp directory removed (with its contents) on drop.
pub(super) struct TempDir(PathBuf);

impl TempDir {
    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A private scratch directory, removed when dropped. bisect keeps the
/// sessions' cache (`cache/`) and, when needed, the stand-in config there;
/// the parallel classifier uses it for the stand-in config only.
pub(super) fn workdir() -> Result<TempDir> {
    let dir = std::env::temp_dir().join(format!(
        "rstest-bisect-{}-{}",
        std::process::id(),
        run_session_seq()
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(TempDir(dir))
}

/// The config file child runs pin with `-c`: the one pytest loaded, or, when
/// none was in effect, an empty stand-in (under `work`) so a child can't pick
/// up a nested one. The stand-in gets a directory of its own, so nothing
/// unrelated sits beside a config pytest loads (the conftest cutoff is pinned
/// separately, see [`child_pins`]).
pub(super) fn pinned_inifile(inifile: Option<&str>, work: &Path) -> Result<PathBuf> {
    if let Some(ini) = inifile {
        return Ok(PathBuf::from(ini));
    }
    let dir = work.join("config");
    std::fs::create_dir_all(&dir)?;
    let blank = dir.join("pytest.ini");
    std::fs::write(&blank, "[pytest]\n")?;
    Ok(blank)
}

/// Did the victim fail in this run? A victim absent from the run never ran (a
/// deselecting `-k`/`-m`, a collection error, a crashed worker, a session cut
/// short). That is not a pass: reading it as one would steer the search to a
/// wrong verdict, so error.
fn victim_failed(out: &Outcomes, victim: &str) -> Result<bool> {
    match out.get(victim).map(|r| r.phase) {
        Some(Phase::Fail) => Ok(true),
        Some(Phase::Pass) => Ok(false),
        None => bail!(
            "rstest bisect: {victim} did not run in the child session (deselected \
             by a pytest option such as -k/-m, a collection error, a crashed \
             worker, or a plugin that ends the session early)"
        ),
    }
}

/// The reproduce oracle's context: the interpreter, where rootdir-relative
/// nodeids resolve, the user's pytest options, and the remaining run budget.
struct Runner<'a> {
    python: &'a Path,
    victim: &'a str,
    rootdir: &'a Path,
    pins: &'a Pins,
    pytest_args: &'a [String],
    budget: u32,
}

impl Runner<'_> {
    /// Run `-n 0` over `preds` followed by the victim (victim LAST so every
    /// predecessor runs before it) and report whether the victim failed.
    /// `None` when the budget is spent: nothing ran, the search must stop.
    fn reproduces(&mut self, preds: &[String]) -> Result<Option<bool>> {
        if self.budget == 0 {
            return Ok(None);
        }
        self.budget -= 1;
        let mut sel: Vec<String> = preds.to_vec();
        sel.push(self.victim.to_string());
        let args = self.pins.session(self.pytest_args);
        let out = run_selection(self.python, self.rootdir, &args, &sel)?;
        victim_failed(&out, self.victim).map(Some)
    }
}

/// The `@argsfile` body selecting exactly `sel`, in order. Collected nodeids
/// are rootdir-relative but a child resolves them from the cwd, so each goes
/// absolute (the report keys them back rootdir-relative). One per line, so a
/// param id with spaces stays one argument.
fn argsfile_body(rootdir: &Path, sel: &[String]) -> String {
    sel.iter()
        .map(|id| format!("{}\n", rootdir.join(id).display()))
        .collect()
}

/// Run exactly `sel` (rootdir-relative nodeids) at `-n 0`. The selection rides
/// a pytest `@argsfile`, not argv: a victim late in a large suite has a prefix
/// of thousands of ids, past the OS argv limit.
fn run_selection(
    python: &Path,
    rootdir: &Path,
    pytest_args: &[String],
    sel: &[String],
) -> Result<Outcomes> {
    let file = std::env::temp_dir().join(format!(
        "rstest-bisect-sel-{}-{}.txt",
        std::process::id(),
        run_session_seq()
    ));
    std::fs::write(&file, argsfile_body(rootdir, sel))?;
    let mut args = pytest_args.to_vec();
    args.push(format!("@{}", file.display()));
    let out = run_session(python, &["-n", "0"], &args);
    let _ = std::fs::remove_file(&file);
    out
}

/// Delta-debugging minimization (Zeller & Hildebrandt `ddmin`) of the
/// predecessor set: return a 1-minimal subset of `preds` that still reproduces
/// (`interesting`) the victim's failure. `interesting` is the reproduce oracle;
/// `None` from it means the run budget is spent, and ddmin stops at once with
/// the smallest set confirmed so far. The flag reports that early stop (the set
/// may then not be 1-minimal).
fn ddmin(
    preds: &[String],
    mut interesting: impl FnMut(&[String]) -> Result<Option<bool>>,
) -> Result<(Vec<String>, bool)> {
    let mut circ: Vec<String> = preds.to_vec();
    let mut n = 2usize;
    while circ.len() >= 2 {
        let chunk = circ.len().div_ceil(n);
        let subsets: Vec<Vec<String>> = circ.chunks(chunk).map(|c| c.to_vec()).collect();

        // (1) any single subset reproduces alone -> narrow to it, reset n=2.
        let mut reduced = false;
        for s in &subsets {
            match interesting(s)? {
                None => return Ok((circ, true)),
                Some(true) => {
                    circ = s.clone();
                    n = 2;
                    reduced = true;
                    break;
                }
                Some(false) => {}
            }
        }
        if reduced {
            continue;
        }

        // (2) any complement (all but one subset) reproduces -> drop that
        // subset, decrease granularity by one. With two subsets each
        // complement IS the other subset, already asked in (1): skip, or the
        // budget pays twice for the same run.
        if subsets.len() > 2 {
            for i in 0..subsets.len() {
                let complement: Vec<String> = subsets
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .flat_map(|(_, s)| s.iter().cloned())
                    .collect();
                match interesting(&complement)? {
                    None => return Ok((circ, true)),
                    Some(true) => {
                        circ = complement;
                        n = (n - 1).max(2);
                        reduced = true;
                        break;
                    }
                    Some(false) => {}
                }
            }
            if reduced {
                continue;
            }
        }

        // (3) neither: increase granularity, or stop when already 1-per-chunk.
        if n >= circ.len() {
            break;
        }
        n = (2 * n).min(circ.len());
    }
    Ok((circ, false))
}

/// Canonical form of a path pytest reported, so it compares equal to the
/// canonical cwd (macOS `/var` vs `/private/var`, Windows `\\?\`).
fn canonical(p: &Path) -> PathBuf {
    strip_verbatim(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
}

/// Collect the suite the way a no-arg run FROM THE ROOTDIR would. From a
/// subdirectory pytest collects only that subtree (ini `testpaths` apply only
/// at the rootdir), which would hide a polluter living elsewhere. So when the
/// first collection came from the invocation dir and that isn't the rootdir,
/// collect again over the roots pytest reported (`testpaths`, else the
/// rootdir). Errors when pytest reported no rootdir (an old worker).
///
/// Also returns where a test selection among the session args came from, if
/// any (see [`Selection`]). pytest itself says whether it saw positional args,
/// so there is no guessing at which tokens are flag values.
fn collect_suite(
    python: &Path,
    pins: &Pins,
    pytest_args: &[String],
    cwd: &Path,
) -> Result<(Collected, PathBuf, Selection)> {
    let first = collect_session(python, &pins.session(pytest_args))?;
    let Some(root) = first.rootdir.as_deref().map(|r| canonical(Path::new(r))) else {
        bail!(
            "rstest bisect: the collection failed (pytest reported no rootdir); \
             run `rstest --collect-only` with the same pytest args to see why"
        );
    };
    let selection = if first.args_source.as_deref() != Some("args") {
        Selection::None
    } else if pytest_args.is_empty() {
        Selection::Config
    } else {
        // pytest folds ini `addopts` and `PYTEST_ADDOPTS` in ahead of the
        // command line, so "args" alone can't say who named the paths. Ask
        // again without the user's args: still "args" means the config did.
        let bare = collect_session(python, &pins.session(&[]))?;
        if bare.args_source.as_deref() == Some("args") {
            Selection::Config
        } else {
            Selection::User
        }
    };
    if first.args_source.as_deref() != Some("invocation_dir") || root == cwd {
        return Ok((first, root, selection));
    }
    let mut args = pins.session(pytest_args);
    args.extend(first.root_args.iter().cloned());
    let full = collect_session(python, &args)?;
    let root = full
        .rootdir
        .as_deref()
        .map(|r| canonical(Path::new(r)))
        .unwrap_or(root);
    Ok((full, root, selection))
}

/// Where a test selection among the collection's args came from. Either kind
/// would be unioned into every child's selection (pytest adds positional args
/// together), running extra tests between the culprits, so bisect refuses both
/// and names the real source.
#[derive(Debug, PartialEq, Eq)]
enum Selection {
    None,
    /// The user's pytest args after `--`.
    User,
    /// Ini `addopts` or `PYTEST_ADDOPTS`.
    Config,
}

/// Match the user's nodeid to a collected one. Collected ids are
/// rootdir-relative; a user in a subdirectory types them cwd-relative
/// (`test_a.py::t` from `tests/`). The cwd-relative reading comes first, as
/// pytest would read it from there: a same-named `test_a.py` at the rootdir
/// must not win. The rootdir-relative form is the fallback.
fn resolve_nodeid(nodeid: &str, ids: &[String], cwd: &Path, rootdir: &Path) -> Option<String> {
    let from_cwd = || -> Option<String> {
        let file = file_of(nodeid);
        let rest = &nodeid[file.len()..];
        let abs = strip_verbatim(cwd.join(file).canonicalize().ok()?);
        let rel: Vec<String> = abs
            .strip_prefix(rootdir)
            .ok()?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let cand = format!("{}{rest}", rel.join("/"));
        ids.contains(&cand).then_some(cand)
    };
    from_cwd().or_else(|| ids.iter().find(|id| *id == nodeid).cloned())
}

/// A rootdir-relative nodeid re-expressed relative to `cwd` (`../b/t.py::x`),
/// the form pytest accepts from where the user ran bisect. Keeps the reproduce
/// command runnable in place, so relative pytest options stay valid. Absolute
/// when the two share no root (another Windows drive).
fn cwd_relative(cwd: &Path, rootdir: &Path, id: &str) -> String {
    let file = file_of(id);
    let rest = &id[file.len()..];
    let abs = rootdir.join(file);
    let here: Vec<_> = cwd.components().collect();
    let there: Vec<_> = abs.components().collect();
    let common = here.iter().zip(&there).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return format!("{}{rest}", abs.display());
    }
    let mut parts: Vec<String> = vec!["..".to_string(); here.len() - common];
    parts.extend(
        there[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    format!("{}{rest}", parts.join("/"))
}

/// Quote `s` for a POSIX shell when it carries anything beyond a safe set, so
/// the printed reproduce command pastes as-is (`[param]` globs in zsh, spaces
/// split everywhere).
fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_-./:=+,@%".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// How the reproduce command is spelled: run from `cwd`, with the user's own
/// `--python` (when they gave one) and pytest options.
struct ReproCtx<'a> {
    cwd: &'a Path,
    rootdir: &'a Path,
    /// The config file pytest loaded for the suite, when there was one.
    inifile: Option<&'a Path>,
    /// `--ff`/`--lf` are active: give the command a fresh cache of its own.
    fresh_cache: bool,
    /// `-x`/`--maxfail` is active: lift it, as the child runs did (a culprit
    /// that fails its own assertions would otherwise stop the run early).
    lift_maxfail: bool,
    python_flag: Option<&'a str>,
    pytest_args: &'a [String],
}

/// Does a file under `rootdir` sit below something that would re-root a plain
/// run of it: a nested pytest config, or a `setup.py` (pytest's rootdir marker
/// when no config file is found), between its directory and the rootdir? A
/// `pytest.ini`/`pytest.toml` counts even when empty: pytest always takes those
/// as the config file (a common bare rootdir marker).
fn under_nested_config(rootdir: &Path, file: &Path) -> bool {
    const ALWAYS_CONFIG: [&str; 5] = [
        "pytest.ini",
        ".pytest.ini",
        "pytest.toml",
        ".pytest.toml",
        "setup.py",
    ];
    file.parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|dir| *dir != rootdir && dir.starts_with(rootdir))
        .any(|dir| {
            ALWAYS_CONFIG.iter().any(|f| dir.join(f).is_file())
                || config::has_pytest_config(dir, &mut std::io::sink())
        })
}

/// An always-present empty file for `-c` in a printed command, where bisect's
/// own stand-in (deleted on exit) can't be referenced. pytest reads no config
/// from a file without a known suffix.
const NULL_CONFIG: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// The minimal reproducing command: the culprits then the victim at `-n 0`,
/// order-pinned. Runnable from where bisect ran: ids are cwd-relative, options
/// verbatim. When a repro file sits where a plain run would re-root (see
/// [`under_nested_config`]) it also pins what the child runs had: the rootdir,
/// and the loaded config, or with none an empty config plus the conftest
/// cutoff at the rootdir.
fn repro_command(ctx: &ReproCtx, culprits: &[String], victim: &str) -> String {
    let mut parts = vec!["rstest".to_string(), "-n".into(), "0".into()];
    if let Some(py) = ctx.python_flag {
        parts.push("--python".into());
        parts.push(shell_quote(py));
    }
    parts.extend(ORDER_PINS.iter().map(|a| a.to_string()));
    if ctx.fresh_cache {
        // Cleared on every run, so re-running the command stays idempotent.
        let cache = std::env::temp_dir().join("rstest-bisect-repro-cache");
        parts.push("-o".into());
        parts.push(shell_quote(&format!("cache_dir={}", cache.display())));
        parts.push("--cache-clear".into());
    }
    let nested = culprits
        .iter()
        .map(String::as_str)
        .chain([victim])
        .any(|id| under_nested_config(ctx.rootdir, &ctx.rootdir.join(file_of(id))));
    if nested {
        parts.push("--rootdir".into());
        parts.push(shell_quote(&ctx.rootdir.display().to_string()));
        match ctx.inifile {
            Some(ini) => {
                parts.push("-c".into());
                parts.push(shell_quote(&ini.display().to_string()));
            }
            None => {
                parts.push("-c".into());
                parts.push(NULL_CONFIG.into());
                parts.push("--confcutdir".into());
                parts.push(shell_quote(&ctx.rootdir.display().to_string()));
            }
        }
    }
    parts.extend(ctx.pytest_args.iter().map(|a| shell_quote(a)));
    if ctx.lift_maxfail {
        parts.push(MAXFAIL_LIFT.into());
    }
    for id in culprits.iter().map(String::as_str).chain([victim]) {
        parts.push(shell_quote(&cwd_relative(ctx.cwd, ctx.rootdir, id)));
    }
    parts.join(" ")
}

/// Run the order-dependency bisect. Exit code: 0 = order-dependent culprit(s)
/// found, 1 = not order-dependent (fails in isolation, or doesn't reproduce in
/// collection order), 2 = the nodeid isn't in the suite or `pytest_args`
/// carries a test selection. `python_flag` is the user's own `--python`,
/// echoed into the reproduce command; `python` is the resolved interpreter.
pub fn run_bisect(
    python: &Path,
    python_flag: Option<&str>,
    nodeid: &str,
    pytest_args: &[String],
    json_path: Option<&Path>,
    sink: &mut Sink,
) -> Result<i32> {
    // A stale doc from an earlier run must never read as this run's result:
    // drop it up front, and record a failed run (exit 2 or an error) as such.
    if let Some(p) = json_path {
        let _ = std::fs::remove_file(p);
    }
    match bisect(python, python_flag, nodeid, pytest_args, json_path, sink) {
        Ok(Verdict::Done(code)) => Ok(code),
        Ok(Verdict::Refused(msg)) => {
            sink.out_line(&msg);
            write_error_json(json_path, nodeid, &msg)?;
            Ok(2)
        }
        Err(e) => {
            let _ = write_error_json(json_path, nodeid, &e.to_string());
            Err(e)
        }
    }
}

/// How [`bisect`] ended: with an exit code (its doc already written), or
/// refused before any verdict (exit 2), with the message saying why.
enum Verdict {
    Done(i32),
    Refused(String),
}

fn bisect(
    python: &Path,
    python_flag: Option<&str>,
    nodeid: &str,
    pytest_args: &[String],
    json_path: Option<&Path>,
    sink: &mut Sink,
) -> Result<Verdict> {
    let cwd = strip_verbatim(std::env::current_dir()?.canonicalize()?);
    let work = workdir()?;
    let pins = Pins {
        cache_root: work.0.join("cache"),
    };

    sink.warn(&format!(
        "rstest bisect: collecting the suite to locate {nodeid}…"
    ));
    let (collected, rootdir, selection) = collect_suite(python, &pins, pytest_args, &cwd)?;
    match selection {
        Selection::None => {}
        Selection::User => {
            return Ok(Verdict::Refused(
                "rstest bisect: the pytest args after `--` include a test selection \
                 (a path or nodeid). Pass only pytest options there; bisect selects \
                 tests by nodeid itself."
                    .into(),
            ));
        }
        Selection::Config => {
            return Ok(Verdict::Refused(
                "rstest bisect: the ini `addopts` (or PYTEST_ADDOPTS) names test \
                 paths, which pytest adds to every run, so bisect can't run an exact \
                 selection. Override it with the options alone, e.g. \
                 `rstest bisect <nodeid> -- -o addopts=\"<options only>\"`, or unset \
                 PYTEST_ADDOPTS."
                    .into(),
            ));
        }
    }
    let blocking: Vec<&str> = collected
        .order_flags
        .iter()
        .map(String::as_str)
        .filter(|f| BLOCKING_ORDER_FLAGS.contains(f))
        .collect();
    if !blocking.is_empty() {
        return Ok(Verdict::Refused(format!(
            "rstest bisect: {} reorders or cuts short every run, and no option can \
             switch it back off, so the victim wouldn't reliably run last. Drop it \
             for the bisect: remove it from the args after `--`, override the ini \
             with `-- -o addopts=\"<options without it>\"`, or unset PYTEST_ADDOPTS.",
            blocking.join(" / ")
        )));
    }
    let ids = &collected.ids;
    let Some(victim) = resolve_nodeid(nodeid, ids, &cwd, &rootdir) else {
        return Ok(Verdict::Refused(format!(
            "rstest bisect: {nodeid} was not collected. Check the nodeid \
             (path::test[param]) and that it isn't deselected."
        )));
    };
    let idx = ids.iter().position(|id| *id == victim).unwrap_or_default();
    let victim = victim.as_str();
    let ini = pinned_inifile(collected.inifile.as_deref(), &work.0)?;
    let ctx = ReproCtx {
        cwd: &cwd,
        rootdir: &rootdir,
        inifile: collected.inifile.as_deref().map(Path::new),
        fresh_cache: needs_fresh_cache(&collected.order_flags),
        lift_maxfail: collected.order_flags.iter().any(|f| f == "--maxfail"),
        python_flag,
        pytest_args,
    };

    // The cutoff pytest actually used (a user's own `--confcutdir` included;
    // pytest always sets one), the rootdir if a worker ever omits it.
    let confcutdir = collected
        .confcutdir
        .clone()
        .map(PathBuf::from)
        .unwrap_or(rootdir.clone());
    let mut child_args = child_pins(&rootdir, &ini, &confcutdir);
    child_args.extend_from_slice(pytest_args);
    child_args.push(MAXFAIL_LIFT.into());
    let mut runner = Runner {
        python,
        victim,
        rootdir: &rootdir,
        pins: &pins,
        pytest_args: &child_args,
        budget: DEFAULT_BUDGET,
    };

    // Isolation check: a test that fails ALONE isn't order-dependent — it's a
    // plain failure, and no predecessor set is the culprit.
    if runner.reproduces(&[])? == Some(true) {
        sink.out_line(&format!(
            "\n{victim}\n  fails in isolation (`{}`) — this is a plain \
             failure, not an order dependency. Fix the test itself.",
            repro_command(&ctx, &[], victim)
        ));
        write_json(json_path, victim, &rootdir, &cwd, &[], None)?;
        return Ok(Verdict::Done(1));
    }

    let preds: Vec<String> = ids[..idx].to_vec();
    if preds.is_empty() {
        sink.out_line(&format!(
            "\n{victim}\n  is first in collection order — nothing runs before it, so there is \
             no polluter to bisect. If it still flakes, it's parallel-only (try \
             `rstest migrate-check`)."
        ));
        write_json(json_path, victim, &rootdir, &cwd, &[], None)?;
        return Ok(Verdict::Done(1));
    }

    // Baseline: does the victim fail when the whole preceding suite runs before
    // it (collection order)? If not, there's nothing to minimize here.
    sink.warn(&format!(
        "rstest bisect: reproducing with all {} preceding test(s)…",
        preds.len()
    ));
    if runner.reproduces(&preds)? != Some(true) {
        sink.out_line(&format!(
            "\n{victim}\n  passes at -n 0 after all {} preceding tests — the failure does not \
             reproduce from collection order alone. It is likely parallel-only \
             (concurrency, not ordering); run `rstest migrate-check`.",
            preds.len()
        ));
        write_json(json_path, victim, &rootdir, &cwd, &[], None)?;
        return Ok(Verdict::Done(1));
    }

    sink.warn("rstest bisect: delta-debugging the predecessor set…");
    let (culprits, capped) = ddmin(&preds, |subset| runner.reproduces(subset))?;

    let repro = repro_command(&ctx, &culprits, victim);
    print_report(sink, victim, &culprits, &repro, capped);
    write_json(json_path, victim, &rootdir, &cwd, &culprits, Some(&repro))?;
    Ok(Verdict::Done(0))
}

/// Print the human culprit report. `capped` notes that the run budget ran out
/// mid-search, so the set may not be 1-minimal.
fn print_report(sink: &mut Sink, victim: &str, culprits: &[String], repro: &str, capped: bool) {
    sink.out_line("\n================= rstest bisect =================");
    sink.out_line(&format!("  victim:   {victim}"));
    sink.out_line(&format!(
        "  culprit{}: {} predecessor test(s) reproduce the failure:",
        if culprits.len() == 1 { "" } else { "s" },
        culprits.len()
    ));
    for c in culprits {
        sink.out_line(&format!("    {c}"));
    }
    if capped {
        sink.out_line(
            "  (run budget reached — this is the smallest reproducing set found, \
             may not be 1-minimal)",
        );
    }
    sink.out_line("\n  Minimal reproducing order (serial):");
    sink.out_line(&format!("    {repro}"));
    sink.out_line(
        "\n  Fix: the culprit leaks state the victim depends on being clean \
         (a module global, a monkeypatch not undone, a cached singleton, an \
         env var, a temp file). Reset it in the culprit's teardown, or make \
         the victim set up its own state.",
    );
    sink.out_line("================================================");
}

/// Write the `--bisect-json` document for a run that ended without a verdict
/// (refused, or an error), when a path was given: no culprits, no command, and
/// the reason in `error`.
fn write_error_json(json_path: Option<&Path>, nodeid: &str, error: &str) -> Result<()> {
    let Some(path) = json_path else {
        return Ok(());
    };
    let doc = serde_json::json!({
        "meta": { "runner": "rstest", "kind": "bisect", "schema": 1 },
        "nodeid": nodeid,
        "order_dependent": false,
        "culprits": [],
        "reproduce_command": null,
        "error": error,
    });
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

/// Write the `--bisect-json` document, when a path was given. `reproduce` is
/// the command when the victim is order-dependent, `None` otherwise. Nodeids
/// are relative to `rootdir`; the command runs from `cwd`.
fn write_json(
    json_path: Option<&Path>,
    victim: &str,
    rootdir: &Path,
    cwd: &Path,
    culprits: &[String],
    reproduce: Option<&str>,
) -> Result<()> {
    let Some(path) = json_path else {
        return Ok(());
    };
    let doc = serde_json::json!({
        "meta": { "runner": "rstest", "kind": "bisect", "schema": 1 },
        "nodeid": victim,
        "rootdir": rootdir.to_string_lossy(),
        "cwd": cwd.to_string_lossy(),
        "order_dependent": reproduce.is_some(),
        "culprits": culprits,
        "reproduce_command": reproduce,
    });
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// An oracle that never runs out of budget.
    fn contains(needles: &'static [&'static str]) -> impl FnMut(&[String]) -> Result<Option<bool>> {
        move |s| Ok(Some(needles.iter().all(|n| s.iter().any(|x| x == n))))
    }

    #[test]
    fn ddmin_finds_the_single_polluter() {
        // Interesting iff the subset contains "poison". ddmin must shrink a
        // 6-element predecessor list to exactly ["poison"].
        let preds = ids(&["a", "b", "poison", "c", "d", "e"]);
        let (out, capped) = ddmin(&preds, contains(&["poison"])).unwrap();
        assert_eq!(out, ids(&["poison"]));
        assert!(!capped);
    }

    #[test]
    fn ddmin_keeps_an_interacting_pair() {
        // Interesting only when BOTH "p" and "q" are present (a two-test
        // interaction). ddmin can't drop either, so both survive.
        let preds = ids(&["p", "x", "y", "q", "z"]);
        let (out, _) = ddmin(&preds, contains(&["p", "q"])).unwrap();
        assert!(out.contains(&"p".to_string()) && out.contains(&"q".to_string()));
        // and it minimized away the irrelevant tests.
        assert!(!out.contains(&"x".to_string()));
    }

    #[test]
    fn ddmin_single_element_is_returned_as_is() {
        let preds = ids(&["only"]);
        let (out, capped) = ddmin(&preds, |s| Ok(Some(!s.is_empty()))).unwrap();
        assert_eq!(out, ids(&["only"]));
        assert!(!capped);
    }

    #[test]
    fn ddmin_propagates_oracle_errors() {
        let preds = ids(&["a", "b"]);
        let r = ddmin(&preds, |_| Err(anyhow::anyhow!("boom")));
        assert!(r.is_err());
    }

    #[test]
    fn ddmin_propagates_errors_from_the_complement_pass() {
        // Subsets are never interesting on their own; the first complement
        // query errors. The error must surface, not be swallowed as "false".
        // 3 items: n=2 asks 2 subsets (no complements at two chunks), n=3
        // asks 3 singletons, then the first complement (call 6) errors.
        let preds = ids(&["a", "b", "c"]);
        let mut calls = 0;
        let r = ddmin(&preds, |_| {
            calls += 1;
            if calls > 5 {
                Err(anyhow::anyhow!("boom"))
            } else {
                Ok(Some(false))
            }
        });
        assert!(r.is_err());
        assert_eq!(calls, 6);
    }

    #[test]
    fn ddmin_never_asks_the_same_set_twice_at_two_chunks() {
        // A pair split across the halves: at n=2 the complements equal the
        // halves already asked, so asking them again only burns budget.
        let preds = ids(&["p", "a", "b", "c", "q", "d"]);
        let mut asked: Vec<Vec<String>> = Vec::new();
        let mut oracle = contains(&["p", "q"]);
        let (out, _) = ddmin(&preds, |s| {
            asked.push(s.to_vec());
            oracle(s)
        })
        .unwrap();
        assert_eq!(out, ids(&["p", "q"]));
        // The first two asks are the halves; neither is re-asked right after.
        assert_eq!(asked[0], ids(&["p", "a", "b"]));
        assert_eq!(asked[1], ids(&["c", "q", "d"]));
        assert_ne!(asked[2], asked[1]);
        assert_ne!(asked[2], asked[0]);
    }

    #[test]
    fn ddmin_empty_input_asks_nothing() {
        let (out, capped) = ddmin(&[], |_| panic!("oracle must not run")).unwrap();
        assert!(out.is_empty());
        assert!(!capped);
    }

    #[test]
    fn ddmin_never_interesting_returns_the_input() {
        // Nothing smaller reproduces: ddmin stops at full granularity and
        // returns the set it started from, not flagged as capped.
        let preds = ids(&["a", "b", "c", "d"]);
        let (out, capped) = ddmin(&preds, |_| Ok(Some(false))).unwrap();
        assert_eq!(out, preds);
        assert!(!capped);
    }

    #[test]
    fn ddmin_stops_at_once_when_the_budget_is_spent() {
        // The oracle narrows once, then runs dry. ddmin must return the
        // narrowed set flagged capped, and never ask again after `None`.
        let preds: Vec<String> = (0..16).map(|i| format!("t{i}")).collect();
        let mut calls = 0;
        let (out, capped) = ddmin(&preds, |_| {
            calls += 1;
            assert!(calls <= 2, "oracle asked after the budget ran out");
            Ok((calls == 1).then_some(true))
        })
        .unwrap();
        assert!(capped);
        assert_eq!(out, preds[..8].to_vec());
    }

    #[test]
    fn ddmin_spending_the_last_run_on_a_finished_search_is_not_capped() {
        // Budget exactly large enough: the last run finishes the search, so
        // the budget hits 0 without ddmin ever seeing `None`. The result is
        // 1-minimal and must not be flagged capped.
        let preds = ids(&["a", "poison"]);
        let mut budget: u32 = 2;
        let mut oracle = contains(&["poison"]);
        let (out, capped) = ddmin(&preds, |s| {
            budget = budget.checked_sub(1).expect("asked past the budget");
            oracle(s)
        })
        .unwrap();
        assert_eq!(out, ids(&["poison"]));
        assert!(!capped);
        assert_eq!(budget, 0);
    }

    #[test]
    fn ddmin_stops_when_the_budget_runs_out_in_the_complement_pass() {
        // 4 items: n=2 asks 2 halves, n=4 asks 4 singletons (all no), then the
        // first complement finds the budget spent. ddmin must stop right
        // there, capped, with the set it had.
        let preds = ids(&["a", "b", "c", "d"]);
        let mut calls = 0;
        let (out, capped) = ddmin(&preds, |_| {
            calls += 1;
            Ok(if calls <= 6 { Some(false) } else { None })
        })
        .unwrap();
        assert!(capped);
        assert_eq!(out, preds);
        assert_eq!(calls, 7, "no ask after the budget ran out");
    }

    #[test]
    fn ddmin_finds_the_last_polluter_in_a_long_list() {
        let names: Vec<String> = (0..37).map(|i| format!("t{i}")).collect();
        let (out, _) = ddmin(&names, contains(&["t36"])).unwrap();
        assert_eq!(out, ids(&["t36"]));
    }

    #[test]
    fn ddmin_pair_is_exactly_one_minimal() {
        let preds = ids(&["a", "p", "b", "c", "q", "d", "e"]);
        let (out, _) = ddmin(&preds, contains(&["p", "q"])).unwrap();
        assert_eq!(out, ids(&["p", "q"]));
    }

    #[test]
    fn reproduces_with_no_budget_runs_nothing() {
        // budget 0 short-circuits before spawning a session, and leaves the
        // budget at 0 (no underflow).
        let pins = Pins {
            cache_root: PathBuf::from("/nonexistent"),
        };
        let mut r = Runner {
            python: Path::new("/nonexistent/python"),
            victim: "t::v",
            rootdir: Path::new("/nonexistent"),
            pins: &pins,
            pytest_args: &[],
            budget: 0,
        };
        assert_eq!(r.reproduces(&ids(&["t::a"])).unwrap(), None);
        assert_eq!(r.budget, 0);
    }

    fn rec(phase: Phase) -> super::super::Rec {
        super::super::Rec {
            phase,
            wall: 0.0,
            cpu: None,
        }
    }

    #[test]
    fn victim_failed_reads_the_victims_phase() {
        let mut out = Outcomes::new();
        out.insert("t::bad".into(), rec(Phase::Fail));
        out.insert("t::ok".into(), rec(Phase::Pass));
        assert!(victim_failed(&out, "t::bad").unwrap());
        assert!(!victim_failed(&out, "t::ok").unwrap());
    }

    #[test]
    fn victim_missing_from_the_run_is_an_error_not_a_pass() {
        // A victim that never ran (an unresolvable id, a deselecting option)
        // must not read as "passes", or bisect reports a wrong verdict.
        let err = victim_failed(&Outcomes::new(), "t::gone").unwrap_err();
        assert!(err.to_string().contains("t::gone did not run"), "{err}");
    }

    #[test]
    fn argsfile_lists_absolute_ids_one_per_line() {
        // A real absolute root: `/root` has no drive on Windows.
        let root = std::env::temp_dir();
        let body = argsfile_body(&root, &ids(&["tests/a.py::t[x y]", "b.py::C::m"]));
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("tests/a.py::t[x y]"), "{body}");
        assert!(Path::new(lines[0].split("::").next().unwrap()).is_absolute());
        assert!(lines[1].ends_with("b.py::C::m"), "{body}");
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-bisect-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("tests")).unwrap();
        std::fs::write(d.join("tests").join("test_a.py"), "").unwrap();
        strip_verbatim(d.canonicalize().unwrap())
    }

    #[test]
    fn resolve_nodeid_exact_match() {
        let collected = ids(&["tests/test_a.py::t"]);
        let got = resolve_nodeid(
            "tests/test_a.py::t",
            &collected,
            Path::new("/x"),
            Path::new("/x"),
        );
        assert_eq!(got.as_deref(), Some("tests/test_a.py::t"));
    }

    #[test]
    fn resolve_nodeid_rebases_a_cwd_relative_id() {
        let root = scratch("resolve");
        let collected = ids(&["tests/test_a.py::t[1]"]);
        let got = resolve_nodeid("test_a.py::t[1]", &collected, &root.join("tests"), &root);
        let miss = resolve_nodeid("test_a.py::other", &collected, &root.join("tests"), &root);
        let absent = resolve_nodeid("nope.py::t", &collected, &root.join("tests"), &root);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(got.as_deref(), Some("tests/test_a.py::t[1]"));
        assert_eq!(miss, None);
        assert_eq!(absent, None);
    }

    #[test]
    fn resolve_nodeid_prefers_the_cwd_relative_reading() {
        // From `tests/`, `test_a.py::t` is `tests/test_a.py::t` to pytest,
        // even when the rootdir holds its own `test_a.py::t`.
        let root = scratch("clash");
        std::fs::write(root.join("test_a.py"), "").unwrap();
        let collected = ids(&["test_a.py::t", "tests/test_a.py::t"]);
        let from_tests = resolve_nodeid("test_a.py::t", &collected, &root.join("tests"), &root);
        let from_root = resolve_nodeid("test_a.py::t", &collected, &root, &root);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(from_tests.as_deref(), Some("tests/test_a.py::t"));
        assert_eq!(from_root.as_deref(), Some("test_a.py::t"));
    }

    #[test]
    fn resolve_nodeid_outside_the_rootdir_is_not_found() {
        let root = scratch("outside");
        let collected = ids(&["tests/test_a.py::t"]);
        let got = resolve_nodeid(
            "test_a.py::t",
            &collected,
            &root.join("tests"),
            Path::new("/elsewhere"),
        );
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(got, None);
    }

    #[test]
    fn shell_quote_leaves_safe_ids_and_quotes_the_rest() {
        assert_eq!(shell_quote("tests/a.py::t"), "tests/a.py::t");
        assert_eq!(shell_quote("-n"), "-n");
        assert_eq!(shell_quote("a.py::t[1-x]"), "'a.py::t[1-x]'");
        assert_eq!(shell_quote("a.py::t[x y]"), "'a.py::t[x y]'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn cwd_relative_rebases_ids_onto_the_invocation_dir() {
        let root = Path::new("/r");
        assert_eq!(cwd_relative(root, root, "a.py::t[1]"), "a.py::t[1]");
        assert_eq!(
            cwd_relative(Path::new("/r/tests/unit"), root, "tests/int/b.py::t"),
            "../int/b.py::t"
        );
        assert_eq!(
            cwd_relative(Path::new("/r/tests"), root, "tests/b.py::C::m"),
            "b.py::C::m"
        );
    }

    #[test]
    fn cwd_relative_without_a_shared_root_stays_absolute() {
        // No component in common (another Windows drive; here a relative cwd
        // against an absolute rootdir): `..` can't bridge it, so the id keeps
        // the rootdir's absolute path.
        let root = std::env::temp_dir();
        let got = cwd_relative(Path::new("elsewhere"), &root, "t.py::x");
        assert_eq!(got, format!("{}::x", root.join("t.py").display()));
    }

    #[test]
    fn repro_command_quotes_and_keeps_options_verbatim() {
        let root = Path::new("/r");
        let args = ids(&["-c", "sub/pytest.ini", "-p", "no:cacheprovider"]);
        let at_root = ReproCtx {
            cwd: root,
            rootdir: root,
            inifile: None,
            fresh_cache: false,
            lift_maxfail: false,
            python_flag: None,
            pytest_args: &[],
        };
        assert_eq!(
            repro_command(&at_root, &ids(&["a.py::p"]), "a.py::v[1]"),
            "rstest -n 0 -p no:randomly a.py::p 'a.py::v[1]'"
        );
        // From a subdirectory: no `cd`, ids go cwd-relative, so a relative
        // option (`-c sub/pytest.ini`) still points where the user meant.
        let below = ReproCtx {
            cwd: Path::new("/r/tests/unit"),
            rootdir: root,
            inifile: Some(Path::new("/r/pytest.ini")),
            fresh_cache: false,
            lift_maxfail: false,
            python_flag: Some("/my venv/bin/python"),
            pytest_args: &args,
        };
        assert_eq!(
            repro_command(&below, &ids(&["tests/int/a.py::p"]), "tests/unit/v.py::v"),
            "rstest -n 0 --python '/my venv/bin/python' -p no:randomly -c sub/pytest.ini \
             -p no:cacheprovider ../int/a.py::p v.py::v"
        );
    }

    #[test]
    fn repro_command_pins_the_rootdir_under_a_nested_config() {
        let root = scratch("nested");
        std::fs::create_dir_all(root.join("pkg").join("tests")).unwrap();
        std::fs::write(root.join("pkg").join("pytest.ini"), "[pytest]\n").unwrap();
        let ini = root.join("pytest.ini");
        let ctx = ReproCtx {
            cwd: &root,
            rootdir: &root,
            inifile: Some(&ini),
            fresh_cache: false,
            lift_maxfail: false,
            python_flag: None,
            pytest_args: &[],
        };
        let nested = repro_command(&ctx, &[], "pkg/tests/test_v.py::v");
        let flat = repro_command(&ctx, &[], "tests/test_a.py::v");
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            nested,
            // Paths go through shell_quote: a Windows path (`C:\...`) is quoted.
            format!(
                "rstest -n 0 -p no:randomly --rootdir {} -c {} pkg/tests/test_v.py::v",
                shell_quote(&root.display().to_string()),
                shell_quote(&ini.display().to_string())
            )
        );
        assert_eq!(flat, "rstest -n 0 -p no:randomly tests/test_a.py::v");
    }

    #[test]
    fn repro_command_without_a_config_pins_an_empty_one() {
        // No config loaded, but a nested one would claim the victim: the
        // printed command can't point at bisect's deleted stand-in, so it uses
        // an always-present empty file and pins the conftest cutoff too.
        let root = scratch("noconfig");
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg").join("pytest.ini"), "[pytest]\n").unwrap();
        let ctx = ReproCtx {
            cwd: &root,
            rootdir: &root,
            inifile: None,
            fresh_cache: false,
            lift_maxfail: false,
            python_flag: None,
            pytest_args: &[],
        };
        let cmd = repro_command(&ctx, &[], "pkg/test_v.py::v");
        let _ = std::fs::remove_dir_all(&root);
        let r = shell_quote(&root.display().to_string());
        assert_eq!(
            cmd,
            format!(
                "rstest -n 0 -p no:randomly --rootdir {r} -c {NULL_CONFIG} --confcutdir {r} \
                 pkg/test_v.py::v"
            )
        );
    }

    #[test]
    fn a_nearer_setup_py_counts_as_re_rooting() {
        // With no config file, pytest roots at the nearest setup.py above the
        // tests; one below the suite's rootdir would re-root a plain run.
        let root = scratch("setuppy");
        std::fs::write(root.join("tests").join("setup.py"), "").unwrap();
        let got = under_nested_config(&root, &root.join("tests").join("test_a.py"));
        let _ = std::fs::remove_dir_all(&root);
        assert!(got);
    }

    #[test]
    fn under_nested_config_stops_at_the_rootdir() {
        let root = scratch("nestedstop");
        std::fs::write(root.join("pytest.ini"), "[pytest]\n").unwrap();
        // The rootdir's own config is not "nested".
        let got = under_nested_config(&root, &root.join("tests").join("test_a.py"));
        let _ = std::fs::remove_dir_all(&root);
        assert!(!got);
    }

    #[test]
    fn child_pins_carry_rootdir_config_and_cutoff() {
        let pins = child_pins(Path::new("/r"), Path::new("/t/pytest.ini"), Path::new("/r"));
        assert_eq!(
            pins,
            ids(&[
                "--rootdir",
                "/r",
                "-c",
                "/t/pytest.ini",
                "--confcutdir",
                "/r",
            ])
        );
    }

    #[test]
    fn session_pins_give_every_session_its_own_empty_cache() {
        let pins = Pins {
            cache_root: PathBuf::from("/w/cache"),
        };
        let a = pins.session(&ids(&["-k", "x"]));
        let b = pins.session(&[]);
        assert_eq!(&a[..3], &ids(&["-p", "no:randomly", "-o"])[..]);
        let prefix = format!("cache_dir={}", Path::new("/w/cache").join("s").display());
        assert!(a[3].starts_with(&prefix), "{a:?}");
        assert_eq!(&a[4..], &ids(&["-k", "x"])[..]);
        // A new directory per session, never `--cache-clear` (which needs
        // the cacheprovider loaded).
        assert_ne!(a[3], b[3]);
        assert!(!a.iter().any(|x| x == "--cache-clear"));
    }

    #[test]
    fn only_cache_driven_flags_need_a_fresh_cache() {
        assert!(needs_fresh_cache(&ids(&["--ff"])));
        assert!(needs_fresh_cache(&ids(&["--maxfail", "--lf"])));
        assert!(!needs_fresh_cache(&ids(&["--maxfail"])));
        assert!(!needs_fresh_cache(&[]));
        for f in BLOCKING_ORDER_FLAGS {
            assert!(
                !needs_fresh_cache(&ids(&[f])),
                "{f} is refused, not refreshed"
            );
        }
    }

    #[test]
    fn pinned_inifile_uses_the_loaded_config_or_a_blank_stand_in() {
        let work = workdir().unwrap();
        let real = pinned_inifile(Some("/r/pytest.ini"), &work.0).unwrap();
        assert_eq!(real, Path::new("/r/pytest.ini"));
        assert!(
            !work.0.join("config").exists(),
            "no stand-in when a config loaded"
        );
        let blank = pinned_inifile(None, &work.0).unwrap();
        assert_eq!(std::fs::read_to_string(&blank).unwrap(), "[pytest]\n");
        // Alone in its own directory.
        let siblings = std::fs::read_dir(blank.parent().unwrap()).unwrap().count();
        assert_eq!(siblings, 1);
        let dir = work.0.clone();
        drop(work);
        assert!(!dir.exists(), "the workdir goes on drop");
    }

    #[test]
    fn an_empty_nested_pytest_ini_counts_as_re_rooting() {
        // pytest takes a pytest.ini as the config even with no [pytest]
        // section, so an empty one re-roots a plain run just the same.
        let root = scratch("emptyini");
        std::fs::write(root.join("tests").join("pytest.ini"), "").unwrap();
        let got = under_nested_config(&root, &root.join("tests").join("test_a.py"));
        let _ = std::fs::remove_dir_all(&root);
        assert!(got);
    }

    #[test]
    fn repro_command_lifts_maxfail_after_the_users_args() {
        let root = Path::new("/r");
        let args = ids(&["-x"]);
        let ctx = ReproCtx {
            cwd: root,
            rootdir: root,
            inifile: None,
            fresh_cache: false,
            lift_maxfail: true,
            python_flag: None,
            pytest_args: &args,
        };
        // After the user's `-x`, so it wins (the last --maxfail does).
        assert_eq!(
            repro_command(&ctx, &ids(&["a.py::p"]), "a.py::v"),
            "rstest -n 0 -p no:randomly -x --maxfail=0 a.py::p a.py::v"
        );
    }

    #[test]
    fn repro_command_with_ff_gets_a_fresh_cache() {
        let root = Path::new("/r");
        let ctx = ReproCtx {
            cwd: root,
            rootdir: root,
            inifile: None,
            fresh_cache: true,
            lift_maxfail: false,
            python_flag: None,
            pytest_args: &[],
        };
        let cache = std::env::temp_dir().join("rstest-bisect-repro-cache");
        assert_eq!(
            repro_command(&ctx, &[], "a.py::v"),
            format!(
                "rstest -n 0 -p no:randomly -o {} --cache-clear a.py::v",
                shell_quote(&format!("cache_dir={}", cache.display()))
            )
        );
    }

    #[test]
    fn write_error_json_records_a_run_without_a_verdict() {
        let p = tmp_json("err");
        write_error_json(Some(&p), "t::v", "boom").unwrap();
        let doc = read(&p);
        assert_eq!(doc["nodeid"], "t::v");
        assert_eq!(doc["order_dependent"], false);
        assert_eq!(doc["culprits"], serde_json::json!([]));
        assert!(doc["reproduce_command"].is_null());
        assert_eq!(doc["error"], "boom");
        write_error_json(None, "t::v", "boom").unwrap();
    }

    fn tmp_json(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rstest-bisect-{tag}-{}.json", std::process::id()))
    }

    fn read(p: &Path) -> serde_json::Value {
        let doc = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let _ = std::fs::remove_file(p);
        doc
    }

    #[test]
    fn write_json_without_a_path_is_a_noop() {
        write_json(
            None,
            "t::v",
            Path::new("/r"),
            Path::new("/r"),
            &ids(&["t::a"]),
            Some("x"),
        )
        .unwrap();
    }

    #[test]
    fn write_json_order_dependent_doc() {
        let p = tmp_json("od");
        let cmd = "rstest -n 0 t::a t::b t::v";
        write_json(
            Some(&p),
            "t::v",
            Path::new("/r"),
            Path::new("/r/sub"),
            &ids(&["t::a", "t::b"]),
            Some(cmd),
        )
        .unwrap();
        let doc = read(&p);
        assert_eq!(doc["meta"]["kind"], "bisect");
        assert_eq!(doc["meta"]["runner"], "rstest");
        assert_eq!(doc["meta"]["schema"], 1);
        assert_eq!(doc["nodeid"], "t::v");
        assert_eq!(doc["rootdir"], "/r");
        assert_eq!(doc["cwd"], "/r/sub");
        assert_eq!(doc["order_dependent"], true);
        assert_eq!(doc["culprits"], serde_json::json!(["t::a", "t::b"]));
        assert_eq!(doc["reproduce_command"], cmd);
    }

    #[test]
    fn write_json_not_order_dependent_has_null_command() {
        let p = tmp_json("nod");
        write_json(
            Some(&p),
            "t::v",
            Path::new("/r"),
            Path::new("/r"),
            &[],
            None,
        )
        .unwrap();
        let doc = read(&p);
        assert_eq!(doc["order_dependent"], false);
        assert_eq!(doc["culprits"], serde_json::json!([]));
        assert!(doc["reproduce_command"].is_null());
    }

    #[test]
    fn write_json_unwritable_path_errors() {
        let p = std::env::temp_dir()
            .join(format!("rstest-bisect-missing-{}", std::process::id()))
            .join("nested")
            .join("out.json");
        assert!(write_json(
            Some(&p),
            "t::v",
            Path::new("/r"),
            Path::new("/r"),
            &[],
            None
        )
        .is_err());
    }

    #[test]
    fn report_single_culprit() {
        let (mut sink, cap) = Sink::captured();
        print_report(
            &mut sink,
            "t::v",
            &ids(&["t::a"]),
            "rstest -n 0 t::a t::v",
            false,
        );
        let out = cap.out();
        assert!(out.contains("victim:   t::v"), "{out}");
        assert!(out.contains("culprit: 1 predecessor test(s)"), "{out}");
        assert!(out.contains("    t::a\n"), "{out}");
        assert!(out.contains("    rstest -n 0 t::a t::v\n"), "{out}");
        assert!(!out.contains("run budget reached"), "{out}");
    }

    #[test]
    fn report_plural_culprits_and_budget_cap() {
        let (mut sink, cap) = Sink::captured();
        print_report(&mut sink, "t::v", &ids(&["t::a", "t::b"]), "x", true);
        let out = cap.out();
        assert!(out.contains("culprits: 2 predecessor test(s)"), "{out}");
        assert!(out.contains("run budget reached"), "{out}");
    }
}
