//! CLI surface: the `Cli` clap struct, the pre-scan that partitions
//! rstest-owned flags from pytest session args (clap can't mirror pytest's
//! plugin-extensible flag surface), and the session-arg parsers.

use std::path::PathBuf;

use clap::Parser;

/// rstest: a fast, pytest-compatible test runner. Unrecognized flags forward
/// to the test session verbatim: clap can't mirror pytest's large,
/// plugin-extensible flag surface, so we pre-scan argv ourselves.
#[derive(Parser, Debug, Clone)]
#[command(name = "rstest", version, disable_help_flag = false)]
pub struct Cli {
    /// Number of worker processes (logical cores); rstest is parallel by
    /// design. Use 0 or 1 for single-worker mode (byte-exact pytest semantics).
    /// Config: `[tool.rstest] numprocesses`. [default: auto]
    #[arg(short = 'n', long = "numprocesses")]
    pub(crate) numprocesses: Option<String>,

    /// Python interpreter to run workers with: a path, or a version request
    /// (`3.12`, `>=3.12,<3.13`, `pypy@3.10`, `3.13t`). Defaults to the active
    /// venv / a discovered `.venv` / `.python-version` / PATH.
    #[arg(long)]
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

    /// Parallel-readiness preflight: collect twice and report tests with
    /// unstable ids, then run -n auto and classify any parallel-only failure
    /// (polluter bisected). Exits non-zero on any such finding.
    #[arg(long)]
    pub(crate) migrate_check: bool,

    /// Write the migrate-check findings as JSON (stable, versioned schema) for
    /// CI gating. Implies --migrate-check.
    #[arg(long)]
    pub(crate) migrate_check_json: Option<PathBuf>,

    /// Substring of a nodeid/site to accept as a known migrate-check finding
    /// (repeatable): it is still reported (marked "allowed") but does not fail
    /// the exit code, so CI can gate on NEW issues while tolerating known ones.
    #[arg(long = "migrate-allow")]
    pub(crate) migrate_allow: Vec<String>,

    /// Zero-config proof: run the suite under plain pytest and under rstest
    /// (-n auto), then report whether outcomes are identical and how much
    /// faster rstest is. The 30-second "should I switch?" answer.
    #[arg(long = "try")]
    pub(crate) r#try: bool,

    /// Distribution mode: "load" (dynamic, duration-aware), "loadfile",
    /// "loadscope", "loadgroup" (xdist_group marker affinity), or "each"
    /// (every test on every worker). [default: load]
    #[arg(long)]
    pub(crate) dist: Option<String>,

    /// Write merged results as junit XML (intercepted: per-worker sessions
    /// would clobber a shared file).
    #[arg(long)]
    pub(crate) junitxml: Option<PathBuf>,

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

    /// Shared-cache remote: a directory or `file://` path (local, an NFS/EFS
    /// mount, or a dir a CI step materializes via `download-artifact` /
    /// `aws s3 sync`). Also settable via `RSTEST_CACHE_REMOTE`. Enables
    /// `--cache-pull` / `--cache-push` / `--cache-compact`.
    #[arg(long, value_name = "URL|DIR")]
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

    /// Maintenance: fold all remote segments into a fresh base and prune them,
    /// then exit without running tests. Needs `--cache-remote`.
    #[arg(long)]
    pub(crate) cache_compact: bool,

    /// With a baseline-dependent gate active (`--durations-regress`), treat a
    /// successful pull that returns NO baseline as a hard error instead of a
    /// silent skip — the steady-state guard against a cache that never
    /// restored. A failed pull is always an error.
    #[arg(long)]
    pub(crate) require_baseline: bool,
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

pub(crate) fn split_args(argv: impl IntoIterator<Item = String>) -> (Vec<String>, Vec<String>) {
    let mut own = vec!["rstest".to_string()];
    let mut session = Vec::new();
    let mut argv = argv.into_iter().peekable();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--doctor" | "--watch" | "--migrate-check" | "--try" | "--fail-on-leak" => {
                own.push(arg)
            }
            "--reruns-only-known-flaky" | "--since-green" | "--incremental" => own.push(arg),
            "--cache-pull" | "--cache-push" | "--cache-compact" | "--require-baseline" => {
                own.push(arg)
            }
            "--cache-remote" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--cache-remote=") => own.push(arg),
            "--migrate-check-json" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--migrate-check-json=") => own.push(arg),
            "--migrate-allow" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--migrate-allow=") => own.push(arg),
            "--changed" | "--changed-strict" | "--shuffle" => own.push(arg),
            _ if arg.starts_with("--changed=") => own.push(arg),
            _ if arg.starts_with("--shuffle=") => own.push(arg),
            // Exact match only: --durations / --durations-min stay session args.
            "--durations-regress" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--durations-regress=") => own.push(arg),
            "--only-rerun" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--only-rerun=") => own.push(arg),
            "--worker-timeout" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--worker-timeout=") => own.push(arg),
            "--reruns" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--reruns=") => own.push(arg),
            "--doctor-json" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--doctor-json=") => own.push(arg),
            "--quarantine" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--quarantine=") => own.push(arg),
            "--doctor-md" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--doctor-md=") => own.push(arg),
            "--doctor-fail-on" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--doctor-fail-on=") => own.push(arg),
            "--junitxml" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--junitxml=") => own.push(arg),
            "--dist" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--dist=") => own.push(arg),
            "--shard" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--shard=") => own.push(arg),
            // Exact "--collect" only: --collect-only/--co stay session args.
            "--collect" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--collect=") => own.push(arg),
            "-n" | "--numprocesses" | "--python" | "--report-json" | "--output" => {
                own.push(arg);
                if let Some(v) = argv.next() {
                    own.push(v);
                }
            }
            _ if arg.starts_with("--numprocesses=")
                || arg.starts_with("--python=")
                || arg.starts_with("--report-json=")
                || arg.starts_with("--output=")
                || arg.starts_with("-n=") =>
            {
                own.push(arg);
            }
            "-h" | "--help" | "-V" | "--version" => own.push(arg),
            "--" => session.extend(argv.by_ref()),
            _ => session.push(arg),
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
    fn split_owns_rstest_flags_and_forwards_the_rest() {
        let (own, session) = split_args(v(&[
            "-n", "4", "--dist", "loadfile", "tests/", "-k", "smoke", "-x",
        ]));
        assert_eq!(own, v(&["rstest", "-n", "4", "--dist", "loadfile"]));
        assert_eq!(session, v(&["tests/", "-k", "smoke", "-x"]));
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
    fn split_owns_doctor_md() {
        let (own, session) = split_args(v(&["--doctor-md", "d.md", "--doctor-md=e.md", "-v"]));
        assert_eq!(
            own,
            v(&["rstest", "--doctor-md", "d.md", "--doctor-md=e.md"])
        );
        assert_eq!(session, v(&["-v"]));
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
