//! CLI surface: the `Cli` clap struct, the pre-scan that partitions
//! rstest-owned flags from pytest session args (clap can't mirror pytest's
//! plugin-extensible flag surface), and the session-arg parsers.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Run-less subcommands: modes that do their own thing and exit without the
/// normal run pipeline. Selected by a leading token (`rstest verify-vendor`),
/// recognized by [`split_args`] before the flag pre-scan; `None` is the default
/// (run the suite). The paired option flags (`--migrate-check-json`, etc.) stay
/// `global` on [`Cli`] so they parse after the subcommand token.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// Verify the vendored pytest tree is byte-identical to what shipped
    /// (rehash `_vendor/` against the packaged `vendor.lock`). Prints a report
    /// and exits 0 if intact, non-zero on any drift.
    VerifyVendor,

    /// Zero-config proof: run the suite under plain pytest and under rstest
    /// (-n auto), then report whether outcomes are identical and how much
    /// faster rstest is. The 30-second "should I switch?" answer.
    Try,

    /// Parallel-readiness preflight: collect twice and report tests with
    /// unstable ids, then run -n auto and classify any parallel-only failure
    /// (polluter bisected). Exits non-zero on any such finding. Combine with
    /// `--migrate-check-json` / `--migrate-allow`.
    MigrateCheck,

    /// Maintenance: fold remote segments into a fresh base and prune them, then
    /// exit without running tests. Needs `--cache-remote`. With no retention
    /// flags it folds all; `--keep-last` / `--max-age` leave a recent window so
    /// the segment set stays bounded without discarding fresh history.
    CacheCompact {
        /// Keep the newest N segments loose; fold only older ones into the
        /// base. Unset folds all. Env: `RSTEST_CACHE_KEEP_LAST`.
        #[arg(long, value_name = "N")]
        keep_last: Option<usize>,
        /// Keep segments younger than this loose; fold older ones. Accepts a
        /// bare number (seconds) or a `s`/`m`/`h`/`d`/`w` suffix (e.g. `30d`).
        /// Unset folds all. Env: `RSTEST_CACHE_MAX_AGE`.
        #[arg(long, value_name = "DURATION")]
        max_age: Option<String>,
    },
}

/// rstest: a fast, pytest-compatible test runner. Unrecognized flags forward
/// to the test session verbatim: clap can't mirror pytest's large,
/// plugin-extensible flag surface, so we pre-scan argv ourselves.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "rstest",
    version,
    disable_help_flag = false,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Run-less mode selected by a leading subcommand token; `None` runs the
    /// suite. See [`Command`].
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    /// Number of worker processes (logical cores); rstest is parallel by
    /// design. Use 0 or 1 for single-worker mode (byte-exact pytest semantics).
    /// Config: `[tool.rstest] numprocesses`. [default: auto]
    #[arg(short = 'n', long = "numprocesses")]
    pub(crate) numprocesses: Option<String>,

    /// Python interpreter to run workers with: a path, or a version request
    /// (`3.12`, `>=3.12,<3.13`, `pypy@3.10`, `3.13t`). Defaults to the active
    /// venv / a discovered `.venv` / `.python-version` / PATH.
    #[arg(long, global = true)]
    pub(crate) python: Option<String>,

    /// Write a per-test outcome snapshot (compat-harness recorder shape).
    #[arg(long)]
    pub(crate) report_json: Option<PathBuf>,

    /// Diagnose the suite after running: wait-bound tests, parallel
    /// floor, fixture hotspots, slowest files.
    #[arg(long)]
    pub(crate) doctor: bool,

    /// Write the doctor analysis as JSON (stable, versioned schema) for
    /// CI trending. Implies doctor instrumentation; combine with
    /// --doctor for the human report too.
    #[arg(long)]
    pub(crate) doctor_json: Option<PathBuf>,

    /// Write the doctor analysis as GitHub-flavored markdown (job-summary
    /// ready; implies doctor instrumentation). In CI a doctor run auto-publishes
    /// to the job summary; the flag is for a custom path or GitLab/TeamCity.
    #[arg(long)]
    pub(crate) doctor_md: Option<PathBuf>,

    /// Fail the run when a doctor metric breaches a threshold (repeatable),
    /// turning the advisory signal into a CI gate. Grammar `metric OP value`,
    /// e.g. `--doctor-fail-on 'parallel_efficiency<30'`. Implies instrumentation.
    #[arg(long = "doctor-fail-on", value_name = "COND")]
    pub(crate) doctor_fail_on: Vec<String>,

    /// Fail the run if any test leaks a thread or file descriptor (net still
    /// open after its teardown). Turns the leak signal into a CI gate; enables
    /// the leak-check instrumentation on its own (no --doctor needed).
    #[arg(long = "fail-on-leak")]
    pub(crate) fail_on_leak: bool,

    /// Write the migrate-check findings as JSON (stable, versioned schema) for
    /// CI gating. Used with the `migrate-check` subcommand.
    #[arg(long, global = true)]
    pub(crate) migrate_check_json: Option<PathBuf>,

    /// Substring of a nodeid/site to accept as a known migrate-check finding
    /// (repeatable): it is still reported (marked "allowed") but does not fail
    /// the exit code, so CI can gate on NEW issues while tolerating known ones.
    #[arg(long = "migrate-allow", global = true)]
    pub(crate) migrate_allow: Vec<String>,

    /// Distribution mode: "load" (dynamic, duration-aware), "loadfile",
    /// "loadscope", "loadgroup" (xdist_group marker affinity), or "each"
    /// (every test on every worker). [default: load]
    #[arg(long)]
    pub(crate) dist: Option<String>,

    /// Write merged results as junit XML (intercepted: per-worker sessions
    /// would clobber a shared file).
    #[arg(long)]
    pub(crate) junitxml: Option<PathBuf>,

    /// Write a self-contained HTML report of the merged run (rendered
    /// orchestrator-side, so it works under the parallel pool where pytest-html
    /// produces nothing at -n ≥ 2).
    #[arg(long)]
    pub(crate) html: Option<PathBuf>,

    /// Watch the project and rerun on change: only-test-file changes rerun
    /// just those files; any other .py change reruns the tests that import
    /// the changed module (import-graph selection).
    #[arg(long)]
    pub(crate) watch: bool,

    /// Rerun failed tests up to N times; tests that then pass are
    /// reported flaky (run stays green). Crash-aware: a test that killed
    /// its worker gets retried on the replacement, within this budget.
    #[arg(long)]
    pub(crate) reruns: Option<u32>,

    /// Quarantine list: a file of nodeids or glob patterns (one per line,
    /// # comments). Matching failures are demoted to a non-fatal outcome
    /// (own section, flagged, never the exit code); others still fail.
    #[arg(long, value_name = "FILE")]
    pub(crate) quarantine: Option<PathBuf>,

    /// With reruns active, retry only failures whose error text matches
    /// this regex (repeatable). pytest-rerunfailures' --only-rerun.
    #[arg(long = "only-rerun", value_name = "REGEX")]
    pub(crate) only_rerun: Vec<String>,

    /// With reruns active, retry only tests that have a prior *flaky* history
    /// in `.rstest_cache/flakes.json` (passed-after-rerun on some earlier
    /// run). A first-time failure with no flaky history is reported failed
    /// without spending the budget — so a deterministic mass-failure (one
    /// cause failing many tests identically) no longer burns reruns for zero
    /// recovery. `@pytest.mark.flaky` tests are always retried (the marker is
    /// an explicit declaration). Composes with `--only-rerun` (both gates
    /// must pass).
    #[arg(long = "reruns-only-known-flaky")]
    pub(crate) reruns_only_known_flaky: bool,

    /// Kill a worker stuck on ONE test longer than this many seconds
    /// (hang backstop; the test is reported failed, the worker replaced).
    /// Off by default; catches what in-process timeouts can't (blocked C exts).
    #[arg(long, value_name = "SECS")]
    pub(crate) worker_timeout: Option<u64>,

    /// Per-test timeout: fail any test whose call phase runs longer than SECS.
    /// Interrupted in-process, so the failure's traceback points at the stuck
    /// line (pytest-timeout-style; no plugin needed). `@pytest.mark.timeout(N)`
    /// overrides per test. Fractional seconds allowed. A blocked C extension
    /// that never returns to Python is caught by the --worker-timeout backstop
    /// instead (auto-armed from this value when --worker-timeout is unset).
    #[arg(long, value_name = "SECS")]
    pub(crate) timeout: Option<f64>,

    /// Run only tests affected by changed files (import-graph selection).
    /// Without a value: working tree + untracked vs HEAD. With a value:
    /// vs that git rev (e.g. --changed=origin/main in CI).
    #[arg(long, num_args = 0..=1, default_missing_value = "HEAD", value_name = "REV")]
    pub(crate) changed: Option<String>,

    /// Strict --changed for gating CI: an unconnectable changed source file
    /// forces a FULL run (no silent skip), and "nothing affected" exits 5
    /// instead of 0. Implies --changed (vs HEAD) when not given.
    #[arg(long)]
    pub(crate) changed_strict: bool,

    /// Diff-coverage gate: fail the run when the percentage of ADDED/CHANGED
    /// lines (vs the --changed base, else HEAD) that are covered by tests falls
    /// below PCT. Requires --cov. Reports the uncovered added lines per file.
    #[arg(long = "cov-diff-fail-under", value_name = "PCT")]
    pub(crate) cov_diff_fail_under: Option<f64>,

    /// Write the diff-coverage report as JSON to PATH:
    /// {"pct","covered","uncovered","files":{"<path>":[<uncovered line>...]}}.
    /// Scores coverage of ADDED/CHANGED lines vs the --changed base (else HEAD).
    /// Requires --cov. Independent of --cov-diff-fail-under (no gate implied).
    #[arg(long = "cov-diff-json", value_name = "PATH")]
    pub(crate) cov_diff_json: Option<PathBuf>,

    /// Incremental testing: run only what changed since the last GREEN run,
    /// re-using --changed's coverage-aware selection with an auto-managed
    /// baseline (the commit of the last all-passing run, stored in the cache).
    /// The baseline advances only when a run is fully green, so a failing test
    /// keeps being selected until it passes. First run (no baseline) runs
    /// everything. Ignored when --changed is given explicitly.
    #[arg(long = "since-green")]
    pub(crate) since_green: bool,

    /// Dispatch-level incremental testing: collect the whole suite, then SKIP
    /// running any test that was green last run and whose covered source is
    /// byte-identical now (content-addressed via the coverage index — no git).
    /// Skipped tests are carried forward as cached passes. Needs a warm coverage
    /// index (a prior `--cov-context=test` run); full collection + `--dist load`
    /// only. A config-file change disables skipping for that run.
    #[arg(long)]
    pub(crate) incremental: bool,

    /// Collection strategy: "full" (every worker collects the whole suite,
    /// verified by hash) or "lazy" (each file collected by one worker on
    /// demand). Config `[tool.rstest] collect`. [default: full]
    #[arg(long, value_name = "MODE")]
    pub(crate) collect: Option<String>,

    /// Gate CI on per-test duration regressions: compare each test's wall time
    /// against the duration cache and exit non-zero when any test grew past
    /// RATIO x baseline (e.g. 2.0). Jitter-floored below 50ms / 0.5s growth.
    #[arg(long, value_name = "RATIO")]
    pub(crate) durations_regress: Option<f64>,

    /// Run tests in a seeded random order (pytest-randomly-style) to flush
    /// order dependencies. No value: per-run seed, printed; --shuffle=SEED
    /// reproduces. Parallel pool with full collection only.
    #[arg(long, num_args = 0..=1, default_missing_value = "random", value_name = "SEED")]
    pub(crate) shuffle: Option<String>,

    /// Terminal output style: "dots", "verbose" (like -v), or "bar"
    /// (pytest-sugar-style live progress). Config `[tool.rstest] output`.
    /// Default "bar" on a tty ("verbose" with -v), "dots" off-tty.
    #[arg(long, value_name = "STYLE")]
    pub(crate) output: Option<String>,

    /// Split the suite across N independent CI jobs and run only shard K
    /// (`--shard K/N`, K 1-based), balanced by the duration cache. Buckets are
    /// disjoint, so merging per-job JUnit reconstructs the full run.
    #[arg(long, value_name = "K/N")]
    pub(crate) shard: Option<String>,

    /// Shared-cache remote: a directory / `file://` path (local, an NFS/EFS
    /// mount, or a dir a CI step materializes), or an `s3://` / `gs://` bucket
    /// URL driven through the `aws` / `gcloud` CLI already on the runner. Also
    /// settable via `RSTEST_CACHE_REMOTE`. Enables `--cache-pull` /
    /// `--cache-push` / the `cache-compact` subcommand.
    #[arg(long, value_name = "URL|DIR", global = true)]
    pub(crate) cache_remote: Option<String>,

    /// Before the run, merge the remote's segments + base into the local
    /// `.rstest_cache` (durations, flake history). Warms scheduling and the
    /// regression baseline. Needs `--cache-remote`.
    #[arg(long)]
    pub(crate) cache_pull: bool,

    /// After the run, publish THIS run's contribution as one immutable segment
    /// on the remote (durations + flake events). Concurrent shards never
    /// conflict. Needs `--cache-remote`.
    #[arg(long)]
    pub(crate) cache_push: bool,

    /// With a baseline-dependent gate active (`--durations-regress`), treat a
    /// successful pull that returns NO baseline as a hard error instead of a
    /// silent skip — the steady-state guard against a cache that never
    /// restored. A failed pull is always an error.
    #[arg(long)]
    pub(crate) require_baseline: bool,

    /// After a `--cache-push`, if the remote holds more than N loose segments,
    /// fold them into the base inline so the set stays bounded without a
    /// separate `cache-compact` job. The retention window is taken from
    /// `RSTEST_CACHE_KEEP_LAST` / `RSTEST_CACHE_MAX_AGE`. Best-effort: any
    /// failure warns and never fails the run. Env:
    /// `RSTEST_CACHE_COMPACT_THRESHOLD`.
    #[arg(long, value_name = "N", global = true)]
    pub(crate) cache_compact_threshold: Option<usize>,

    /// Run under debugpy for editor (VS Code) debugging: force single-worker
    /// mode with inherited stdio (like `--pdb`), start debugpy in the worker,
    /// and block until a client attaches before collecting. Bare `--debug`
    /// listens on 127.0.0.1:5678; `--debug=PORT` overrides. The target
    /// interpreter must have `debugpy` installed.
    #[arg(long, num_args = 0..=1, default_missing_value = "5678", value_name = "PORT")]
    pub(crate) debug: Option<String>,

    /// Stream per-test results as newline-delimited JSON (one object per test
    /// phase report) to FILE as the run progresses — the live surface a Test
    /// Explorer / editor consumes. FILE may be a regular file or a named pipe
    /// (fifo). Works in every run mode; human output is unaffected.
    #[arg(long = "stream-json", value_name = "FILE")]
    pub(crate) stream_json: Option<PathBuf>,
}

/// -x / --maxfail=N from the session args (also forwarded: each worker
/// session stops itself; the orchestrator does the global coordination).
pub(crate) fn parse_maxfail(args: &[String]) -> Option<u64> {
    let mut limit = None;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-x" | "--exitfirst" => limit = Some(1),
            "--maxfail" => {
                if let Some(v) = it.peek().and_then(|v| v.parse().ok()) {
                    limit = Some(v);
                }
            }
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
    session_args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--collect-only"
                | "--co"
                | "-s"
                | "--capture=no"
                | "--pdb"
                | "--trace"
                | "--sw"
                | "--stepwise"
                | "--sw-skip"
                | "--stepwise-skip"
                | "--sw-reset"
                | "--stepwise-reset"
        ) || a.starts_with("--capture=")
    })
}

pub(crate) fn is_collect_only(session_args: &[String]) -> bool {
    session_args
        .iter()
        .any(|a| a == "--collect-only" || a == "--co")
}

/// Split argv into rstest-owned args (fed to clap) and session args
/// (paths + pytest flags, forwarded verbatim).
pub(crate) fn split_argv() -> (Vec<String>, Vec<String>) {
    split_args(std::env::args().skip(1))
}

/// Single source of truth for the rstest-owned flag surface. Every long/short
/// flag on the `Cli` clap struct must appear in exactly one of these tables;
/// the `every_clap_flag_is_covered_by_the_split_tables` test enforces that, so
/// a new flag can't be silently forwarded to the pytest session.
///
/// `BOOL_FLAGS`: switches that consume no value.
const BOOL_FLAGS: &[&str] = &[
    "--doctor",
    "--watch",
    "--fail-on-leak",
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

/// Run-less subcommand names (see [`Command`]). Recognized only as the LEADING
/// argv token by [`split_args`], before the flag pre-scan; everything after the
/// token is split by the flag tables as usual, so `rstest try -k foo tests/`
/// still forwards `-k foo tests/` to the session. Kept in kebab-case to match
/// clap's derived subcommand names. A pytest path literally named after a
/// subcommand is shadowed (`rstest ./try` / `rstest -- try` disambiguates).
const SUBCOMMANDS: &[&str] = &["verify-vendor", "try", "migrate-check", "cache-compact"];

/// Optional-value flags (`num_args = 0..=1`): a bare `--changed` consumes
/// nothing, an attached `--changed=REV` carries its value inline. Never eats
/// the following argv item (that item is a path / pytest flag).
const OPT_FLAGS: &[&str] = &["--changed", "--shuffle", "--debug"];

/// Flags that take a value: either the following argv item (`--dist load`) or
/// `=`-joined (`--dist=load`). `-n` also accepts the attached short forms
/// `-n4` / `-n=4`, handled in `owned_without_value`.
const VALUE_FLAGS: &[&str] = &[
    "-n",
    "--numprocesses",
    "--python",
    "--report-json",
    "--output",
    "--cache-remote",
    "--migrate-check-json",
    "--migrate-allow",
    "--durations-regress",
    "--only-rerun",
    "--cov-diff-fail-under",
    "--cov-diff-json",
    "--worker-timeout",
    "--timeout",
    "--reruns",
    "--doctor-json",
    "--quarantine",
    "--doctor-md",
    "--doctor-fail-on",
    "--junitxml",
    "--html",
    "--dist",
    "--shard",
    "--collect",
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

pub(crate) fn split_args(argv: impl IntoIterator<Item = String>) -> (Vec<String>, Vec<String>) {
    let mut own = vec!["rstest".to_string()];
    let mut session = Vec::new();
    let mut argv = argv.into_iter().peekable();
    // A run-less subcommand is recognized ONLY as the leading token, so its
    // paired flags (all `global` on `Cli`) follow it and pytest args still route
    // to the session below. A non-leading match is treated as a pytest path.
    if argv
        .peek()
        .is_some_and(|first| SUBCOMMANDS.contains(&first.as_str()))
    {
        own.push(argv.next().unwrap());
    }
    while let Some(arg) = argv.next() {
        if arg == "--" {
            session.extend(argv.by_ref());
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
    (own, session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn durations_forms() {
        assert_eq!(parse_durations(&v(&[])), None);
        assert_eq!(parse_durations(&v(&["--durations=10"])), Some((10, 0.005)));
        assert_eq!(parse_durations(&v(&["--durations", "3"])), Some((3, 0.005)));
        assert_eq!(parse_durations(&v(&["--durations=0"])), Some((0, 0.005)));
        assert_eq!(
            parse_durations(&v(&["--durations=5", "--durations-min=0.1"])),
            Some((5, 0.1))
        );
        // min alone does nothing (pytest needs --durations to render)
        assert_eq!(parse_durations(&v(&["--durations-min=0.1"])), None);
    }

    #[test]
    fn maxfail_forms() {
        assert_eq!(parse_maxfail(&v(&["-x"])), Some(1));
        assert_eq!(parse_maxfail(&v(&["--exitfirst"])), Some(1));
        assert_eq!(parse_maxfail(&v(&["--maxfail", "3"])), Some(3));
        assert_eq!(parse_maxfail(&v(&["--maxfail=7"])), Some(7));
        // maxfail=0 means "no limit" in pytest.
        assert_eq!(parse_maxfail(&v(&["--maxfail=0"])), None);
        assert_eq!(parse_maxfail(&v(&["-k", "x"])), None);
    }

    #[test]
    fn passthrough_flags() {
        assert!(needs_passthrough_io(&v(&["--co"])));
        assert!(needs_passthrough_io(&v(&["-s"])));
        assert!(needs_passthrough_io(&v(&["--pdb"])));
        assert!(needs_passthrough_io(&v(&["--capture=tee-sys"])));
        // Stepwise is sequential: route it to the single-session path so the
        // vendored stepwise plugin owns resume/stop and its cache round-trips.
        assert!(needs_passthrough_io(&v(&["--sw"])));
        assert!(needs_passthrough_io(&v(&["--stepwise"])));
        assert!(needs_passthrough_io(&v(&["--sw-skip"])));
        assert!(needs_passthrough_io(&v(&["--stepwise-skip"])));
        assert!(needs_passthrough_io(&v(&["--sw-reset"])));
        assert!(needs_passthrough_io(&v(&["--stepwise-reset"])));
        assert!(!needs_passthrough_io(&v(&["-k", "x", "-v"])));
    }

    #[test]
    fn every_clap_flag_is_covered_by_the_split_tables() {
        // The split tables are the single source of truth for the rstest-owned
        // flag surface; clap is the other. This asserts they agree, so a flag
        // added to the `Cli` struct without a table entry fails here instead of
        // silently leaking to the pytest session.
        use clap::CommandFactory;
        let covered = |tok: &str| {
            BOOL_FLAGS.contains(&tok) || OPT_FLAGS.contains(&tok) || VALUE_FLAGS.contains(&tok)
        };
        for arg in Cli::command().get_arguments() {
            if let Some(long) = arg.get_long() {
                let tok = format!("--{long}");
                assert!(
                    covered(&tok),
                    "clap flag {tok} missing from split_args tables"
                );
            }
            if let Some(short) = arg.get_short() {
                let tok = format!("-{short}");
                assert!(
                    covered(&tok),
                    "clap short flag {tok} missing from split_args tables"
                );
            }
        }
        // Every clap subcommand must be in SUBCOMMANDS, or split_args would
        // forward its leading token to the pytest session instead of clap.
        for sub in Cli::command().get_subcommands() {
            assert!(
                SUBCOMMANDS.contains(&sub.get_name()),
                "clap subcommand {} missing from SUBCOMMANDS",
                sub.get_name()
            );
        }
    }

    #[test]
    fn split_owns_rstest_flags_and_forwards_the_rest() {
        let (own, session) = split_args(v(&[
            "-n", "4", "--dist", "loadfile", "tests/", "-k", "smoke", "-x",
        ]));
        assert_eq!(own, v(&["rstest", "-n", "4", "--dist", "loadfile"]));
        assert_eq!(session, v(&["tests/", "-k", "smoke", "-x"]));
    }

    #[test]
    fn split_owns_attached_short_numprocesses() {
        // Attached `-n4` and `-n=4` are rstest's, not forwarded to pytest.
        let (own, session) = split_args(v(&["-n4", "tests/"]));
        assert_eq!(own, v(&["rstest", "-n4"]));
        assert_eq!(session, v(&["tests/"]));

        let (own, session) = split_args(v(&["-n=4", "tests/"]));
        assert_eq!(own, v(&["rstest", "-n=4"]));
        assert_eq!(session, v(&["tests/"]));
    }

    #[test]
    fn split_forwards_pytest_collect_flags() {
        let (own, session) = split_args(v(&["--collect-only", "--co"]));
        assert_eq!(own, v(&["rstest"]));
        assert_eq!(session, v(&["--collect-only", "--co"]));
    }

    #[test]
    fn split_double_dash_forwards_everything() {
        let (own, session) = split_args(v(&["--", "-n", "9", "--doctor"]));
        assert_eq!(own, v(&["rstest"]));
        assert_eq!(session, v(&["-n", "9", "--doctor"]));
    }

    #[test]
    fn split_equals_forms() {
        let (own, session) = split_args(v(&["--reruns=2", "--junitxml=o.xml", "-v"]));
        assert_eq!(own, v(&["rstest", "--reruns=2", "--junitxml=o.xml"]));
        assert_eq!(session, v(&["-v"]));
    }

    #[test]
    fn split_owns_leading_subcommand_and_its_global_flags() {
        // A leading subcommand token is rstest-owned; its paired global flag
        // rides along, and pytest args after it still forward to the session.
        let (own, session) = split_args(v(&[
            "migrate-check",
            "--migrate-check-json",
            "o.json",
            "-k",
            "foo",
            "tests/",
        ]));
        assert_eq!(
            own,
            v(&["rstest", "migrate-check", "--migrate-check-json", "o.json"])
        );
        assert_eq!(session, v(&["-k", "foo", "tests/"]));
    }

    #[test]
    fn split_only_recognizes_subcommand_as_leading_token() {
        // `try` as a non-leading token is a pytest path, not the subcommand.
        let (own, session) = split_args(v(&["tests/", "try"]));
        assert_eq!(own, v(&["rstest"]));
        assert_eq!(session, v(&["tests/", "try"]));
    }

    #[test]
    fn clap_parses_leading_subcommand() {
        use clap::Parser;
        let (own, _) = split_args(v(&["verify-vendor"]));
        assert_eq!(Cli::parse_from(&own).command, Some(Command::VerifyVendor));
        // Default (no subcommand) => a normal run.
        assert_eq!(Cli::parse_from(["rstest"]).command, None);
    }

    #[test]
    fn cache_compact_retention_flags_are_owned_not_forwarded() {
        use clap::Parser;
        // Regression: --keep-last / --max-age are value flags; the pre-scan must
        // keep them on the rstest side (space AND =-joined), not forward them to
        // the session, or clap never sees them and the policy silently no-ops.
        let (own, session) = split_args(v(&[
            "cache-compact",
            "--keep-last",
            "200",
            "--max-age=30d",
            "--cache-remote",
            "./rc",
        ]));
        assert!(
            session.is_empty(),
            "nothing should forward, got {session:?}"
        );
        let cli = Cli::parse_from(&own);
        assert!(matches!(
            cli.command,
            Some(Command::CacheCompact {
                keep_last: Some(200),
                max_age: Some(ref d),
            }) if d == "30d"
        ));
    }

    #[test]
    fn split_owns_doctor_md() {
        let (own, session) = split_args(v(&["--doctor-md", "d.md", "--doctor-md=e.md", "-v"]));
        assert_eq!(
            own,
            v(&["rstest", "--doctor-md", "d.md", "--doctor-md=e.md"])
        );
        assert_eq!(session, v(&["-v"]));
    }

    #[test]
    fn split_owns_debug_and_stream_json() {
        // --debug is optional-value (bare consumes nothing, =PORT inline); it
        // must never eat the following path. --stream-json takes a value.
        let (own, session) = split_args(v(&["--debug", "tests/"]));
        assert_eq!(own, v(&["rstest", "--debug"]));
        assert_eq!(session, v(&["tests/"]));

        let (own, session) = split_args(v(&["--debug=5678", "tests/"]));
        assert_eq!(own, v(&["rstest", "--debug=5678"]));
        assert_eq!(session, v(&["tests/"]));

        let (own, session) = split_args(v(&["--stream-json", "out.ndjson", "-k", "x"]));
        assert_eq!(own, v(&["rstest", "--stream-json", "out.ndjson"]));
        assert_eq!(session, v(&["-k", "x"]));
    }

    #[test]
    fn clap_parses_debug_and_stream_json() {
        use clap::Parser;
        let (own, _) = split_args(v(&["--debug=5678", "--stream-json", "o.ndjson"]));
        let cli = Cli::parse_from(&own);
        assert_eq!(cli.debug.as_deref(), Some("5678"));
        assert_eq!(
            cli.stream_json.as_deref(),
            Some(std::path::Path::new("o.ndjson"))
        );
        // Bare --debug falls back to the default port.
        let (own, _) = split_args(v(&["--debug"]));
        assert_eq!(Cli::parse_from(&own).debug.as_deref(), Some("5678"));
    }

    #[test]
    fn split_owns_doctor_fail_on() {
        // Both spaced and =-joined forms are rstest-owned; the value (which
        // contains a `<`/`>`) must not leak into the pytest session args.
        let (own, session) = split_args(v(&[
            "--doctor-fail-on",
            "parallel_efficiency<30",
            "--doctor-fail-on=wait_pct>50",
            "tests/",
        ]));
        assert_eq!(
            own,
            v(&[
                "rstest",
                "--doctor-fail-on",
                "parallel_efficiency<30",
                "--doctor-fail-on=wait_pct>50",
            ])
        );
        assert_eq!(session, v(&["tests/"]));
    }
}
