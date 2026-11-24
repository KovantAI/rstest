//! The argv pre-scan: partition rstest-owned flags from pytest session args
//! (clap can't mirror pytest's plugin-extensible flag surface), and the
//! parsers that read pytest session args (`-x`/`--maxfail`, `--durations`,
//! passthrough IO, paths).

use std::path::Path;

/// pytest short options that take a value. In a cluster (`-rA`, `-ksmoke`,
/// `-pno:cov`) everything after one of these is its value, not more switches.
const SHORT_VALUE_FLAGS: &[char] = &['k', 'm', 'p', 'c', 'o', 'W', 'r', 'n'];

/// The short switches in one pytest argv token, the way argparse expands a
/// cluster: `-sv` -> `['s', 'v']`, `-xrA` -> `['x', 'r']` (`A` is `-r`'s value).
/// Empty for long options, a bare `-`, and positionals.
pub(crate) fn short_switches(arg: &str) -> Vec<char> {
    let Some(rest) = arg.strip_prefix('-') else {
        return Vec::new();
    };
    if rest.starts_with('-') {
        return Vec::new();
    }
    let mut out = Vec::new();
    for c in rest.chars() {
        out.push(c);
        if SHORT_VALUE_FLAGS.contains(&c) {
            break;
        }
    }
    out
}

/// Does this argv token set short switch `c`, alone or clustered (`-x`, `-xv`)?
pub(crate) fn has_short(arg: &str, c: char) -> bool {
    short_switches(arg).contains(&c)
}

/// Session (pytest and common plugin) long options whose value can be the
/// next token (`--ignore tests/slow`). Mined from the vendored core's
/// `addoption` calls, plus the logging options and the plugins rstest's
/// docs name. A plugin option missing here still works in `--opt=value` form.
const SESSION_VALUE_FLAGS: &[&str] = &[
    // pytest core
    "--assert",
    "--basetemp",
    "--capture",
    "--code-highlight",
    "--color",
    "--confcutdir",
    "--config-file",
    "--deselect",
    "--doctest-glob",
    "--doctest-report",
    "--durations",
    "--durations-min",
    "--ignore",
    "--ignore-glob",
    "--import-mode",
    "--junit-prefix",
    "--junit-xml",
    "--junitprefix",
    "--junitxml",
    "--last-failed-no-failures",
    "--lfnf",
    "--log-auto-indent",
    "--log-cli-date-format",
    "--log-cli-format",
    "--log-cli-level",
    "--log-date-format",
    "--log-disable",
    "--log-file",
    "--log-file-date-format",
    "--log-file-format",
    "--log-file-level",
    "--log-file-mode",
    "--log-format",
    "--log-level",
    "--max-warnings",
    "--maxfail",
    "--override-ini",
    "--pastebin",
    "--pdbcls",
    "--pythonwarnings",
    "--report-chars",
    "--rootdir",
    "--show-capture",
    "--tb",
    "--verbosity",
    // plugins
    "--cov-config",
    "--cov-context",
    "--cov-fail-under",
    "--cov-report",
    "--dc",
    "--ds",
    "--hypothesis-profile",
    "--hypothesis-seed",
    "--max-worker-restart",
    "--maxprocesses",
    "--randomly-seed",
    "--report-log",
    "--rerun-except",
    "--reruns-delay",
    "--rsyncdir",
    "--rsyncignore",
    "--timeout-method",
    "--tx",
];

/// Session options with an *optional* value (argparse `nargs="?"`): the next
/// token is the value unless it looks like another option (`--cov src`).
const SESSION_OPT_VALUE_FLAGS: &[&str] = &["--cache-show", "--cov"];

/// Which session args are positionals (paths, nodeids, `@argsfile`) rather
/// than options or an option's separate value, the way pytest's argparse
/// reads them: `-k api` / `--ignore tests/slow` / `-xk api` name no path,
/// even when `api` or `tests/slow` exists on disk.
pub(crate) fn positional_mask(args: &[String]) -> Vec<bool> {
    let mut mask = vec![false; args.len()];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let takes_next = if a == "--" {
            mask[i + 1..].iter_mut().for_each(|m| *m = true);
            break;
        } else if a.starts_with("--") {
            !a.contains('=')
                && (SESSION_VALUE_FLAGS.contains(&a)
                    || (SESSION_OPT_VALUE_FLAGS.contains(&a)
                        && args.get(i + 1).is_some_and(|n| !n.starts_with('-'))))
        } else if a.len() > 1 && a.starts_with('-') {
            // `-k VALUE` / `-xk VALUE`: the cluster ends on a value flag
            // with nothing attached, so the value is the next token.
            let sw = short_switches(a);
            sw.len() == a.len() - 1 && sw.last().is_some_and(|c| SHORT_VALUE_FLAGS.contains(c))
        } else {
            mask[i] = true;
            false
        };
        i += if takes_next { 2 } else { 1 };
    }
    mask
}

/// Positional session args naming an existing path (a pytest `@argsfile`
/// counts by its file): the explicit selection.
pub(crate) fn path_args(args: &[String]) -> Vec<&String> {
    args.iter()
        .zip(positional_mask(args))
        .filter(|(a, pos)| *pos && std::path::Path::new(a.strip_prefix('@').unwrap_or(a)).exists())
        .map(|(a, _)| a)
        .collect()
}

/// The session args minus the explicit path selection: the flags, with
/// their values, to keep when rstest substitutes its own selection
/// (`--changed`, `--watch` reruns).
pub(crate) fn without_path_args(args: &[String]) -> Vec<String> {
    args.iter()
        .zip(positional_mask(args))
        .filter(|(a, pos)| {
            !*pos || !std::path::Path::new(a.strip_prefix('@').unwrap_or(a)).exists()
        })
        .map(|(a, _)| a.clone())
        .collect()
}

/// -x / --maxfail=N from the session args (also forwarded: each worker
/// session stops itself; the orchestrator does the global coordination).
/// Argv only: a limit from ini `addopts` arrives later, from the workers.
pub(crate) fn parse_maxfail(args: &[String]) -> Option<u64> {
    let mut limit = None;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--exitfirst" => limit = Some(1),
            "--maxfail" => {
                if let Some(v) = it.peek().and_then(|v| v.parse().ok()) {
                    limit = Some(v);
                }
            }
            _ if has_short(a, 'x') => limit = Some(1),
            _ => {
                if let Some(v) = a.strip_prefix("--maxfail=").and_then(|v| v.parse().ok()) {
                    limit = Some(v);
                }
            }
        }
    }
    limit.filter(|&v| v > 0)
}

/// --durations=N / --durations-min=X from the session args. Workers also
/// receive them (harmless; their terminals are nulled); the orchestrator
/// owns the rendered block. Returns (N, min_secs); N == 0 means all.
pub(crate) fn parse_durations(args: &[String]) -> Option<(usize, f64)> {
    let mut n: Option<usize> = None;
    let mut min = 0.005;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--durations" => {
                if let Some(v) = it.peek().and_then(|v| v.parse().ok()) {
                    n = Some(v);
                }
            }
            "--durations-min" => {
                if let Some(v) = it.peek().and_then(|v| v.parse().ok()) {
                    min = v;
                }
            }
            _ => {
                if let Some(v) = a.strip_prefix("--durations=").and_then(|v| v.parse().ok()) {
                    n = Some(v);
                }
                if let Some(v) = a
                    .strip_prefix("--durations-min=")
                    .and_then(|v| v.parse().ok())
                {
                    min = v;
                }
            }
        }
    }
    n.map(|n| (n, min))
}

/// Session flags that need pytest's own terminal (or stdin): run a single
/// worker with inherited stdio and let the vendored core render. Stepwise is
/// here too because it is inherently sequential and wants `-n 0` like xdist.
pub(crate) fn needs_passthrough_io(session_args: &[String]) -> bool {
    passthrough_trigger(session_args).is_some()
}

/// The first session flag that forces the passthrough path (for messages),
/// matching [`needs_passthrough_io`]. `-s` counts clustered too (`-sv`), and
/// `--capture` in both its `=VALUE` and two-token (`--capture no`) forms.
pub(crate) fn passthrough_trigger(session_args: &[String]) -> Option<&str> {
    session_args
        .iter()
        .find(|a| {
            matches!(
                a.as_str(),
                "--collect-only"
                    | "--co"
                    | "--capture"
                    | "--pdb"
                    | "--trace"
                    | "--sw"
                    | "--stepwise"
                    | "--sw-skip"
                    | "--stepwise-skip"
                    | "--sw-reset"
                    | "--stepwise-reset"
            ) || a.starts_with("--capture=")
                || has_short(a, 's')
        })
        .map(String::as_str)
}

pub(crate) fn is_collect_only(session_args: &[String]) -> bool {
    session_args
        .iter()
        .any(|a| a == "--collect-only" || a == "--co")
}

/// Split argv into rstest-owned args (fed to clap) and session args
/// (paths + pytest flags, forwarded verbatim).
pub(crate) fn split_argv() -> (Vec<String>, Vec<String>) {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Fail fast, before any worker spawns, like clap's own usage errors.
    if let Some(msg) = misplaced_subcommand(&argv).or_else(|| detached_auto_quarantine(&argv)) {
        eprintln!("{msg}");
        std::process::exit(2);
    }
    if let Some(msg) = shadowed_path(Path::new("."), &argv) {
        eprintln!("{msg}");
    }
    split_args(argv)
}

/// Single source of truth for the rstest-owned flag surface. Every long/short
/// flag on the `Cli` clap struct must appear in exactly one of these tables;
/// the `every_clap_flag_is_covered_by_the_split_tables` test enforces that, so
/// a new flag can't be silently forwarded to the pytest session.
///
/// `BOOL_FLAGS`: switches that consume no value.
pub(super) const BOOL_FLAGS: &[&str] = &[
    "--doctor",
    "--xdist-trial",
    "--watch",
    "--fail-on-leak",
    "--warn-on-dead-master-path",
    "--instrument-workers",
    "--reruns-only-known-flaky",
    "--since-green",
    "--incremental",
    "--cache-pull",
    "--cache-push",
    "--require-baseline",
    "--changed-strict",
    "-h",
    "--help",
    "-V",
    "--version",
];

/// Run-less subcommand names (see [`super::Command`]). Recognized by [`split_args`] as
/// the leading argv token or after options only (`rstest -q try`; see
/// [`subcommand_index`]); everything else is split by the flag tables as
/// usual, so `rstest try -k foo tests/` still forwards `-k foo tests/` to the
/// session. Kept in kebab-case to match clap's derived subcommand names. A
/// pytest path literally named after a subcommand is shadowed when leading,
/// with a warning ([`shadowed_path`]; `rstest ./try` / `rstest -- try`
/// disambiguates).
pub(super) const SUBCOMMANDS: &[&str] = &[
    "verify-vendor",
    "try",
    "migrate-check",
    "xdist-removal-check",
    "audit",
    "bisect",
    "cache-compact",
    "shard-verify",
    "replay",
    "explain",
    "flaky",
    "since-green",
    "report",
    "install-skills",
];

/// The `--output` styles rstest renders. `--output` with any other value is a
/// plugin's flag (pytest-playwright's `--output DIR`) and is forwarded to the
/// pytest session instead (see [`split_args`]).
pub(crate) const OUTPUT_STYLES: &[&str] = &[
    "dots",
    "verbose",
    "bar",
    "github",
    "gitlab",
    "buildkite",
    "teamcity",
    "azure",
    "tap",
    "json",
];

/// `--output VALUE` (or `--output=VALUE`) whose VALUE is not one of
/// [`OUTPUT_STYLES`]: the plugin's flag, not rstest's.
fn foreign_output(arg: &str, next: Option<&String>) -> bool {
    let value = match arg.strip_prefix("--output=") {
        Some(v) => Some(v),
        None if arg == "--output" => next.map(String::as_str),
        None => None,
    };
    value.is_some_and(|v| !OUTPUT_STYLES.contains(&v))
}

/// The value of an `--output` that [`split_args`] forwarded to the session.
pub(crate) fn forwarded_output(session_args: &[String]) -> Option<&str> {
    let mut it = session_args.iter();
    while let Some(a) = it.next() {
        if a == "--output" {
            return it.next().map(String::as_str);
        }
        if let Some(v) = a.strip_prefix("--output=") {
            return Some(v);
        }
    }
    None
}

/// Optional-value flags (`num_args = 0..=1`): a bare `--changed` consumes
/// nothing, an attached `--changed=REV` carries its value inline. Never eats
/// the following argv item (that item is a path / pytest flag).
pub(super) const OPT_FLAGS: &[&str] = &[
    "--changed",
    "--shuffle",
    "--debug",
    "--auto-quarantine",
    "--fork-pool",
];

/// Flags that take a value: either the following argv item (`--dist load`) or
/// `=`-joined (`--dist=load`). `-n` also accepts the attached short forms
/// `-n4` / `-n=4`, handled in `owned_without_value`.
pub(super) const VALUE_FLAGS: &[&str] = &[
    "-n",
    "--numprocesses",
    "--python",
    "--report-json",
    "--output",
    "--cache-remote",
    "--migrate-check-json",
    "--migrate-allow",
    "--xdist-removal-json",
    "--audit-json",
    "--audit-repeat",
    "--bisect-json",
    "--durations-regress",
    "--only-rerun",
    "--cov-diff-fail-under",
    "--cov-diff-json",
    "--worker-timeout",
    "--timeout",
    "--reruns",
    "--doctor-json",
    "--quarantine",
    "--auto-quarantine-max",
    "--doctor-md",
    "--doctor-fail-on",
    "--junitxml",
    "--junit-xml",
    "--html",
    "--dist",
    "--shard",
    "--budget",
    "--collect",
    "--order",
    "--keep-last",
    "--max-age",
    "--cache-compact-threshold",
    "--stream-json",
];

/// True when `arg` is the `=`-joined form of flag `f` (e.g. `--dist=load` for
/// `--dist`, `-n=4` for `-n`).
fn is_eq_form(arg: &str, f: &str) -> bool {
    arg.strip_prefix(f)
        .is_some_and(|rest| rest.starts_with('='))
}

/// rstest-owned tokens clap consumes as a single argv item, no following value:
/// boolean switches, optional-value flags (bare or `=VALUE`), `=`-joined value
/// flags, and the attached short numprocesses forms `-n4` / `-n=4` (bare `-n`
/// is a value flag handled by the caller). Exactness matters: `--durations`,
/// `--durations-min`, `--collect-only`, `--co` are NOT prefixes of any table
/// entry, so they correctly stay session args.
fn owned_without_value(arg: &str) -> bool {
    BOOL_FLAGS.contains(&arg)
        || OPT_FLAGS.iter().any(|f| arg == *f || is_eq_form(arg, f))
        || VALUE_FLAGS.iter().any(|f| is_eq_form(arg, f))
        || (arg.starts_with("-n") && arg != "-n")
}

/// pytest switches that never take a value, so a token after them is never
/// their argument. Only these (plus rstest's own flags and self-contained
/// `--flag=value` forms) may precede a subcommand: see [`subcommand_index`].
const PYTEST_SWITCHES: &[&str] = &[
    "--quiet",
    "--verbose",
    "--exitfirst",
    "--showlocals",
    "--lf",
    "--last-failed",
    "--ff",
    "--failed-first",
    "--nf",
    "--new-first",
    "--sw",
    "--stepwise",
    "--sw-skip",
    "--stepwise-skip",
    "--cache-clear",
    "--strict-markers",
    "--strict-config",
    "--strict",
    "--disable-warnings",
    "--disable-pytest-warnings",
    "--no-header",
    "--no-summary",
    "--runxfail",
    "--setup-show",
    "--full-trace",
];

/// A value-less pytest switch: one of [`PYTEST_SWITCHES`] or a cluster of the
/// value-less short flags `-q -v -x -s -l` (`-q`, `-vv`, `-xs`).
pub(super) fn is_pytest_switch(arg: &str) -> bool {
    PYTEST_SWITCHES.contains(&arg)
        || arg.strip_prefix('-').is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .chars()
                    .all(|c| matches!(c, 'q' | 'v' | 'x' | 's' | 'l'))
        })
}

/// Where the run-less subcommand sits in argv, if there is one.
///
/// The leading token always counts (`rstest try ...`; `./try` or `-- try`
/// reach a path literally named after a subcommand). A later token counts
/// only when everything before it is an option that cannot swallow it as a
/// value: an rstest-owned flag (with its value), a value-less pytest switch
/// (`-q`, `-x`, ...), or a self-contained `--flag=value`. So `rstest -q try`
/// and `rstest --python X migrate-check` select the subcommand, while
/// `rstest -k try` (the `-k` expression) and `rstest tests/ audit` don't. A
/// non-leading match that exists as a path in the cwd stays a pytest path.
pub(super) fn subcommand_index(argv: &[String]) -> Option<usize> {
    let mut i = 0;
    while let Some(arg) = argv.get(i) {
        if SUBCOMMANDS.contains(&arg.as_str()) {
            return (i == 0 || !std::path::Path::new(arg).exists()).then_some(i);
        }
        if foreign_output(arg, argv.get(i + 1)) {
            return None;
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            i += 2;
        } else if owned_without_value(arg)
            || is_pytest_switch(arg)
            || (arg.starts_with("--") && arg.contains('='))
        {
            i += 1;
        } else {
            return None;
        }
    }
    None
}

/// A subcommand name that [`subcommand_index`] did not select but that sits
/// where a test path would (not right after a flag that could take it as a
/// value, not after `--`, not an existing path): almost certainly a misplaced
/// subcommand, as in `rstest -k foo try`. Returns the usage error to print
/// instead of letting pytest report `file or directory not found: try` after
/// spawning workers.
pub(crate) fn misplaced_subcommand(argv: &[String]) -> Option<String> {
    if subcommand_index(argv).is_some() {
        return None;
    }
    let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    (1..end).find_map(|i| {
        let tok = argv[i].as_str();
        let prev = argv[i - 1].as_str();
        let may_be_value = prev.starts_with('-') && !prev.contains('=');
        (SUBCOMMANDS.contains(&tok) && !may_be_value && !std::path::Path::new(tok).exists()).then(
            || {
                format!(
                    "rstest: `{tok}` is a subcommand, but subcommands must come first: \
                     `rstest {tok} ...` (options after it). To run a test path named \
                     `{tok}`, write `./{tok}`."
                )
            },
        )
    })
}

/// A leading subcommand name that is also an existing path (`rstest flaky`
/// with a `flaky/` test dir): the subcommand wins, so the tests never run and
/// a CI job goes green on nothing. Returns the warning naming the escape.
/// `cwd` is where the path would resolve (`.` outside tests).
pub(crate) fn shadowed_path(cwd: &Path, argv: &[String]) -> Option<String> {
    let tok = argv.first()?;
    (subcommand_index(argv) == Some(0) && cwd.join(tok).exists()).then(|| {
        format!(
            "rstest: warning: `{tok}` runs the `{tok}` subcommand, not the path `{tok}` here. \
             To run that path's tests, write `./{tok}`."
        )
    })
}

/// `--auto-quarantine N` with a space: an optional-value flag never takes the
/// next token, so `N` would run as a test path (pytest: "file or directory
/// not found: 0") while the flag stays at its default. A bare number that is
/// not a path is the usage error; `./0` still runs a path named `0`.
pub(super) fn detached_auto_quarantine(argv: &[String]) -> Option<String> {
    let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    argv[..end].windows(2).find_map(|w| {
        let n = w[1].as_str();
        (w[0] == "--auto-quarantine"
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
            && !std::path::Path::new(n).exists())
        .then(|| {
            format!("rstest: write `--auto-quarantine={n}`: the value must be attached with `=`")
        })
    })
}

/// Route argv tokens to `own` (rstest flags, fed to clap) or `session`
/// (pytest args, forwarded verbatim).
fn route(argv: impl IntoIterator<Item = String>, own: &mut Vec<String>, session: &mut Vec<String>) {
    let mut argv = argv.into_iter().peekable();
    while let Some(arg) = argv.next() {
        if arg == "--" {
            session.extend(argv.by_ref());
        } else if foreign_output(&arg, argv.peek()) {
            // Not an rstest style: a plugin's `--output` (pytest-playwright),
            // forwarded whole so the plugin sees it at every worker count.
            let joined = arg.contains('=');
            session.push(arg);
            if !joined {
                session.extend(argv.next());
            }
        } else if owned_without_value(&arg) {
            own.push(arg);
        } else if VALUE_FLAGS.contains(&arg.as_str()) {
            own.push(arg);
            if let Some(v) = argv.next() {
                own.push(v);
            }
        } else {
            session.push(arg);
        }
    }
}

pub(crate) fn split_args(argv: impl IntoIterator<Item = String>) -> (Vec<String>, Vec<String>) {
    let mut own = vec!["rstest".to_string()];
    let mut session = Vec::new();
    let mut argv: Vec<String> = argv.into_iter().collect();
    // A run-less subcommand: the leading token, or the first positional after
    // options that can't take it as a value (see `subcommand_index`). Options
    // before it route as usual (clap accepts rstest's own before the
    // subcommand; pytest switches like `-q` go to the session), and the
    // subcommand's paired flags (all `global` on `Cli`) may follow it.
    let Some(at) = subcommand_index(&argv) else {
        route(argv, &mut own, &mut session);
        return (own, session);
    };
    let rest = argv.split_off(at + 1);
    let sub = argv.pop().expect("subcommand token at `at`");
    route(argv, &mut own, &mut session);
    // `shard-verify` (report-json paths), `replay` (a run-id / `--journal`),
    // `bisect` (a single nodeid), `explain` (a nodeid) and `install-skills`
    // (its own flags) build no pytest session from argv: every token after
    // them is a clap positional or a subcommand-local flag, so route them all
    // to `own` rather than forwarding non-flag tokens to the (nonexistent
    // argv-built) session. The nodeids contain `::`, which the flag tables
    // would otherwise route to the session and hide from clap. `replay` gets
    // its real session args from the journal, not argv.
    let consumes_all = matches!(
        sub.as_str(),
        "shard-verify"
            | "replay"
            | "bisect"
            | "explain"
            | "flaky"
            | "since-green"
            | "report"
            | "install-skills"
    );
    own.push(sub);
    if consumes_all {
        own.extend(rest);
    } else {
        route(rest, &mut own, &mut session);
    }
    (own, session)
}
