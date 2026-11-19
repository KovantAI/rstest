//! CLI surface: the `Cli` clap struct, the pre-scan that partitions
//! rstest-owned flags from pytest session args (clap can't mirror pytest's
//! plugin-extensible flag surface), and the session-arg parsers.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

mod argv;

pub(crate) use argv::*;

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
    /// unstable ids, then run in parallel and classify any parallel-only failure
    /// (polluter bisected). Exits non-zero on any such finding. Combine with
    /// `--migrate-check-json` / `--migrate-allow`.
    MigrateCheck,

    /// Readiness check for uninstalling pytest-xdist: scan the pytest config,
    /// `PYTEST_ADDOPTS`, the project's sources and the installed plugins for
    /// what breaks or changes once xdist is gone (xdist-only flags, `-n` /
    /// `--dist` in addopts, `import xdist`, xdist hook implementations,
    /// `hasplugin("xdist")` gates), with the fix per finding. Exits non-zero on
    /// any blocking finding. `--xdist-trial` also runs the suite with xdist
    /// hidden; `--xdist-removal-json` writes the findings for CI.
    XdistRemovalCheck,

    /// Auto parallel-safety audit: run the suite in parallel (repeat with
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

    /// Rank the suite's flaky tests from the per-test run history
    /// (`.rstest_cache/history.msgpack`) without running anything: flips
    /// (a pass only on rerun, or a failure right after a pass), the recent
    /// outcome strip, whether it is getting worse, and which tests
    /// `--auto-quarantine` catches on the next run. Needs no interpreter.
    Flaky {
        /// Show the N most-flipping tests; 0 shows all.
        #[arg(long, value_name = "N", default_value_t = 20)]
        top: usize,
        /// Emit the ranking as JSON (schema-stamped) to stdout for
        /// dashboards, instead of the table.
        #[arg(long)]
        json: bool,
    },

    /// Write a self-contained HTML dashboard of the per-test run history
    /// (`.rstest_cache/history.msgpack`) without running anything: failing and
    /// flaky tests per run, test time per run, the flaky ranking, the slowest
    /// tests, duration drift and the runs. Needs no interpreter.
    Report {
        /// Where to write it (default `.rstest_cache/report.html`).
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },

    /// What changed since the last green run, without running tests: commits,
    /// interpreter version, installed packages and a few test-relevant
    /// environment variables. Every whole-suite run that exits 0 on a clean
    /// tree records the state it compares against
    /// (`.rstest_cache/green_env.json`).
    SinceGreen {
        /// Emit the diff as JSON (schema-stamped) to stdout.
        #[arg(long)]
        json: bool,
    },

    /// Install the rstest agent skills (`migrate-to-rstest`, `rstest-triage`)
    /// bundled with this binary, so they match its flags and subcommands.
    /// Writes `.claude/skills/` in the current directory by default (commit it
    /// to share with the team). A skill already installed with different
    /// contents is left alone unless `--force`. Needs no interpreter.
    InstallSkills {
        /// Install for every project: `~/.claude/skills/` (or
        /// `~/.agents/skills/` with `--agents`).
        #[arg(long)]
        user: bool,
        /// Write `.agents/skills/` instead of `.claude/skills/`, for Codex and
        /// other agents that read the Agent Skills layout.
        #[arg(long)]
        agents: bool,
        /// Install into DIR instead (each skill lands in `DIR/<skill>/`).
        /// Overrides `--user` / `--agents`.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Overwrite installed skills that differ from the bundled copy. Files
        /// you added to a skill directory are kept.
        #[arg(long)]
        force: bool,
    },
}

// Unrecognized flags are split off by our own argv pre-scan: clap can't mirror
// pytest's large, plugin-extensible flag surface.
/// rstest: a fast, pytest-compatible test runner. Unrecognized flags are
/// forwarded to the test session verbatim.
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
    #[arg(short = 'n', long = "numprocesses", value_parser = numprocesses_arg)]
    pub(crate) numprocesses: Option<String>,

    /// Fork-prewarm the worker pool (Unix only): import once in a template
    /// process, then fork the workers off it instead of paying that import in
    /// every freshly spawned worker. `core` (bare `--fork-pool`) imports the
    /// vendored pytest core; `app` also imports your app's modules, and
    /// under `--watch` keeps the template for the session. No effect on
    /// Windows or single-worker runs. [default: off; `app` under `--watch`]
    #[arg(
        long = "fork-pool",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "core",
        value_enum
    )]
    pub(crate) fork_pool: Option<crate::scheduling::warm::ForkPool>,

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

    /// Fail the run if any test leaks a thread or file descriptor (one it
    /// created that is still open after its teardown). Turns the leak signal into a CI gate; enables
    /// the leak-check instrumentation on its own (no --doctor needed).
    #[arg(long = "fail-on-leak")]
    pub(crate) fail_on_leak: bool,

    /// Opt-in: scan installed plugins and loaded conftests for code that only
    /// works on an xdist controller (rstest has none) and warn about the ones
    /// that will crash or silently do nothing at `-n >= 2`. `rstest try` turns
    /// it on for its rstest run.
    #[arg(long = "warn-on-dead-master-path")]
    pub(crate) warn_on_dead_master_path: bool,

    /// Internal: turn on the workers' cpu/fixture instrumentation without the
    /// doctor report. Passed only by a parent rstest (migrate-check's
    /// classifier runs); a flag rather than an env var so a user's shell or CI
    /// environment can never switch it on. Also marks the run as a probe that
    /// records no flip history.
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

    /// Write the `xdist-removal-check` findings as JSON (stable, versioned
    /// schema) for CI gating. Used with the `xdist-removal-check` subcommand.
    #[arg(long, global = true)]
    pub(crate) xdist_removal_json: Option<PathBuf>,

    /// With `xdist-removal-check`: also run the suite with pytest-xdist hidden
    /// from the plugin manager (`-p no:xdist`, which drops its options and
    /// hook specs like an uninstall; `import xdist` still succeeds),
    /// and, when xdist is installed, once with it loaded, to name the tests
    /// that only pass with it.
    #[arg(long, global = true)]
    pub(crate) xdist_trial: bool,

    /// Write the `audit` findings as JSON (stable, versioned schema) for CI
    /// gating. Used with the `audit` subcommand.
    #[arg(long, global = true)]
    pub(crate) audit_json: Option<PathBuf>,

    /// How many times `audit` repeats the parallel run; a parallel flake is
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
    /// tests near changed code, then flaky tests first, then the throughput
    /// order for the rest, for the earliest possible red signal). Pairs with `--maxfail`/`-x` for true
    /// early exit. Auto-selects fail-fast under `--watch`. Config
    /// `[tool.rstest] order`.
    #[arg(long, value_name = "MODE")]
    pub(crate) order: Option<String>,

    /// Write merged results as junit XML (intercepted: per-worker sessions
    /// would clobber a shared file). `--junit-xml` is pytest's alias.
    #[arg(long, alias = "junit-xml")]
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

    /// Quarantine tests whose recent history (`.rstest_cache/
    /// history.msgpack`) flipped at least N times: a failure right after a
    /// pass on the same clean commit, or a pass only on rerun. On by default
    /// (N=3); =0 turns it off.
    /// A test that broke and stays broken flips once and still fails the
    /// run; one whose last 3 results all failed is released whatever its past
    /// flips. Unions with --quarantine; a test is also released once its
    /// flips age out. Attach N with `=`.
    #[arg(long, num_args = 0..=1, default_missing_value = "3", value_name = "N")]
    pub(crate) auto_quarantine: Option<u32>,

    /// Ceiling for --auto-quarantine: when more auto-quarantined tests fail
    /// than PCT percent of this run's tests (default 5, at least one test),
    /// none of them is quarantined and the run warns: that many flappers
    /// failing at once points at a broken environment, not flaky tests.
    #[arg(long, value_name = "PCT", value_parser = clap::value_parser!(u32).range(0..=100))]
    pub(crate) auto_quarantine_max: Option<u32>,

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
    /// The baseline advances only when a run is fully green on a clean working
    /// tree (no uncommitted or untracked changes), so a failing test keeps being
    /// selected until it passes at a commit. First run (no baseline) runs
    /// everything. Ignored when --changed is given explicitly.
    #[arg(long = "since-green")]
    pub(crate) since_green: bool,

    /// Dispatch-level incremental testing: collect the whole suite, then SKIP
    /// running any test that was green last run and whose covered source and
    /// transitively imported modules are byte-identical now (content-addressed
    /// via the coverage index; git not required). Skipped tests are carried
    /// forward as cached passes. Needs a warm coverage index (a prior
    /// `--cov-context=test` run); full collection + `--dist load` only. A
    /// config-file or git-tracked non-Python file change disables skipping for
    /// that run.
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
    /// machine-readable streams. Config `[tool.rstest] output`. Default at
    /// -n 0/1 (no --reruns): pytest's own terminal output. Default in the
    /// parallel pool: "bar" on a tty ("verbose" with -v), "dots" off-tty. Any
    /// other value is not rstest's: `--output VALUE` goes to the pytest
    /// session unchanged (a plugin's own `--output`, e.g. pytest-playwright's
    /// artifacts dir).
    #[arg(long, value_name = "STYLE")]
    pub(crate) output: Option<String>,

    /// Split the suite across N independent CI jobs and run only shard K
    /// (`--shard K/N`, K 1-based), balanced by the duration cache. Buckets are
    /// disjoint when every job sees the same collection and duration cache;
    /// `rstest shard-verify` proves it after the fact. Needs `-n 2` or more.
    #[arg(long, value_name = "K/N")]
    pub(crate) shard: Option<String>,

    /// Run the tests most worth running in this much wall time (`90s`, `5m`),
    /// from recorded durations: test files changed since the last green run
    /// always, then recently failed, near-change and flaky tests, then the
    /// shortest of the rest. An estimate that picks tests, not a timeout. A
    /// budget run is never a green baseline. Under `--shard` each shard gets
    /// the whole budget. Needs `-n 2` or more; implies `--order fail-fast`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) budget: Option<String>,

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

impl Cli {
    /// The files a run writes itself. The clean-tree checks skip them, or a
    /// report left untracked in the checkout would mark every later run dirty.
    pub(crate) fn run_outputs(&self) -> Vec<&Path> {
        [
            &self.report_json,
            &self.doctor_json,
            &self.doctor_md,
            &self.junitxml,
            &self.html,
            &self.cov_diff_json,
            &self.stream_json,
        ]
        .into_iter()
        .filter_map(|p| p.as_deref())
        .collect()
    }
}

/// clap validator for `-n`: reject a non-count up front as a usage error
/// (naming the flag and value) instead of failing later with a bare
/// `invalid digit found in string`. The value stays a string (`auto` kept
/// verbatim for the banner).
fn numprocesses_arg(s: &str) -> Result<String, String> {
    if crate::config::is_valid_numprocesses(s) {
        Ok(s.to_string())
    } else {
        Err(format!("expected {}", crate::config::NUMPROCESSES_EXPECTED))
    }
}

/// `--auto-quarantine` / `--auto-quarantine-max` defaults.
pub(crate) const AUTO_QUARANTINE: u32 = 3;
pub(crate) const AUTO_QUARANTINE_MAX: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn output_routes_by_value() {
        // One of rstest's styles: owned.
        for args in [&["--output", "json"][..], &["--output=tap"][..]] {
            let (own, session) = split_args(v(args));
            assert!(session.is_empty(), "{args:?} leaked to the session");
            let cli = Cli::try_parse_from(own).unwrap();
            assert!(cli.output.is_some());
        }
        // Anything else is a plugin's `--output` (pytest-playwright): forwarded.
        let (own, session) = split_args(v(&["-n", "2", "--output", "artifacts", "t.py"]));
        assert_eq!(own, v(&["rstest", "-n", "2"]));
        assert_eq!(session, v(&["--output", "artifacts", "t.py"]));
        let (own, session) = split_args(v(&["--output=test-results"]));
        assert_eq!(own, v(&["rstest"]));
        assert_eq!(session, v(&["--output=test-results"]));
        // A bare trailing `--output` stays rstest's (clap reports the missing value).
        let (own, _) = split_args(v(&["--output"]));
        assert_eq!(own, v(&["rstest", "--output"]));
        assert_eq!(forwarded_output(&v(&["-x", "--output", "a"])), Some("a"));
        assert_eq!(forwarded_output(&v(&["--output=b"])), Some("b"));
        assert_eq!(forwarded_output(&v(&["-x"])), None);
    }

    #[test]
    fn junit_xml_alias_is_owned_like_junitxml() {
        // pytest spells it both ways; forwarding the alias to every worker
        // would bring back the shared-file clobbering the interception avoids.
        for args in [&["--junit-xml", "o.xml"][..], &["--junit-xml=o.xml"][..]] {
            let (own, session) = split_args(v(args));
            assert!(session.is_empty(), "{args:?} leaked to the session");
            let cli = Cli::try_parse_from(own).unwrap();
            assert_eq!(cli.junitxml.as_deref(), Some(std::path::Path::new("o.xml")));
        }
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
    fn run_outputs_lists_every_report_path_given() {
        use clap::Parser;
        let cli = Cli::parse_from(["rstest"]);
        assert!(cli.run_outputs().is_empty());
        let paths = [
            "--report-json=a",
            "--doctor-json=b",
            "--doctor-md=c",
            "--junitxml=d",
            "--html=e",
            "--cov-diff-json=f",
            "--stream-json=g",
        ];
        let cli = Cli::parse_from(std::iter::once("rstest").chain(paths));
        let got: Vec<_> = cli
            .run_outputs()
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect();
        assert_eq!(got, ["a", "b", "c", "d", "e", "f", "g"]);
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
    fn flaky_routes_its_flags_to_clap() {
        use clap::Parser;
        // `--top` is in no flag table: without `flaky` consuming its tail it
        // would leak to a pytest session.
        let (own, session) = split_args(v(&["flaky", "--top", "5", "--json"]));
        assert_eq!(own, v(&["rstest", "flaky", "--top", "5", "--json"]));
        assert!(session.is_empty(), "session={session:?}");
        let cli = Cli::parse_from(&own);
        assert!(matches!(
            cli.command,
            Some(Command::Flaky { top: 5, json: true })
        ));
        let cli = Cli::parse_from(v(&["rstest", "flaky"]));
        assert!(matches!(
            cli.command,
            Some(Command::Flaky {
                top: 20,
                json: false
            })
        ));
    }

    #[test]
    fn report_routes_out_to_clap() {
        use clap::Parser;
        // `--out` is in no flag table: `report` consumes its tail.
        let (own, session) = split_args(v(&["report", "--out", "r.html"]));
        assert_eq!(own, v(&["rstest", "report", "--out", "r.html"]));
        assert!(session.is_empty(), "session={session:?}");
        let cli = Cli::parse_from(&own);
        assert!(matches!(
            cli.command,
            Some(Command::Report { out: Some(ref p) }) if p == std::path::Path::new("r.html")
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
    fn numprocesses_rejects_non_count_at_parse_time() {
        // Regression: `-n abc` used to parse fine and later die with a bare
        // `Error: invalid digit found in string` (no flag, no value named).
        for good in ["auto", "0", "4", "16"] {
            let cli = Cli::try_parse_from(v(&["rstest", "-n", good])).unwrap();
            assert_eq!(cli.numprocesses.as_deref(), Some(good));
        }
        for bad in ["abc", "2 ", "4.0", "", "logical"] {
            let err = Cli::try_parse_from(v(&["rstest", "-n", bad]))
                .expect_err(bad)
                .to_string();
            assert!(err.contains("-n"), "{bad:?}: {err}");
            assert!(
                err.contains("non-negative integer or \"auto\""),
                "{bad:?}: {err}"
            );
        }
        // The long and attached forms go through the same validator.
        assert!(Cli::try_parse_from(v(&["rstest", "--numprocesses=x"])).is_err());
        assert!(Cli::try_parse_from(v(&["rstest", "-nx"])).is_err());
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
        // `try` after a positional is a pytest path, not the subcommand.
        let (own, session) = split_args(v(&["tests/", "try"]));
        assert_eq!(own, v(&["rstest"]));
        assert_eq!(session, v(&["tests/", "try"]));
        // ... and so is a flag's value (`-k try` is a keyword expression).
        let (own, session) = split_args(v(&["-k", "try"]));
        assert_eq!(own, v(&["rstest"]));
        assert_eq!(session, v(&["-k", "try"]));
    }

    #[test]
    fn split_accepts_options_before_the_subcommand() {
        use clap::Parser;
        // A pytest switch before the subcommand still forwards to the session.
        let (own, session) = split_args(v(&["-q", "try", "-k", "foo"]));
        assert_eq!(own, v(&["rstest", "try"]));
        assert_eq!(session, v(&["-q", "-k", "foo"]));
        assert_eq!(Cli::parse_from(&own).command, Some(Command::Try));
        // rstest's own options (with values) before it stay owned; clap takes
        // parent options before the subcommand token.
        let (own, session) = split_args(v(&[
            "--python",
            "py3",
            "-n",
            "2",
            "--dist=load",
            "migrate-check",
            "--migrate-check-json",
            "o.json",
        ]));
        assert_eq!(
            own,
            v(&[
                "rstest",
                "--python",
                "py3",
                "-n",
                "2",
                "--dist=load",
                "migrate-check",
                "--migrate-check-json",
                "o.json"
            ])
        );
        assert!(session.is_empty(), "session={session:?}");
        let cli = Cli::parse_from(&own);
        assert_eq!(cli.command, Some(Command::MigrateCheck));
        assert_eq!(cli.python.as_deref(), Some("py3"));
        // A consume-all subcommand still takes everything after it.
        let (own, _) = split_args(v(&["-vv", "explain", "t.py::test_a"]));
        assert_eq!(own, v(&["rstest", "explain", "t.py::test_a"]));
    }

    #[test]
    fn docs_never_call_a_selected_subcommand_a_test_path() {
        // Regression: cli-commands.md, exit-codes.md and replay.md said a
        // subcommand is recognized only as the first argument, so
        // `rstest --python X try` would run `try` as a test path. The parser
        // selects it (see `subcommand_index`). Every documented
        // `rstest ... <subcommand>` example the parser treats as the
        // subcommand must not be described as a test path / plain run.
        let wrong = regex::Regex::new(
            r"(?i)test path|plain test run|normal test run|no longer recognized|only as the first argument",
        )
        .unwrap();
        let mut bad = Vec::new();
        for (page, text) in crate::doc_pages::pages() {
            for sentence in crate::doc_pages::sentences(&text) {
                let selects_late = crate::doc_pages::code_spans(&sentence)
                    .iter()
                    .filter_map(|span| span.strip_prefix("rstest "))
                    .any(|cmd| {
                        let argv: Vec<String> = cmd.split_whitespace().map(String::from).collect();
                        subcommand_index(&argv).is_some_and(|i| i > 0)
                    });
                if selects_late && wrong.is_match(&sentence) {
                    bad.push(format!("{page}: {sentence}"));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "docs describe a subcommand the parser selects as a test path:\n{}",
            bad.join("\n")
        );
        // The example the docs now give parses as documented.
        let (own, session) = split_args(v(&["--python", "X", "try"]));
        assert!(session.is_empty(), "session={session:?}");
        let cli = Cli::parse_from(&own);
        assert_eq!(cli.command, Some(Command::Try));
        assert_eq!(cli.python.as_deref(), Some("X"));
    }

    #[test]
    fn docs_never_mention_a_hidden_flag() {
        // Regression: concepts/monorepo.md listed the internal
        // `--instrument-workers` (hidden from --help) as a user-facing flag.
        // Hidden flags are plumbing between rstest processes, not interface.
        use clap::CommandFactory;
        fn hidden(cmd: &clap::Command, out: &mut Vec<String>) {
            out.extend(
                cmd.get_arguments()
                    .filter(|a| a.is_hide_set())
                    .filter_map(|a| a.get_long().map(|l| format!("--{l}"))),
            );
            for sub in cmd.get_subcommands() {
                hidden(sub, out);
            }
        }
        let mut flags = Vec::new();
        hidden(&Cli::command(), &mut flags);
        assert!(flags.contains(&"--instrument-workers".to_string()));
        let mut bad = Vec::new();
        for (page, text) in crate::doc_pages::pages() {
            for flag in &flags {
                let pat = format!(r"{}(?:[^a-z0-9-]|$)", regex::escape(flag));
                if regex::Regex::new(&pat).unwrap().is_match(&text) {
                    bad.push(format!("{page}: {flag}"));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "hidden flags documented:\n{}",
            bad.join("\n")
        );
    }

    #[test]
    fn every_visible_flag_is_documented() {
        // The reference pages were complete only by hand: a new flag could
        // ship with nothing in docs/reference/ to say it exists (replay's
        // --journal was only ever shown inside an example). Run flags belong
        // in cli.md; a subcommand's flags, including the global ones only
        // subcommands read, in cli-commands.md. Each is written as inline
        // code (`--flag`, `--flag=VALUE`, `-n, --flag`, ...).
        use clap::CommandFactory;
        fn visible(cmd: &clap::Command, global: bool) -> Vec<String> {
            cmd.get_arguments()
                .filter(|a| !a.is_hide_set() && a.is_global_set() == global)
                .flat_map(|a| {
                    let aliases = a.get_visible_aliases().unwrap_or_default();
                    a.get_long().into_iter().chain(aliases)
                })
                .map(|l| format!("--{l}"))
                .collect()
        }
        let page = |name: &str| {
            crate::doc_pages::pages()
                .into_iter()
                .find(|(p, _)| p == name)
                .unwrap_or_else(|| panic!("{name} not found"))
                .1
        };
        let documented = |text: &str, flag: &str| {
            let pat = format!(r"`(?:-[a-zA-Z], )?{}(?:[^a-z0-9-]|$)", regex::escape(flag));
            regex::Regex::new(&pat).unwrap().is_match(text)
        };
        let cli = Cli::command();
        let run_flags = visible(&cli, false);
        assert!(run_flags.contains(&"--doctor".to_string()));
        let (cli_md, commands_md) = (
            page("docs/reference/cli.md"),
            page("docs/reference/cli-commands.md"),
        );
        let mut missing: Vec<String> = run_flags
            .iter()
            .filter(|f| !documented(&cli_md, f))
            .map(|f| format!("docs/reference/cli.md: {f}"))
            .collect();
        for flag in visible(&cli, true) {
            if !documented(&commands_md, &flag) {
                missing.push(format!("docs/reference/cli-commands.md: {flag}"));
            }
        }
        for sub in cli.get_subcommands() {
            for flag in visible(sub, false) {
                if !documented(&commands_md, &flag) {
                    missing.push(format!(
                        "docs/reference/cli-commands.md: {} {flag}",
                        sub.get_name()
                    ));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "flags in --help with no reference entry:\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn misplaced_subcommand_is_a_clear_usage_error() {
        // Selected subcommands and flag values are not misplaced.
        assert_eq!(misplaced_subcommand(&v(&["-q", "try"])), None);
        assert_eq!(misplaced_subcommand(&v(&["try", "tests/"])), None);
        assert_eq!(misplaced_subcommand(&v(&["-k", "try"])), None);
        assert_eq!(misplaced_subcommand(&v(&["-x", "--", "a", "try"])), None);
        // After an option with a value, or a path: explain the order.
        let msg = misplaced_subcommand(&v(&["-k", "foo", "try"])).unwrap();
        assert!(msg.contains("subcommands must come first"), "{msg}");
        assert!(misplaced_subcommand(&v(&["tests/", "audit"])).is_some());
    }

    #[test]
    fn detached_auto_quarantine_value_is_a_usage_error() {
        let msg = detached_auto_quarantine(&v(&["--auto-quarantine", "0", "tests/"])).unwrap();
        assert!(msg.contains("`--auto-quarantine=0`"), "{msg}");
        for ok in [
            &["--auto-quarantine=0", "tests/"][..],
            &["--auto-quarantine", "tests/"],
            &["--", "--auto-quarantine", "0"],
            &["-k", "0"],
        ] {
            assert_eq!(detached_auto_quarantine(&v(ok)), None, "{ok:?}");
        }
    }

    #[test]
    fn leading_subcommand_shadowing_a_path_warns() {
        let dir = std::env::temp_dir().join(format!("rstest-shadow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(shadowed_path(&dir, &v(&["flaky"])), None);
        std::fs::create_dir(dir.join("flaky")).unwrap();
        let msg = shadowed_path(&dir, &v(&["flaky"])).unwrap();
        assert!(msg.contains("write `./flaky`"), "{msg}");
        // Not leading, or escaped: no subcommand selected, nothing shadowed.
        assert_eq!(shadowed_path(&dir, &v(&["-q", "flaky"])), None);
        assert_eq!(shadowed_path(&dir, &v(&["./flaky"])), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pytest_switch_clusters_are_value_less_only() {
        for s in ["-q", "-vv", "-xs", "--quiet", "--lf"] {
            assert!(is_pytest_switch(s), "{s}");
        }
        for s in ["-k", "-m", "-p", "-rA", "-", "--tb", "-qk"] {
            assert!(!is_pytest_switch(s), "{s}");
        }
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
    fn fork_pool_takes_an_optional_mode() {
        use crate::scheduling::warm::ForkPool;
        let parse = |args: &[&str]| {
            let (own, session) = split_args(v(args));
            (Cli::try_parse_from(own).map(|c| c.fork_pool), session)
        };
        assert_eq!(
            parse(&["--fork-pool", "tests/"]).0.unwrap(),
            Some(ForkPool::Core)
        );
        // The next token is a path, never the mode.
        assert_eq!(parse(&["--fork-pool", "tests/"]).1, v(&["tests/"]));
        assert_eq!(
            parse(&["--fork-pool=core"]).0.unwrap(),
            Some(ForkPool::Core)
        );
        assert_eq!(parse(&["--fork-pool=app"]).0.unwrap(), Some(ForkPool::App));
        assert_eq!(parse(&[]).0.unwrap(), None);
        assert!(parse(&["--fork-pool=bogus"]).0.is_err());
    }

    #[test]
    fn warn_on_dead_master_path_is_an_owned_flag() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (own, session) = split_args(v(&["--warn-on-dead-master-path", "tests/"]));
        assert_eq!(own, v(&["rstest", "--warn-on-dead-master-path"]));
        assert_eq!(session, v(&["tests/"]));
        assert!(Cli::parse_from(&own).warn_on_dead_master_path);
        assert!(!Cli::parse_from(["rstest"]).warn_on_dead_master_path);
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
