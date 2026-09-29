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
    /// faster rstest is. The one-command "should I switch?" answer (costs one
    /// serial pytest run plus one rstest run).
    Try,

    /// Parallel-readiness preflight: collect twice and report tests with
    /// unstable ids, then run -n auto and classify any parallel-only failure
    /// (polluter bisected). Exits non-zero on any such finding. Combine with
    /// `--migrate-check-json` / `--migrate-allow`.
    MigrateCheck,

    /// Auto parallel-safety audit: run the suite under -n auto (repeat with
    /// `--audit-repeat` to catch probabilistic flakes), diff against the -n 0
    /// oracle, and print the tests that fail ONLY in parallel with a
    /// ready-to-paste `@pytest.mark.serial` fix-list. Exits non-zero on any
    /// parallel-only failure. `--audit-json` writes the findings for CI.
    Audit,

    /// Order-dependency bisect: for a test that fails only when run after some
    /// other test, delta-debug the predecessor set at -n 0 to the minimal set of
    /// earlier tests that reproduce the failure — the polluter(s). Prints the
    /// culprits and a minimal reproducing command. `--bisect-json` writes it.
    Bisect {
        /// The failing test's nodeid (`path::test[param]`), rootdir- or
        /// cwd-relative.
        #[arg(value_name = "NODEID")]
        nodeid: String,
        /// pytest options after `--` (`-p plugin`, `-o key=val`, `-m expr`),
        /// applied to the collection and every child run. Not test paths:
        /// bisect selects tests by nodeid itself.
        #[arg(last = true, value_name = "PYTEST_ARGS")]
        pytest_args: Vec<String>,
    },

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

    /// Verify a sharded run covered the whole suite. Reads the per-shard
    /// report-json files (each written with `--report-json` while `--shard`
    /// was active) and checks the shards agree on one collection and that
    /// every collected test ran on exactly one shard. Exits non-zero if any
    /// test was dropped or ran on two shards, or if the shards disagree. Reads
    /// only the files, so it needs no interpreter.
    ShardVerify {
        /// The per-shard report-json files (one per shard, in any order).
        #[arg(value_name = "REPORT_JSON", required = true, num_args = 1..)]
        reports: Vec<PathBuf>,
    },

    /// Re-run a recorded parallel schedule. Every pool run (`-n >= 2`) journals
    /// its exact per-worker assignment + order to `.rstest_cache/replay/`;
    /// `replay` pins that schedule so a parallel-only failure reproduces. The
    /// journal keys on nodeid, so a run journaled on CI replays locally: upload
    /// `.rstest_cache/replay/latest.json` as an artifact and pass it with
    /// `--journal`. Reruns and work-stealing are off; the recorded shuffle is
    /// already baked into the pinned order.
    Replay {
        /// Which recorded run to replay: a run-uid (the journal file stem under
        /// `.rstest_cache/replay/`). Omit to replay the most recent local run.
        #[arg(value_name = "RUN_ID")]
        run_id: Option<String>,
        /// Replay a journal FILE directly (typically a downloaded CI artifact)
        /// instead of one from the local cache. Takes precedence over RUN_ID.
        #[arg(long, value_name = "FILE")]
        journal: Option<PathBuf>,
    },

    /// Print one test's dossier from the caches without running anything:
    /// last recorded duration, flake/fail history, last-green outcome, and the
    /// coverage footprint (files it covered). Merges `durations.json`,
    /// `flakes.json`, `incremental_outcomes.json`, and `coverage_index.json`
    /// for the given nodeid. Reads only cache files, so it needs no interpreter.
    Explain {
        /// The test nodeid to explain, e.g. `tests/test_x.py::test_y`.
        #[arg(value_name = "NODEID", required = true)]
        nodeid: String,
        /// Emit the dossier as JSON (schema-stamped) to stdout for tooling,
        /// instead of the human-readable report.
        #[arg(long)]
        json: bool,
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

    /// Fork-prewarm the worker pool (Unix only): import the vendored pytest core
    /// once in a zygote, then fork the workers off it instead of paying that
    /// import in every freshly spawned worker. Cuts pool startup at high `-n`;
    /// no effect on Windows or single-worker runs. [default: off]
    #[arg(long = "fork-pool")]
    pub(crate) fork_pool: bool,

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

    /// Internal: turn on the workers' cpu/fixture instrumentation without the
    /// doctor report. Passed only by a parent rstest (migrate-check's
    /// classifier runs); a flag rather than an env var so a user's shell or CI
    /// environment can never switch it on.
    #[arg(long = "instrument-workers", hide = true)]
    pub(crate) instrument_workers: bool,

    /// Write the migrate-check findings as JSON (stable, versioned schema) for
    /// CI gating. Used with the `migrate-check` subcommand.
    #[arg(long, global = true)]
    pub(crate) migrate_check_json: Option<PathBuf>,

    /// Substring of a nodeid/site to accept as a known migrate-check finding
    /// (repeatable): it is still reported (marked "allowed") but does not fail
    /// the exit code, so CI can gate on NEW issues while tolerating known ones.
    #[arg(long = "migrate-allow", global = true)]
    pub(crate) migrate_allow: Vec<String>,

    /// Write the `audit` findings as JSON (stable, versioned schema) for CI
    /// gating. Used with the `audit` subcommand.
    #[arg(long, global = true)]
    pub(crate) audit_json: Option<PathBuf>,

    /// How many times `audit` repeats the `-n auto` run; a parallel flake is
    /// probabilistic, so more repeats catch more of them. [default: 1]
    #[arg(long, global = true, value_name = "N")]
    pub(crate) audit_repeat: Option<u32>,

    /// Write the `bisect` result as JSON (culprits + reproduce command) for
    /// tooling. Used with the `bisect` subcommand.
    #[arg(long, global = true)]
    pub(crate) bisect_json: Option<PathBuf>,

    /// Distribution mode: "load" (dynamic, duration-aware), "loadfile",
    /// "loadscope", "loadgroup" (xdist_group marker affinity), or "each"
    /// (every test on every worker). [default: load]
    #[arg(long)]
    pub(crate) dist: Option<String>,

    /// Dispatch ordering under `--dist load`: "throughput" (slow tests first,
    /// to pack workers — the default) or "fail-fast" (recently-failed, then
    /// flaky tests first, then the throughput order for the rest, for the
    /// earliest possible red signal). Pairs with `--maxfail`/`-x` for true
    /// early exit. Auto-selects fail-fast under `--watch`. Config
    /// `[tool.rstest] order`.
    #[arg(long, value_name = "MODE")]
    pub(crate) order: Option<String>,

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

    /// Run only tests affected by changed files. Coverage-aware when a warm
    /// coverage map is present (any prior `--cov --cov-context=test` run writes
    /// it): changed lines map to the exact covering tests, and the run reports
    /// how many of the mapped tests are affected. A cold map degrades to
    /// import-graph reachability with a one-line hint. Without a value: working
    /// tree + untracked vs HEAD. With a value: vs that git rev (e.g.
    /// --changed=origin/main in CI).
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
    /// demand). Config `[tool.rstest] collect`. [default: auto — lazy for a
    /// big-enough parallel run (>=2000 known tests and tests*workers>=16000 on
    /// a file-affine dist), else full; first/cold-cache run stays full]
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

    /// Output style: "dots", "verbose" (like -v), or "bar" (pytest-sugar-style
    /// live progress) for terminals; "github", "gitlab", "buildkite",
    /// "teamcity", or "azure" for CI annotations; "tap" or "json" for
    /// machine-readable streams. Config `[tool.rstest] output`. Default "bar"
    /// on a tty ("verbose" with -v), "dots" off-tty. An unknown style warns and
    /// falls back to "dots".
    #[arg(long, value_name = "STYLE")]
    pub(crate) output: Option<String>,

    /// Split the suite across N independent CI jobs and run only shard K
    /// (`--shard K/N`, K 1-based), balanced by the duration cache. Buckets are
    /// disjoint when every job sees the same collection and duration cache;
    /// `rstest shard-verify` proves it after the fact. Needs `-n 2` or more.
    #[arg(long, value_name = "K/N")]
    pub(crate) shard: Option<String>,

    /// Shared-cache remote: a directory / `file://` path (local, an NFS/EFS
    /// mount, or a dir a CI step materializes), an `s3://` / `gs://` bucket
    /// URL driven through the `aws` / `gcloud` CLI already on the runner, or an
    /// `http(s)://` endpoint (bearer token from `RSTEST_CACHE_REMOTE_TOKEN`;
    /// needs the default `http-cache` build feature). Also
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
    "--fork-pool",
    "--watch",
    "--fail-on-leak",
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

/// Run-less subcommand names (see [`Command`]). Recognized only as the LEADING
/// argv token by [`split_args`], before the flag pre-scan; everything after the
/// token is split by the flag tables as usual, so `rstest try -k foo tests/`
/// still forwards `-k foo tests/` to the session. Kept in kebab-case to match
/// clap's derived subcommand names. A pytest path literally named after a
/// subcommand is shadowed (`rstest ./try` / `rstest -- try` disambiguates).
const SUBCOMMANDS: &[&str] = &[
    "verify-vendor",
    "try",
    "migrate-check",
    "audit",
    "bisect",
    "cache-compact",
    "shard-verify",
    "replay",
    "explain",
];

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
    "--doctor-md",
    "--doctor-fail-on",
    "--junitxml",
    "--html",
    "--dist",
    "--shard",
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
        let sub = argv.next().unwrap();
        // `shard-verify` (report-json paths), `replay` (a run-id / `--journal`),
        // `bisect` (a single nodeid) and `explain` (a nodeid) build no pytest
        // session from argv: every token after them is a clap positional or a
        // subcommand-local flag, so route them all to `own` rather than
        // forwarding non-flag tokens to the (nonexistent argv-built) session.
        // The nodeids contain `::`, which the flag tables would otherwise route
        // to the session and hide from clap. `replay` gets its real session
        // args from the journal, not argv.
        let consumes_all =
            sub == "shard-verify" || sub == "replay" || sub == "bisect" || sub == "explain";
        own.push(sub);
        if consumes_all {
            own.extend(argv.by_ref());
            return (own, session);
        }
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
    fn shard_verify_routes_all_positionals_to_clap() {
        // Report paths after `shard-verify` are clap positionals, not forwarded
        // pytest args (shard-verify runs no session).
        let (own, session) = split_args(v(&["shard-verify", "a.json", "b.json"]));
        assert_eq!(own, v(&["rstest", "shard-verify", "a.json", "b.json"]));
        assert!(session.is_empty(), "session={session:?}");
    }

    #[test]
    fn replay_routes_run_id_and_journal_to_clap() {
        use clap::Parser;
        // `replay` builds no session from argv: the run-id positional and
        // `--journal` are clap tokens, nothing forwards to pytest.
        let (own, session) = split_args(v(&["replay", "myrun", "--journal", "ci.json"]));
        assert_eq!(
            own,
            v(&["rstest", "replay", "myrun", "--journal", "ci.json"])
        );
        assert!(session.is_empty(), "session={session:?}");
        let cli = Cli::parse_from(&own);
        assert!(matches!(
            cli.command,
            Some(Command::Replay { run_id: Some(ref r), journal: Some(ref j) })
                if r == "myrun" && j == std::path::Path::new("ci.json")
        ));
        // Bare `replay` => replay the latest local run.
        let (own, _) = split_args(v(&["replay"]));
        assert!(matches!(
            Cli::parse_from(&own).command,
            Some(Command::Replay {
                run_id: None,
                journal: None
            })
        ));
    }

    #[test]
    fn explain_routes_nodeid_and_json_to_clap() {
        use clap::Parser;
        // `explain` runs no session: the nodeid positional and `--json` are clap
        // tokens, not forwarded to pytest, even though the nodeid is a non-flag.
        let (own, session) = split_args(v(&["explain", "t/x.py::test_a", "--json"]));
        assert_eq!(own, v(&["rstest", "explain", "t/x.py::test_a", "--json"]));
        assert!(session.is_empty(), "session={session:?}");
        let cli = Cli::parse_from(&own);
        assert!(matches!(
            cli.command,
            Some(Command::Explain { ref nodeid, json: true }) if nodeid == "t/x.py::test_a"
        ));
    }

    #[test]
    fn bisect_routes_the_nodeid_to_clap() {
        // The nodeid contains `::` and `[param]`; it must reach clap as the
        // positional, not be forwarded to a (nonexistent) pytest session.
        let (own, session) = split_args(v(&[
            "bisect",
            "tests/test_a.py::test_v[1-x]",
            "--bisect-json",
            "out.json",
        ]));
        assert_eq!(
            own,
            v(&[
                "rstest",
                "bisect",
                "tests/test_a.py::test_v[1-x]",
                "--bisect-json",
                "out.json",
            ])
        );
        assert!(session.is_empty(), "session={session:?}");
    }

    #[test]
    fn bisect_keeps_pytest_args_after_double_dash_for_clap() {
        // `--` after `bisect` must not trigger the forward-everything-to-the-
        // session rule: the pytest args belong to bisect's own positional.
        let argv = ["bisect", "t.py::v", "--", "-p", "no:randomly", "-o", "x=1"];
        let (own, session) = split_args(v(&argv));
        assert!(session.is_empty(), "session={session:?}");
        let cli = Cli::try_parse_from(own).unwrap();
        assert_eq!(
            bisect_parts(&cli),
            Some(("t.py::v", v(&["-p", "no:randomly", "-o", "x=1"])))
        );
    }

    /// The `bisect` subcommand's (nodeid, pytest args), or `None` for any other
    /// command line.
    fn bisect_parts(cli: &Cli) -> Option<(&str, Vec<String>)> {
        match &cli.command {
            Some(Command::Bisect {
                nodeid,
                pytest_args,
            }) => Some((nodeid.as_str(), pytest_args.clone())),
            _ => None,
        }
    }

    #[test]
    fn bisect_parses_nodeid_and_json_path() {
        let cli = Cli::try_parse_from(v(&[
            "rstest",
            "bisect",
            "t.py::test_v",
            "--bisect-json",
            "b.json",
        ]))
        .unwrap();
        assert_eq!(bisect_parts(&cli), Some(("t.py::test_v", vec![])));
        // Any other command line is not a bisect.
        assert_eq!(bisect_parts(&Cli::parse_from(["rstest"])), None);
        assert_eq!(
            cli.bisect_json.as_deref(),
            Some(std::path::Path::new("b.json"))
        );

        // The nodeid is required.
        assert!(Cli::try_parse_from(v(&["rstest", "bisect"])).is_err());
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
        // Clustered, either position; `-x` after a value flag is its value.
        assert_eq!(parse_maxfail(&v(&["-xv"])), Some(1));
        assert_eq!(parse_maxfail(&v(&["-vx"])), Some(1));
        assert_eq!(parse_maxfail(&v(&["-kx"])), None);
        assert_eq!(parse_maxfail(&v(&["-rx"])), None);
    }

    #[test]
    fn short_switches_expand_clusters_up_to_a_value_flag() {
        assert_eq!(short_switches("-sv"), vec!['s', 'v']);
        assert_eq!(short_switches("-xrA"), vec!['x', 'r']);
        assert_eq!(short_switches("-ksmoke"), vec!['k']);
        assert!(short_switches("--verbose").is_empty());
        assert!(short_switches("tests/").is_empty());
        assert!(short_switches("-").is_empty());
    }

    #[test]
    fn passthrough_sees_clustered_s_and_two_token_capture() {
        for args in [
            &["-sv"][..],
            &["-vs"],
            &["-xsv"],
            &["--capture", "no"],
            &["--capture=no"],
        ] {
            assert!(
                needs_passthrough_io(&v(args)),
                "{args:?} should force passthrough"
            );
        }
        // `-rs` / `-ks`: `s` is the value of `-r` / `-k`.
        for args in [&["-v"][..], &["-rs"], &["-ks"]] {
            assert!(
                !needs_passthrough_io(&v(args)),
                "{args:?} must not force passthrough"
            );
        }
    }

    #[test]
    fn path_args_skip_option_values_that_exist_on_disk() {
        // `src` exists (cwd is the crate dir under cargo test): as a value it
        // is not a selection, as a positional it is.
        let args = v(&[
            "-k",
            "src",
            "--ignore",
            "src",
            "--cov",
            "src",
            "-xk",
            "src",
            "--tb=short",
            "src",
        ]);
        assert_eq!(path_args(&args), vec!["src"]);
        assert_eq!(
            without_path_args(&args),
            v(&[
                "-k",
                "src",
                "--ignore",
                "src",
                "--cov",
                "src",
                "-xk",
                "src",
                "--tb=short"
            ])
        );
        // `--cov` without a value leaves the next positional alone.
        assert_eq!(path_args(&v(&["--cov", "-q", "src"])), vec!["src"]);
        // After `--` everything is positional.
        assert_eq!(path_args(&v(&["--", "src"])), vec!["src"]);
        // `@argsfile` counts by its file; a missing path isn't a selection.
        assert_eq!(path_args(&v(&["@src", "no/such/dir"])), vec!["@src"]);
        assert_eq!(without_path_args(&v(&["@src", "-q"])), v(&["-q"]));
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
    fn instrument_workers_is_a_hidden_owned_flag() {
        use clap::{CommandFactory, Parser};
        // Owned by clap (never forwarded to pytest) and parsed as a switch.
        let (own, session) = split_args(v(&["--instrument-workers", "tests/"]));
        assert_eq!(own, v(&["rstest", "--instrument-workers"]));
        assert_eq!(session, v(&["tests/"]));
        assert!(Cli::parse_from(&own).instrument_workers);
        assert!(!Cli::parse_from(["rstest"]).instrument_workers);
        // Internal plumbing for a parent rstest: kept out of --help.
        let cmd = Cli::command();
        let arg = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some("instrument-workers"))
            .unwrap();
        assert!(arg.is_hide_set());
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
