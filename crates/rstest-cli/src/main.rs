#[allow(dead_code)]
mod collect; // D5: single-point collection
#[allow(dead_code)]
mod config;
mod discover;
mod doctor;
mod migrate;
mod mono;
mod reporting;
mod scheduling;
mod select;
mod watch;

use reporting::{color, flakes, junit, progress, report, status};
use scheduling::{durations, lazy, pool, proto, shard, worker};

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
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
    numprocesses: Option<String>,

    /// Python interpreter to run workers with: a path, or a version request
    /// (`3.12`, `>=3.12,<3.13`, `pypy@3.10`, `3.13t`). Defaults to the active
    /// venv / a discovered `.venv` / `.python-version` / PATH.
    #[arg(long)]
    python: Option<String>,

    /// Write a per-test outcome snapshot (compat-harness recorder shape).
    #[arg(long)]
    report_json: Option<PathBuf>,

    /// Diagnose the suite after running: wait-bound tests, parallel
    /// floor, fixture hotspots, slowest files.
    #[arg(long)]
    doctor: bool,

    /// Write the doctor analysis as JSON (stable, versioned schema) for
    /// CI trending. Implies doctor instrumentation; combine with
    /// --doctor for the human report too.
    #[arg(long)]
    doctor_json: Option<PathBuf>,

    /// Write the doctor analysis as GitHub-flavored markdown (job-summary
    /// ready; implies doctor instrumentation). In CI a doctor run auto-publishes
    /// to the job summary; the flag is for a custom path or GitLab/TeamCity.
    #[arg(long)]
    doctor_md: Option<PathBuf>,

    /// Fail the run when a doctor metric breaches a threshold (repeatable),
    /// turning the advisory signal into a CI gate. Grammar `metric OP value`,
    /// e.g. `--doctor-fail-on 'parallel_efficiency<30'`. Implies instrumentation.
    #[arg(long = "doctor-fail-on", value_name = "COND")]
    doctor_fail_on: Vec<String>,

    /// Parallel-readiness preflight: collect twice and report tests with
    /// unstable ids, then run -n auto and classify any parallel-only failure
    /// (polluter bisected). Exits non-zero on any such finding.
    #[arg(long)]
    migrate_check: bool,

    /// Write the migrate-check findings as JSON (stable, versioned schema) for
    /// CI gating. Implies --migrate-check.
    #[arg(long)]
    migrate_check_json: Option<PathBuf>,

    /// Substring of a nodeid/site to accept as a known migrate-check finding
    /// (repeatable): it is still reported (marked "allowed") but does not fail
    /// the exit code, so CI can gate on NEW issues while tolerating known ones.
    #[arg(long = "migrate-allow")]
    migrate_allow: Vec<String>,

    /// Zero-config proof: run the suite under plain pytest and under rstest
    /// (-n auto), then report whether outcomes are identical and how much
    /// faster rstest is. The 30-second "should I switch?" answer.
    #[arg(long = "try")]
    r#try: bool,

    /// Distribution mode: "load" (dynamic, duration-aware), "loadfile",
    /// "loadscope", "loadgroup" (xdist_group marker affinity), or "each"
    /// (every test on every worker). [default: load]
    #[arg(long)]
    dist: Option<String>,

    /// Write merged results as junit XML (intercepted: per-worker sessions
    /// would clobber a shared file).
    #[arg(long)]
    junitxml: Option<PathBuf>,

    /// Watch the project and rerun on change: only-test-file changes rerun
    /// just those files; any other .py change reruns the tests that import
    /// the changed module (import-graph selection).
    #[arg(long)]
    watch: bool,

    /// Rerun failed tests up to N times; tests that then pass are
    /// reported flaky (run stays green). Crash-aware: a test that killed
    /// its worker gets retried on the replacement, within this budget.
    #[arg(long)]
    reruns: Option<u32>,

    /// Quarantine list: a file of nodeids or glob patterns (one per line,
    /// # comments). Matching failures are demoted to a non-fatal outcome
    /// (own section, flagged, never the exit code); others still fail.
    #[arg(long, value_name = "FILE")]
    quarantine: Option<PathBuf>,

    /// With reruns active, retry only failures whose error text matches
    /// this regex (repeatable). pytest-rerunfailures' --only-rerun.
    #[arg(long = "only-rerun", value_name = "REGEX")]
    only_rerun: Vec<String>,

    /// With reruns active, retry only tests that have a prior *flaky* history
    /// in `.rstest_cache/flakes.json` (passed-after-rerun on some earlier
    /// run). A first-time failure with no flaky history is reported failed
    /// without spending the budget — so a deterministic mass-failure (one
    /// cause failing many tests identically) no longer burns reruns for zero
    /// recovery. `@pytest.mark.flaky` tests are always retried (the marker is
    /// an explicit declaration). Composes with `--only-rerun` (both gates
    /// must pass).
    #[arg(long = "reruns-only-known-flaky")]
    reruns_only_known_flaky: bool,

    /// Kill a worker stuck on ONE test longer than this many seconds
    /// (hang backstop; the test is reported failed, the worker replaced).
    /// Off by default; catches what in-process timeouts can't (blocked C exts).
    #[arg(long, value_name = "SECS")]
    worker_timeout: Option<u64>,

    /// Run only tests affected by changed files (import-graph selection).
    /// Without a value: working tree + untracked vs HEAD. With a value:
    /// vs that git rev (e.g. --changed=origin/main in CI).
    #[arg(long, num_args = 0..=1, default_missing_value = "HEAD", value_name = "REV")]
    changed: Option<String>,

    /// Strict --changed for gating CI: an unconnectable changed source file
    /// forces a FULL run (no silent skip), and "nothing affected" exits 5
    /// instead of 0. Implies --changed (vs HEAD) when not given.
    #[arg(long)]
    changed_strict: bool,

    /// Collection strategy: "full" (every worker collects the whole suite,
    /// verified by hash) or "lazy" (each file collected by one worker on
    /// demand). Config `[tool.rstest] collect`. [default: full]
    #[arg(long, value_name = "MODE")]
    collect: Option<String>,

    /// Gate CI on per-test duration regressions: compare each test's wall time
    /// against the duration cache and exit non-zero when any test grew past
    /// RATIO x baseline (e.g. 2.0). Jitter-floored below 50ms / 0.5s growth.
    #[arg(long, value_name = "RATIO")]
    durations_regress: Option<f64>,

    /// Run tests in a seeded random order (pytest-randomly-style) to flush
    /// order dependencies. No value: per-run seed, printed; --shuffle=SEED
    /// reproduces. Parallel pool with full collection only.
    #[arg(long, num_args = 0..=1, default_missing_value = "random", value_name = "SEED")]
    shuffle: Option<String>,

    /// Terminal output style: "dots", "verbose" (like -v), or "bar"
    /// (pytest-sugar-style live progress). Config `[tool.rstest] output`.
    /// Default "bar" on a tty ("verbose" with -v), "dots" off-tty.
    #[arg(long, value_name = "STYLE")]
    output: Option<String>,

    /// Split the suite across N independent CI jobs and run only shard K
    /// (`--shard K/N`, K 1-based), balanced by the duration cache. Buckets are
    /// disjoint, so merging per-job JUnit reconstructs the full run.
    #[arg(long, value_name = "K/N")]
    shard: Option<String>,
}

/// -x / --maxfail=N from the session args (also forwarded: each worker
/// session stops itself; the orchestrator does the global coordination).
fn parse_maxfail(args: &[String]) -> Option<u64> {
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
fn parse_durations(args: &[String]) -> Option<(usize, f64)> {
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
fn needs_passthrough_io(session_args: &[String]) -> bool {
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

fn is_collect_only(session_args: &[String]) -> bool {
    session_args
        .iter()
        .any(|a| a == "--collect-only" || a == "--co")
}

/// Strip Windows `\\?\` verbatim prefix that `canonicalize` adds. Editor
/// URIs and path-prefix checks choke on it; no-op on non-Windows paths.
fn strip_verbatim(p: std::path::PathBuf) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        std::path::PathBuf::from(rest.to_string())
    } else {
        p
    }
}

/// Run a single collect-only session and write a structured discovery doc
/// (meta, tests, collect_errors). Bypasses passthrough so collection rides
/// the wire (RSTEST_SEND_IDS), not pytest's text tree. Returns exit status.
fn run_collect_discovery(
    python: &std::path::Path,
    args: &[String],
    out: &std::path::Path,
) -> Result<i32> {
    // Workers inherit the environment; this flips on the full id+location
    // payload from `pytest_collection_finish` (single session, so no
    // per-worker designate needed).
    std::env::set_var("RSTEST_SEND_IDS", "1");
    let mut w = worker::Worker::spawn_with_io(python, None, worker::Stdio::Null)?;
    // Item-dispatch session: its `pytest_collection_finish` emits the
    // id+location payload (runtestloop returns early on --collect-only). The
    // plain run_tests session has no collection_finish, so can't feed discovery.
    w.send(&proto::Command::RunItemsSession {
        args: args.to_vec(),
    })?;

    let mut ids: Vec<String> = Vec::new();
    let mut locations: Vec<(String, Option<u64>)> = Vec::new();
    let mut marks: Vec<Vec<String>> = Vec::new();
    let mut collect_errors: Vec<(String, String)> = Vec::new();
    let exitstatus = loop {
        match w.recv()? {
            proto::Event::CollectionDone {
                ids: i,
                locations: l,
                marks: m,
                ..
            } => {
                if let Some(i) = i {
                    ids = i;
                }
                if let Some(l) = l {
                    locations = l;
                }
                if let Some(m) = m {
                    marks = m;
                }
            }
            proto::Event::CollectError { path, longrepr } => collect_errors.push((path, longrepr)),
            proto::Event::Done { exitstatus } => break exitstatus,
            _ => {}
        }
    };
    w.shutdown()?;

    // Absolute rootdir so `file` resolves to an editor-usable URI.
    let cwd = std::env::current_dir()?;
    let rootdir = config::discover(&cwd).rootdir;
    let rootdir = if rootdir.is_absolute() {
        rootdir
    } else {
        cwd.join(rootdir)
    };
    let rootdir = strip_verbatim(std::fs::canonicalize(&rootdir).unwrap_or(rootdir));
    let tests: Vec<serde_json::Value> = ids
        .iter()
        .enumerate()
        .map(|(i, nodeid)| {
            let (file_rel, lineno) = locations.get(i).cloned().unwrap_or_default();
            // Absolute path for editor URIs; empty rel means pytest gave none.
            let file = if file_rel.is_empty() {
                String::new()
            } else {
                let rel = file_rel.strip_prefix("./").unwrap_or(&file_rel);
                rootdir.join(rel).to_string_lossy().into_owned()
            };
            // All pytest marker names on the item (own + inherited); empty
            // when the worker is older / sent none.
            let markers = marks.get(i).cloned().unwrap_or_default();
            serde_json::json!({
                "nodeid": nodeid,
                "file": file,
                "lineno": lineno,
                "markers": markers,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "meta": {
            "runner": "rstest",
            "kind": "discovery",
            "schema": 1,
            "count": ids.len(),
            "rootdir": rootdir.to_string_lossy(),
        },
        "tests": tests,
        "collect_errors": collect_errors
            .iter()
            .map(|(p, l)| serde_json::json!({"path": p, "longrepr": l}))
            .collect::<Vec<_>>(),
    });
    std::fs::write(out, serde_json::to_vec_pretty(&doc)?)?;
    Ok(exitstatus)
}

/// Split argv into rstest-owned args (fed to clap) and session args
/// (paths + pytest flags, forwarded verbatim).
fn split_argv() -> (Vec<String>, Vec<String>) {
    split_args(std::env::args().skip(1))
}

fn split_args(argv: impl IntoIterator<Item = String>) -> (Vec<String>, Vec<String>) {
    let mut own = vec!["rstest".to_string()];
    let mut session = Vec::new();
    let mut argv = argv.into_iter().peekable();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--doctor" | "--watch" | "--migrate-check" | "--try" => own.push(arg),
            "--reruns-only-known-flaky" => own.push(arg),
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

fn main() -> Result<()> {
    let (own_args, args) = split_argv();
    let cli = Cli::parse_from(&own_args);
    if cli.watch {
        return watch::watch_loop(&cli, &args);
    }
    let status = execute(&cli, &args)?;
    std::process::exit(status);
}

pub fn execute(cli: &Cli, args: &[String]) -> Result<i32> {
    let args = args.to_vec();
    let start = Instant::now();
    let started_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // One uid per test run, shared by every worker (xdist's testrun_uid
    // contract). Monorepo children inherit the root's: one run.
    if std::env::var_os("RSTEST_RUN_UID").is_none() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::set_var(
            "RSTEST_RUN_UID",
            format!("{nanos:x}{:x}", std::process::id()),
        );
    }
    // CLI > [tool.rstest] > built-in defaults.
    let settings = config::rstest_settings(&std::env::current_dir()?);

    // Monorepo: cwd has no pytest config of its own, subdirectories do.
    // Each subproject runs as its own session group (cwd switched, so
    // rootdir/ini/conftest match pytest-in-that-dir). Explicit paths stay single.
    if std::env::var_os("RSTEST_MONO_PROJECT").is_none() {
        let cwd = std::env::current_dir()?;
        let path_args = args
            .iter()
            .any(|a| !a.starts_with('-') && std::path::Path::new(a).exists());
        if !path_args && !config::has_pytest_config(&cwd) {
            let projects = mono::discover_projects(&cwd, settings.projects.as_deref());
            let threshold = if settings.projects.is_some() { 1 } else { 2 };
            if projects.len() >= threshold {
                return execute_monorepo(cli, &args, &cwd, projects);
            }
        }
    }
    let numprocesses = cli
        .numprocesses
        .clone()
        .or_else(|| settings.numprocesses.clone())
        .unwrap_or_else(|| "auto".into());
    let dist_name = cli
        .dist
        .clone()
        .or_else(|| settings.dist.clone())
        .unwrap_or_else(|| "load".into());
    // Validate once, up front: every run path (byte-exact, lazy, pool) shares
    // this name, so an invalid value must error the same way regardless of
    // suite size, not slip through the lazy/small-suite path silently.
    if !matches!(
        dist_name.as_str(),
        "load" | "loadfile" | "loadscope" | "loadgroup" | "each"
    ) {
        anyhow::bail!(
            "unknown --dist mode: {dist_name} (use load|loadfile|loadscope|loadgroup|each)"
        );
    }
    let reruns = cli.reruns.or(settings.reruns).unwrap_or(0);
    // Flaky-aware reruns: when on, load the prior flaky set ONCE so the pool
    // can gate rerun eligibility on it. None = feature off (no gating).
    // Gate on `reruns > 0` deliberately: the gate only ever suppresses the
    // global `--reruns` budget. @mark.flaky tests always bypass it (see the
    // pool gate), so a run whose only budget is @mark.flaky needs no set
    // loaded — loading one would change nothing.
    let known_flaky: Option<std::collections::HashSet<String>> = if reruns > 0
        && (cli.reruns_only_known_flaky || settings.reruns_only_known_flaky.unwrap_or(false))
    {
        Some(flakes::known_flaky())
    } else {
        None
    };
    let worker_timeout = cli.worker_timeout.or(settings.worker_timeout);
    let n = parse_numprocesses(&numprocesses)?;
    let passthrough = needs_passthrough_io(&args);
    // Honor `--reruns` in single-worker mode via a degenerate one-worker pool:
    // the rerun loop is orchestrator-side (rerunfailures neutralized inside).
    // Passthrough can't be pooled, so reruns stay inert there.
    let single_worker_reruns = reruns > 0 && n <= 1 && !passthrough;
    // A one-worker rerun pool is 1 worker everywhere downstream (banner,
    // doctor, report-json meta), never 0.
    let n = if single_worker_reruns { 1 } else { n };
    let palette = color::Palette::detect(&args);
    let verbose = args
        .iter()
        .any(|a| a == "--verbose" || (a.starts_with("-v") && a.chars().skip(1).all(|c| c == 'v')));
    // -vv (or more): pytest shows ALL durations, no hidden-cutoff note.
    let very_verbose = args.iter().filter(|a| *a == "--verbose").count() >= 2
        || args
            .iter()
            .any(|a| a.starts_with("-vv") && a.chars().skip(1).all(|c| c == 'v'));
    // Output style: --output > [tool.rstest] output > (-v ? verbose : tty ?
    // bar : dots). Auto-promote to the sugar bar on a tty, stay on plain dots
    // off-tty so logs stay byte-stable (the live footer self-disables there).
    let mode = match cli.output.as_deref().or(settings.output.as_deref()) {
        Some("bar") => progress::Mode::Bar,
        Some("verbose") => progress::Mode::Verbose,
        Some("dots") => progress::Mode::Dots,
        Some("github") => progress::Mode::Github,
        Some("json") => progress::Mode::Json,
        Some("tap") => progress::Mode::Tap,
        Some("teamcity") => progress::Mode::Teamcity,
        Some("gitlab") => progress::Mode::Gitlab,
        Some("buildkite") => progress::Mode::Buildkite,
        Some("azure") => progress::Mode::Azure,
        Some(other) => {
            eprintln!(
                "rstest: unknown --output '{other}' \
                 (use dots|verbose|bar|github|gitlab|buildkite|teamcity|azure|tap|json); using dots"
            );
            progress::Mode::Dots
        }
        None if verbose => progress::Mode::Verbose,
        None if std::io::stdout().is_terminal() => progress::Mode::Bar,
        None => progress::Mode::Dots,
    };
    let durations = parse_durations(&args);
    // Validate `--doctor-fail-on` conditions up front: a typo'd metric or a
    // missing operator aborts now, never silently as a gate that can't fire.
    let doctor_gate = doctor::parse_conditions(&cli.doctor_fail_on)?;
    if cli.doctor || cli.doctor_json.is_some() || cli.doctor_md.is_some() || !doctor_gate.is_empty()
    {
        // Workers inherit the environment; this flips on cpu/fixture
        // instrumentation in the shim plugin.
        std::env::set_var("RSTEST_DOCTOR", "1");
    }

    // Session args forward verbatim: the vendored core owns ini semantics
    // (python_files, testpaths, rootdir) and collection, so session
    // behavior is exactly pytest's.
    let scope = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let python = discover::resolve(&scope, cli.python.as_deref())?;
    // Zero-config "should I switch?" proof: pytest baseline vs rstest -n auto.
    if cli.r#try {
        return migrate::run_try(&python, &args);
    }
    // Parallel-readiness preflight: its own collect-twice path, not a run.
    if cli.migrate_check || cli.migrate_check_json.is_some() {
        return migrate::run_migrate_check(
            &python,
            &args,
            cli.migrate_check_json.as_deref(),
            &cli.migrate_allow,
        );
    }
    // `--collect-only --report-json <p>` writes a structured discovery doc
    // (nodeid + abs file + 0-based line + markers), the machine-readable
    // surface editors/CI consume. Own single-session path (NOT passthrough).
    if is_collect_only(&args) {
        if let Some(out) = &cli.report_json {
            let code = run_collect_discovery(&python, &args, out)?;
            std::process::exit(code);
        }
    }
    // Json/Tap modes keep stdout a pure machine stream: no banner
    // (TAP gets its version header instead).
    if !passthrough && mode == progress::Mode::Tap {
        println!("TAP version 13");
    }
    if !passthrough && mode != progress::Mode::Json && mode != progress::Mode::Tap {
        let worker_desc = if single_worker_reruns {
            "single worker (rerun pool; not byte-exact)".to_string()
        } else if n <= 1 {
            "single worker (pytest-exact mode)".to_string()
        } else {
            format!("{n} workers (parallel by default; -n 0 for single-worker mode)")
        };
        println!("rstest {} — {worker_desc}", env!("CARGO_PKG_VERSION"));
    }
    let mut args = args;
    let effective_changed = cli
        .changed
        .clone()
        .or_else(|| cli.changed_strict.then(|| "HEAD".to_string()))
        .map(|rev| select::resolve_base_rev(&rev))
        .transpose()?;
    if let Some(rev) = &effective_changed {
        let rev = if rev == "HEAD" {
            None
        } else {
            Some(rev.as_str())
        };
        let cwd = std::env::current_dir()?;
        let project = config::discover(&cwd);
        // Coverage-aware selection: uses the line->test index when it is warm
        // (any --cov-context=test run writes it), else falls back per-file to
        // import-graph reachability, so --changed only ever gets tighter.
        let changes = select::changed_line_ranges(rev)?;
        match select::affected_with_coverage(
            &project.rootdir,
            &project,
            &changes,
            cli.changed_strict,
            rev,
        )? {
            select::Selection::FullRun(reason) => {
                eprintln!("rstest: --changed falling back to full run ({reason})");
            }
            select::Selection::Tests(tests) if tests.is_empty() => {
                println!(
                    "rstest: no tests affected by {} changed file(s)",
                    changes.len()
                );
                // Strict gating needs to DISTINGUISH "ran nothing" from
                // "everything passed": pytest's nothing-collected code.
                std::process::exit(if cli.changed_strict { 5 } else { 0 });
            }
            select::Selection::Tests(tests) => {
                eprintln!(
                    "rstest: {} changed file(s) -> {} affected test target(s)",
                    changes.len(),
                    tests.len()
                );
                let mut selected: Vec<String> =
                    tests.iter().map(|t| t.display().to_string()).collect();
                // Keep the user's flags; drop any explicit path args in
                // favor of the selection.
                selected.extend(
                    args.iter()
                        .filter(|a| a.starts_with('-') || !std::path::Path::new(a).exists())
                        .cloned(),
                );
                args = selected;
            }
        }
    }
    if reruns > 0 && passthrough {
        eprintln!(
            "rstest: --reruns is ignored under -s/--pdb/--co \
             (interactive single session); drop those flags to enable reruns"
        );
    }
    if single_worker_reruns {
        // Not silent: a byte-exact run is now a one-worker pool (dispatch
        // order, gw0 id, rerunfailures neutralized). Say so on stderr so log
        // scrapers and existing configs see the switch, not just the banner.
        eprintln!(
            "rstest: --reruns at -n {numprocesses} runs a one-worker rerun pool \
             (not byte-exact); use -n 0/1 without --reruns for the byte-exact session"
        );
    }
    // --shuffle reorders the orchestrator's dispatch queue, so it needs
    // the full-collection pool. Refusing (not ignoring) matters: a user
    // probing for order dependence must not get a silently ordered run.
    let shuffle_seed: Option<u64> = match cli.shuffle.as_deref() {
        None => None,
        Some(v) => {
            if n <= 1 || passthrough {
                if single_worker_reruns {
                    anyhow::bail!(
                        "--shuffle is not supported by the one-worker rerun pool \
                         (--reruns at -n <= 1); raise -n to 2+ to combine shuffle \
                         with reruns"
                    );
                }
                anyhow::bail!(
                    "--shuffle needs the parallel pool (-n >= 2); in single-worker \
                     mode the session owns its own order (use pytest-randomly there)"
                );
            }
            if collect_lazy(cli, &settings, &dist_name, &args)? {
                anyhow::bail!("--shuffle is not supported with --collect lazy");
            }
            if dist_name == "each" {
                anyhow::bail!(
                    "--shuffle is not supported with --dist each (workers run the \
                     full suite in session order)"
                );
            }
            let seed = if v == "random" {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0)
                    ^ u64::from(std::process::id())
            } else {
                v.parse().map_err(|_| {
                    anyhow::anyhow!("--shuffle seed must be an unsigned integer, got '{v}'")
                })?
            };
            eprintln!("rstest: shuffle seed {seed} (reproduce with --shuffle={seed})");
            Some(seed)
        }
    };
    // --shard K/N: partition the suite and keep bucket K. Purely an
    // orchestrator-side node-id (or, in lazy mode, file) filter.
    let shard: Option<(usize, usize)> = match cli.shard.as_deref() {
        None => None,
        Some(spec) => {
            let (k, total) = shard::parse_shard(spec)?;
            if total == 1 {
                None // 1/1 is the whole suite: no-op.
            } else {
                if n <= 1 || passthrough {
                    if single_worker_reruns {
                        anyhow::bail!(
                            "--shard is not supported by the one-worker rerun pool \
                             (--reruns at -n <= 1); raise -n to 2+ to combine shard \
                             with reruns"
                        );
                    }
                    anyhow::bail!(
                        "--shard needs the parallel pool (-n >= 2); the single-worker \
                         path runs the session's own full suite with no dispatch filter"
                    );
                }
                if shuffle_seed.is_some() {
                    anyhow::bail!(
                        "--shard is not supported with --shuffle: shards must partition \
                         the suite identically on every machine, which a per-run shuffle \
                         defeats (shuffle within a shard is fine to add later)"
                    );
                }
                if dist_name == "each" {
                    anyhow::bail!(
                        "--shard is not supported with --dist each (every worker runs the \
                         full suite; there is no dispatch queue to partition)"
                    );
                }
                Some((k, total))
            }
        }
    };
    let mut outcome = if passthrough || (n <= 1 && !single_worker_reruns) {
        let io = if passthrough {
            worker::Stdio::Inherit
        } else {
            worker::Stdio::Null
        };
        let mut w = worker::Worker::spawn_with_io(&python, None, io)?;
        w.send(&proto::Command::RunTests { args: args.clone() })?;
        let mut run = report::Run::default();
        run.track_phase_durations = durations.is_some();
        let mut prog = progress::Progress::default();
        prog.set_palette(palette);
        prog.set_mode(mode);
        let mut fixtures: Vec<proto::FixtureStat> = Vec::new();
        let mut warnings: Vec<proto::WarningEntry> = Vec::new();
        let exitstatus = loop {
            match w.recv()? {
                proto::Event::Report(r) => {
                    if !passthrough {
                        prog.on_report(None, &r);
                    }
                    run.record(None, r);
                }
                proto::Event::CollectError { path, longrepr } => run.collect_error(path, longrepr),
                proto::Event::CollectSkip { .. } => run.collect_skips += 1,
                proto::Event::DoctorFixtures { fixtures: fx } => fixtures.extend(fx),
                proto::Event::Warnings { entries } => warnings.extend(entries),
                proto::Event::CollectionDone { .. }
                | proto::Event::NodeInput { .. }
                | proto::Event::ItemStart { .. }
                | proto::Event::ItemDone { .. }
                | proto::Event::Stopped { .. }
                | proto::Event::LazyReady { .. }
                | proto::Event::FileCollected { .. }
                | proto::Event::ItemStartId { .. }
                | proto::Event::ItemDoneId { .. }
                | proto::Event::StoppedIds { .. } => {}
                proto::Event::Done { exitstatus } => break exitstatus,
            }
        };
        w.shutdown()?;
        pool::PoolOutcome {
            run,
            prog,
            fixtures,
            warnings,
            cache_dir: None,
            exitstatus,
        }
    } else if collect_lazy(cli, &settings, &dist_name, &args)? {
        let cwd = std::env::current_dir()?;
        let project = config::discover(&cwd);
        let paths: Vec<PathBuf> = args
            .iter()
            .filter(|a| !a.starts_with('-') && std::path::Path::new(a).exists())
            .map(PathBuf::from)
            .collect();
        let mut files = collect::collect_test_files(&paths, &project)?;
        if let Some((k, total)) = shard {
            let before = files.len();
            files = shard::shard_files(&files, &durations::load(), &cwd, k, total);
            eprintln!(
                "rstest: shard {k}/{total} -> {} of {before} test file(s)",
                files.len()
            );
        }
        lazy::run_lazy_pool(
            &python,
            n.min(files.len().max(1)),
            &args,
            files,
            mode,
            palette,
            // Steal (split files across workers) only on an EXPLICIT --dist
            // load: lazy defaults to strict file affinity, since stealing
            // exposes cross-file/in-file order dependence affinity doesn't.
            cli.dist.as_deref() == Some("load") || settings.dist.as_deref() == Some("load"),
            parse_maxfail(&args),
            reruns,
            &cli.only_rerun
                .iter()
                .map(|p| regex::Regex::new(p))
                .collect::<Result<Vec<_>, _>>()?,
            worker_timeout.map(std::time::Duration::from_secs),
            known_flaky.as_ref(),
        )?
    } else {
        let dist = match dist_name.as_str() {
            "load" => pool::Dist::Load,
            "loadfile" => pool::Dist::Loadfile,
            "loadscope" => pool::Dist::Loadscope,
            "loadgroup" => pool::Dist::Loadgroup,
            "each" => pool::Dist::Each,
            other => {
                anyhow::bail!(
                    "unknown --dist mode: {other} (use load|loadfile|loadscope|loadgroup|each)"
                )
            }
        };
        if dist == pool::Dist::Each && reruns > 0 {
            anyhow::bail!(
                "--reruns is not supported with --dist each (every worker runs the \
                 full suite; rerun-on-another-worker semantics do not apply)"
            );
        }
        pool::run_pool(
            &python,
            n,
            &args,
            mode,
            durations.is_some(),
            palette,
            dist,
            parse_maxfail(&args),
            reruns,
            &cli.only_rerun
                .iter()
                .map(|p| regex::Regex::new(p))
                .collect::<Result<Vec<_>, _>>()?,
            worker_timeout.map(std::time::Duration::from_secs),
            shuffle_seed,
            shard,
            known_flaky.as_ref(),
        )?
    };

    // Quarantine BEFORE any output or exit-code consumer: classification,
    // counts, junit, report-json, and the sessionfinish envelope must all
    // see the demoted outcomes consistently.
    if let Some(qpath) = &cli.quarantine {
        if passthrough {
            eprintln!("rstest: --quarantine has no effect in passthrough mode; ignoring");
        } else {
            let matcher = quarantine_matcher(qpath)?;
            let demoted = outcome.run.quarantine(|id| matcher.is_match(id));
            // pytest exit 1 = tests failed; if every failure was
            // quarantined the run is green by policy. Exit codes 2+
            // (usage/internal errors) are never touched.
            if !demoted.is_empty() && outcome.exitstatus == 1 && outcome.run.all_passed() {
                outcome.exitstatus = 0;
            }
        }
    }
    // Loaded before this run's events are recorded below, so the history
    // annotations say "before this run".
    let flake_history = flakes::load();

    if !passthrough && mode == progress::Mode::Json {
        // Pure NDJSON: close the stream with a session-finish envelope
        // (counts + duration + exit status). No human summary/failures.
        outcome.prog.finish();
        let envelope = serde_json::json!({
            "event": "sessionfinish",
            "exitstatus": outcome.exitstatus,
            "duration": (start.elapsed().as_secs_f64() * 100.0).round() / 100.0,
            "counts": outcome.run.counts(),
        });
        println!("{envelope}");
    } else if !passthrough && mode == progress::Mode::Tap {
        // Pure TAP: close the stream with the trailing plan. Failure text
        // already rode along as `#` diagnostics; no human summary.
        outcome.prog.finish();
        outcome.prog.tap_plan();
    } else if !passthrough {
        outcome.prog.finish();
        let wrap = match mode {
            progress::Mode::Gitlab => report::FailureWrap::GitlabSection,
            progress::Mode::Buildkite => report::FailureWrap::BuildkiteGroup,
            _ => report::FailureWrap::Plain,
        };
        // Bar mode already inlines each failure as it happens; re-printing
        // the batched block would duplicate it.
        if mode != progress::Mode::Bar {
            outcome.run.print_failures(&palette, wrap);
        }
        outcome.run.print_quarantined(&palette, &flake_history);
        outcome.run.print_flaky(&palette, &flake_history, wrap);
        print_warnings_summary(&outcome.warnings, &palette);
        if let Some((dn, dmin)) = durations {
            outcome
                .run
                .print_durations(dn, dmin, very_verbose, &palette);
        }
        let warn_total: u64 = outcome.warnings.iter().map(|w| w.count).sum();
        let warn_part = if warn_total > 0 {
            format!(", {warn_total} warnings")
        } else {
            String::new()
        };
        let elapsed = start.elapsed().as_secs_f64();
        // Bar mode closes with pytest-sugar's segmented results bar above
        // the stable summary line (which tooling/CI greps, so keep it intact).
        // The bar gives its own visual break; other modes get a blank line.
        if mode == progress::Mode::Bar && std::io::stdout().is_terminal() {
            let c = outcome.run.counts();
            let g = c["passed"];
            let r = c["failed"] + c["errors"] + c["collect_errors"];
            let y = c["skipped"] + c["xfailed"] + c["xpassed"];
            let n = g + r + y;
            println!(
                "\nResults ({elapsed:.2}s):\n  {} {n}/{n}",
                status::summary_bar(g as usize, r as usize, y as usize, &palette),
            );
        } else {
            println!();
        }
        let summary = format!("{}{warn_part} in {elapsed:.2}s", outcome.run.summary_line());
        let summary = if outcome.run.all_passed() {
            palette.green(&summary)
        } else {
            palette.red(&summary)
        };
        println!("{summary}");
        // CI-native surfaces emitted from the aggregate at end-of-run. Failures
        // already rode along above, so these add each platform's flake signal
        // (GitHub/Azure annotations here; TeamCity as live service messages).
        match mode {
            progress::Mode::Github => print_github_annotations(&outcome.run),
            progress::Mode::Azure => print_azure_annotations(&outcome.run),
            progress::Mode::Buildkite => buildkite_flaky_annotate(&outcome.run),
            progress::Mode::Teamcity => {
                let msgs = progress::teamcity_flaky_messages(&outcome.run.flaky);
                if !msgs.is_empty() {
                    println!("{msgs}");
                }
            }
            _ => {}
        }
    }

    // A passthrough-IO run (-s/--pdb/--co) skips doctor instrumentation, so the
    // gate can't evaluate; say so instead of a silent false green.
    if !doctor_gate.is_empty() && passthrough {
        eprintln!(
            "rstest: --doctor-fail-on is ignored under -s/--pdb/--co \
             (no doctor instrumentation in an interactive single session)"
        );
    }
    let mut doctor_gate_failed = false;
    if (cli.doctor
        || cli.doctor_json.is_some()
        || cli.doctor_md.is_some()
        || !doctor_gate.is_empty())
        && !passthrough
    {
        let report = doctor::analyze(
            &outcome.run,
            &merge_fixtures(outcome.fixtures),
            start.elapsed().as_secs_f64(),
            n,
        );
        // In json mode stdout is a pure NDJSON stream, so the doctor's human
        // report would corrupt it; --doctor-json still writes to its file.
        if cli.doctor && mode != progress::Mode::Json {
            doctor::render(&report);
        }
        if let Some(path) = &cli.doctor_json {
            doctor::write_json(path, &report)?;
        }
        if let Some(path) = &cli.doctor_md {
            doctor::write_markdown(path, &report)?;
        }
        doctor::append_ci_summary(&report)?;
        if !doctor_gate.is_empty() {
            let gate = doctor::evaluate(&report, &doctor_gate);
            for s in &gate.skipped {
                eprintln!("rstest: --doctor-fail-on: {s}");
            }
            if gate.breaches.is_empty() {
                eprintln!(
                    "rstest: --doctor-fail-on: all {} condition(s) passed",
                    doctor_gate.len()
                );
            } else {
                // stderr, not stdout: --output json/tap keep stdout a pure
                // machine stream, and the failure block must not corrupt it
                // (same reason the human doctor render is gated above).
                eprintln!(
                    "\n{}",
                    palette.bold_red("=========== doctor gate failures ===========")
                );
                for b in &gate.breaches {
                    eprintln!("  {b}");
                }
                doctor_gate_failed = true;
            }
        }
    }
    if let Some(path) = &cli.junitxml {
        junit::write(path, &outcome.run, start.elapsed().as_secs_f64())?;
    }
    // Merged lastfailed: workers' own writes are blocked in pool mode
    // (each knows only its failures); write the union into pytest's cache
    // so a follow-up `--lf` behaves exactly as after a serial run.
    if let Some(cache_dir) = &outcome.cache_dir {
        // Each mode keys outcomes "nodeid [gwN]"; lastfailed needs the
        // plain nodeids (deduped, since a test may fail on several workers).
        let failed: std::collections::BTreeMap<String, bool> = outcome
            .run
            .failed_nodeids()
            .map(|id| {
                let plain = id.rsplit_once(" [gw").map(|(p, _)| p).unwrap_or(id);
                (plain.to_string(), true)
            })
            .collect();
        let dir = std::path::Path::new(cache_dir).join("v/cache");
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = std::fs::write(
                dir.join("lastfailed"),
                serde_json::to_vec(&failed).unwrap_or_default(),
            );
        }
    }
    // Duration regression gate: must compare BEFORE durations::save
    // overwrites the baseline with this run's times.
    let mut duration_regressions = 0usize;
    if let Some(ratio) = cli.durations_regress {
        if ratio <= 1.0 {
            anyhow::bail!("--durations-regress ratio must be > 1.0, got {ratio}");
        }
        let baseline = durations::load();
        if baseline.is_empty() {
            eprintln!(
                "rstest: --durations-regress: no duration baseline yet \
                 (.rstest_cache/durations.json); comparison skipped"
            );
        } else {
            let rows = durations::regressions(&outcome.run, &baseline, ratio);
            if rows.is_empty() {
                eprintln!("rstest: --durations-regress: no regressions (>= {ratio}x baseline)");
            } else {
                println!(
                    "\n{}",
                    palette.bold_red(&format!(
                        "=========== duration regressions (>= {ratio}x baseline) ==========="
                    ))
                );
                for (nodeid, old, new) in &rows {
                    println!("  {old:7.2}s -> {new:7.2}s  {nodeid}");
                }
                duration_regressions = rows.len();
            }
        }
    }
    // Each-mode ids carry the [gwN] suffix and every test ran N times, so
    // they would poison the duration cache used for LPT scheduling.
    if dist_name != "each" {
        durations::save(&outcome.run);
        // Flake history rides the same cadence (and the same [gwN]-key
        // poisoning concern rules out each-mode).
        flakes::record(&outcome.run);
    }
    if let Some(path) = &cli.report_json {
        outcome.run.write_snapshot(
            path,
            &report::RunMeta {
                exitstatus: outcome.exitstatus,
                duration_seconds: start.elapsed().as_secs_f64(),
                started_at_epoch: started_epoch,
                workers: n,
                argv: std::env::args().collect(),
            },
        )?;
    }

    // Coverage: workers save suffixed data files (pytest-cov worker mode);
    // the orchestrator plays the xdist-master role, so combine and report.
    let mut exitstatus = outcome.exitstatus;
    if !passthrough && args.iter().any(|a| a == "--cov" || a.starts_with("--cov=")) {
        println!();
        let status = std::process::Command::new(&python)
            .args(["-m", "rstest_worker.covtool"])
            .args(&args)
            .env("PYTHONPATH", worker::worker_pythonpath())
            .status();
        match status {
            Ok(s) if !s.success() && exitstatus == 0 => exitstatus = 1,
            Ok(_) => {}
            Err(e) => eprintln!("rstest: coverage reporting failed to run: {e}"),
        }
    }
    if duration_regressions > 0 {
        eprintln!(
            "rstest: {duration_regressions} duration regression{} vs baseline (--durations-regress)",
            if duration_regressions > 1 { "s" } else { "" }
        );
        if exitstatus == 0 {
            exitstatus = 1;
        }
    }
    if doctor_gate_failed {
        eprintln!("rstest: --doctor-fail-on: threshold breach (see doctor gate failures above)");
        if exitstatus == 0 {
            exitstatus = 1;
        }
    }
    Ok(exitstatus)
}

/// Sequential session groups, one per subproject (monorepo P0).
fn execute_monorepo(
    cli: &Cli,
    args: &[String],
    root: &std::path::Path,
    projects: Vec<PathBuf>,
) -> Result<i32> {
    if needs_passthrough_io(args) {
        anyhow::bail!(
            "--pdb/-s/--co need a single pytest session; run inside one project \
             of this monorepo (for --collect-only --report-json discovery, run \
             it once per project)"
        );
    }
    if cli.watch {
        anyhow::bail!("--watch at a monorepo root is not supported yet; run inside a project");
    }
    // Validate --doctor-fail-on once here so a malformed condition fails fast
    // at the root, not as N separate child aborts (children re-validate too).
    doctor::parse_conditions(&cli.doctor_fail_on)?;
    // The monorepo orchestrator prints per-project banners and a summary
    // around captured child output, so a clean NDJSON stream isn't possible.
    // The merged --report-json document is the machine-readable surface.
    if cli.output.as_deref() == Some("json") {
        anyhow::bail!(
            "--output json streams live per-session results and can't be merged \
             across a monorepo's projects; use --report-json <path> for one merged \
             machine-readable document, or run --output json inside a single project"
        );
    }
    // Same problem for TAP: each child would emit its own version header,
    // numbering, and plan; concatenated, that is not one valid stream.
    if cli.output.as_deref() == Some("tap") {
        anyhow::bail!(
            "--output tap can't be merged across a monorepo's projects; use \
             --junitxml for per-project machine-readable results, or run \
             --output tap inside a single project"
        );
    }
    let rels: Vec<String> = projects
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                // Stable, OS-independent project keys: forward slashes on
                // Windows too, so summary/meta/merged-report keys match the
                // `libs/b` form the gate and report contract expect.
                .replace('\\', "/")
        })
        .collect();
    // Worker budget: the user's -n (or auto = cores), split across projects
    // by their last-known suite time (duration caches). Each project runs as
    // a CHILD rstest process (cwd-isolated, output captured, printed whole).
    let budget = parse_numprocesses(
        &cli.numprocesses
            .clone()
            .unwrap_or_else(|| "auto".to_string()),
    )
    .unwrap_or(4)
    .max(1);
    // --changed at a monorepo root: classify projects ONCE against the
    // repo-wide changed set. Directly-changed projects keep --changed;
    // dependents run their FULL suite; the rest are skipped.
    let mono_changed = cli
        .changed
        .clone()
        .or_else(|| cli.changed_strict.then(|| "HEAD".to_string()))
        .map(|rev| select::resolve_base_rev(&rev))
        .transpose()?;
    let impacts: Option<Vec<mono::ChangeImpact>> = match &mono_changed {
        Some(rev) => {
            let rev = if rev == "HEAD" {
                None
            } else {
                Some(rev.as_str())
            };
            let changed = select::changed_files_from_git(rev)?;
            let impacts = mono::classify_changes(root, &projects, &changed, cli.changed_strict);
            let skipped = impacts
                .iter()
                .filter(|i| **i == mono::ChangeImpact::Unaffected)
                .count();
            eprintln!(
                "rstest: --changed: {} changed file(s) -> {} of {} projects affected",
                changed.len(),
                projects.len() - skipped,
                projects.len()
            );
            Some(impacts)
        }
        None => None,
    };
    let costs: Vec<Option<f64>> = projects.iter().map(|p| mono::project_cost(p)).collect();
    // A project pinning its own numprocesses (e.g. 0 for an
    // order-sensitive suite that needs pytest-exact mode) keeps it.
    let fixed: Vec<Option<usize>> = projects.iter().map(|p| mono::project_fixed_n(p)).collect();
    let shares = mono::plan_shares_with_fixed(&costs, &fixed, budget);
    println!(
        "rstest {} — monorepo: {} projects, {budget} workers ({})",
        env!("CARGO_PKG_VERSION"),
        projects.len(),
        rels.iter()
            .zip(&shares)
            .map(|(r, s)| format!("{r}:-n{s}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let start = Instant::now();
    let exe = std::env::current_exe()?;

    // Launch every project concurrently; the shares cap total worker
    // count at the budget. Output prints in COMPLETION order.
    let (tx, rx) = std::sync::mpsc::channel::<(usize, String, i32)>();
    let mut launched = 0usize;
    let mut skipped_projects: Vec<usize> = Vec::new();
    for (i, (project, rel)) in projects.iter().zip(&rels).enumerate() {
        let impact = impacts
            .as_ref()
            .map(|v| v[i])
            .unwrap_or(mono::ChangeImpact::Direct);
        if impact == mono::ChangeImpact::Unaffected {
            skipped_projects.push(i);
            continue;
        }
        let slug = mono::slug(root, project);
        let mut cmd = std::process::Command::new(&exe);
        cmd.current_dir(project)
            .env("RSTEST_MONO_PROJECT", rel)
            .arg("-n")
            .arg(shares[i].to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Own flags that must travel to the child; output paths get the
        // project slug and anchor at the INVOCATION root.
        match (&cli.python, mono::project_python(project)) {
            (Some(p), _) => {
                cmd.arg("--python").arg(p);
            }
            // A project-local venv beats the inherited environment.
            (None, Some(p)) => {
                cmd.arg("--python").arg(p);
            }
            (None, None) => {}
        }
        if let Some(d) = &cli.dist {
            cmd.arg("--dist").arg(d);
        }
        // Per-project output style. Children write to a captured pipe (not a
        // tty), so bar/verbose render per-test lines and github emits its
        // `::error` annotations, all reprinted under the project header.
        if let Some(o) = &cli.output {
            cmd.arg("--output").arg(o);
        }
        if let Some(r) = &cli.reruns {
            cmd.arg("--reruns").arg(r.to_string());
        }
        if let Some(q) = &cli.quarantine {
            // Children run with cwd=project, so hand them an absolute path.
            // Patterns match each child's project-relative nodeids.
            cmd.arg("--quarantine")
                .arg(std::fs::canonicalize(q).unwrap_or_else(|_| q.clone()));
        }
        for pat in &cli.only_rerun {
            cmd.arg("--only-rerun").arg(pat);
        }
        if let Some(t) = &cli.worker_timeout {
            cmd.arg("--worker-timeout").arg(t.to_string());
        }
        if cli.doctor {
            cmd.arg("--doctor");
        }
        if let Some(rev) = &mono_changed {
            // Only directly-changed projects narrow further; a dependent
            // runs full (its own files didn't change).
            if impact == mono::ChangeImpact::Direct {
                cmd.arg(format!("--changed={rev}"));
                if cli.changed_strict {
                    cmd.arg("--changed-strict");
                }
            }
        }
        if let Some(p) = &cli.junitxml {
            cmd.arg("--junitxml")
                .arg(root.join(mono::suffixed(p, &slug)));
        }
        if cli.report_json.is_some() {
            // Children write to temp parts; the orchestrator merges them
            // into ONE document at the requested path after the run.
            cmd.arg("--report-json").arg(report_part_path(&slug));
        }
        if let Some(p) = &cli.doctor_json {
            cmd.arg("--doctor-json")
                .arg(root.join(mono::suffixed(p, &slug)));
        }
        if let Some(p) = &cli.doctor_md {
            cmd.arg("--doctor-md")
                .arg(root.join(mono::suffixed(p, &slug)));
        }
        // Each project gates its own doctor report; a breach fails that child's
        // exit code, which the orchestrator aggregates.
        for c in &cli.doctor_fail_on {
            cmd.arg("--doctor-fail-on").arg(c);
        }
        cmd.args(args);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("rstest: failed to launch project {rel}: {e}");
                let _ = tx.send((i, format!("launch failed: {e}\n"), 3));
                continue;
            }
        };
        launched += 1;
        let tx = tx.clone();
        std::thread::spawn(move || {
            let (out, status) = match child.wait_with_output() {
                Ok(o) => {
                    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                    text.push_str(&String::from_utf8_lossy(&o.stderr));
                    (text, o.status.code().unwrap_or(3))
                }
                Err(e) => (format!("wait failed: {e}\n"), 3),
            };
            let _ = tx.send((i, out, status));
        });
    }
    drop(tx);

    let mut results: Vec<Option<i32>> = vec![None; projects.len()];
    for (i, output, status) in rx {
        println!("\n=============== project: {} ===============", rels[i]);
        print!("{output}");
        results[i] = Some(status);
    }
    let _ = launched;

    println!("\n=============== monorepo summary ===============");
    let mut report_parts: Vec<(String, Option<PathBuf>, Option<i32>, bool)> = Vec::new();
    let mut statuses = Vec::new();
    for (i, (rel, status)) in rels.iter().zip(&results).enumerate() {
        if skipped_projects.contains(&i) {
            println!("  {rel:<40} skipped (no changes)");
            report_parts.push((rel.clone(), None, None, true));
            continue;
        }
        let status = status.unwrap_or(3);
        let slug = mono::slug(root, &projects[i]);
        report_parts.push((
            rel.clone(),
            Some(report_part_path(&slug)),
            Some(status),
            false,
        ));
        statuses.push(status);
        let verdict = match status {
            0 => "ok".to_string(),
            5 => "no tests".to_string(),
            s => format!("FAILED (exit {s})"),
        };
        println!("  {rel:<40} {verdict}");
    }
    if statuses.is_empty() {
        println!("no projects affected by the change set");
        // Strict gating distinguishes "ran nothing" from "all passed".
        statuses.push(if cli.changed_strict { 5 } else { 0 });
    }
    let merged = pool::merge_statuses(&statuses);
    if let Some(out) = &cli.report_json {
        let out = if out.is_absolute() {
            out.clone()
        } else {
            root.join(out)
        };
        let run_meta = report::RunMeta {
            exitstatus: merged,
            duration_seconds: start.elapsed().as_secs_f64(),
            started_at_epoch: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().saturating_sub(start.elapsed().as_secs()))
                .unwrap_or(0),
            workers: budget,
            argv: std::env::args().collect(),
        };
        if let Err(e) = mono::merge_reports(&report_parts, &run_meta, &out) {
            eprintln!("rstest: failed to write merged report: {e}");
        }
        for (_, part, _, _) in &report_parts {
            if let Some(p) = part {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    println!(
        "{} projects in {:.2}s (exit {merged})",
        statuses.len(),
        start.elapsed().as_secs_f64()
    );
    Ok(merged)
}

/// Temp location for one project's report part during a monorepo run.
fn report_part_path(slug: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "rstest-mono-{}-{slug}.json",
        std::env::var("RSTEST_RUN_UID").unwrap_or_default()
    ))
}

/// Resolve the collection strategy (CLI > [tool.rstest] > "full") and
/// validate lazy-mode constraints.
fn collect_lazy(
    cli: &Cli,
    settings: &config::RstestSettings,
    dist_name: &str,
    args: &[String],
) -> Result<bool> {
    let mode = cli
        .collect
        .clone()
        .or_else(|| settings.collect.clone())
        .unwrap_or_else(|| "full".into());
    match mode.as_str() {
        "full" => Ok(false),
        "lazy" => {
            if !matches!(dist_name, "load" | "loadfile") {
                anyhow::bail!(
                    "--collect lazy is file-affine and cannot honor --dist {dist_name} \
                     (loadscope/loadgroup need a global id list; use --collect full)"
                );
            }
            // Single-test selection by nodeid wants exact-item dispatch;
            // --pyargs selects by import path, which the file walk can't
            // see. Both fall back to full collection.
            if args.iter().any(|a| a.contains("::") || a == "--pyargs") {
                eprintln!(
                    "rstest: nodeid/--pyargs arguments given; --collect lazy falls back \
                     to full collection"
                );
                return Ok(false);
            }
            Ok(true)
        }
        other => anyhow::bail!("unknown --collect mode: {other} (use full|lazy)"),
    }
}

fn parse_numprocesses(value: &str) -> Result<usize> {
    if value == "auto" {
        return Ok(auto_workers());
    }
    Ok(value.parse()?)
}

/// `auto` = logical cores, capped by what the suite can use (worker startup
/// costs real time). Two best-effort signals: test-file count from an
/// ini-aware walk, and the duration cache (a few-second suite needs ~2 workers).
fn auto_workers() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4);
    let mut n = cores;

    if let Ok(cwd) = std::env::current_dir() {
        let project = config::discover(&cwd);
        if let Ok(files) = collect::collect_test_files(&[], &project) {
            if !files.is_empty() {
                n = n.min(files.len());
            }
        }
    }

    let cache = durations::load();
    if !cache.is_empty() {
        let total: f64 = cache.values().sum();
        // ~2s of test time per worker is plenty to amortize startup.
        let by_time = (total / 2.0).ceil() as usize;
        n = n.min(by_time.max(1));
    }

    n.max(1)
}

/// Sum identical fixtures reported by multiple workers.
fn merge_fixtures(all: Vec<proto::FixtureStat>) -> Vec<proto::FixtureStat> {
    use std::collections::BTreeMap;
    let mut merged: BTreeMap<(String, String), proto::FixtureStat> = BTreeMap::new();
    for f in all {
        merged
            .entry((f.name.clone(), f.scope.clone()))
            .and_modify(|m| {
                m.count += f.count;
                m.total += f.total;
            })
            .or_insert(f);
    }
    merged.into_values().collect()
}

/// pytest-style warnings summary: grouped by location, deduped, counted.
fn print_warnings_summary(warnings: &[proto::WarningEntry], palette: &color::Palette) {
    if warnings.is_empty() {
        return;
    }
    use std::collections::BTreeMap;
    let mut merged: BTreeMap<(&str, u64, &str, &str), u64> = BTreeMap::new();
    for w in warnings {
        *merged
            .entry((&w.filename, w.lineno, &w.category, &w.message))
            .or_default() += w.count;
    }
    println!(
        "\n{}",
        palette.yellow("=========== warnings summary ===========")
    );
    for ((filename, lineno, category, message), count) in &merged {
        let times = if *count > 1 {
            format!("  ({count} occurrences)")
        } else {
            String::new()
        };
        println!("{filename}:{lineno}: {category}{times}");
        for line in message.lines().take(3) {
            println!("  {line}");
        }
    }
    println!(
        "{}",
        palette.yellow("-- use -W error::... to turn warnings into errors --")
    );
}

/// Compile the --quarantine file into one matcher: exact nodeids or `*`
/// globs, one per line, `#` comments and blanks skipped.
fn quarantine_matcher(path: &std::path::Path) -> Result<regex::RegexSet> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("--quarantine: cannot read {}: {e}", path.display()))?;
    let patterns: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            format!(
                "^{}$",
                l.split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(".*")
            )
        })
        .collect();
    if patterns.is_empty() {
        eprintln!("rstest: --quarantine: {} lists no patterns", path.display());
    }
    Ok(regex::RegexSet::new(patterns)?)
}

fn print_github_annotations(run: &report::Run) {
    // Under a monorepo the parent runs us with cwd=project, so nodeid paths
    // are project-relative; GitHub resolves annotation `file` from the repo
    // root, so prefix the project's root-relative path (set by the parent).
    let prefix = std::env::var("RSTEST_MONO_PROJECT")
        .ok()
        .filter(|p| !p.is_empty());
    for (nodeid, entry) in run.tests() {
        let failed = entry.call.as_deref() == Some("failed")
            || entry.setup.as_deref() == Some("failed")
            || entry.teardown.as_deref() == Some("failed");
        if !failed {
            continue;
        }
        let rel = nodeid.split("::").next().unwrap_or(nodeid);
        let file = match &prefix {
            Some(p) => format!("{p}/{rel}"),
            None => rel.to_string(),
        };
        let mut props = format!("file={},title={}", gh_prop(&file), gh_prop(nodeid));
        if let Some(l) = entry.lineno {
            props.push_str(&format!(",line={}", l + 1));
        }
        let msg = entry.longrepr.as_deref().unwrap_or("test failed");
        println!("::error {props}::{}", gh_data(msg));
    }
    // Flaky-passed tests (green only after reruns) surface as warnings:
    // the run is green, but the flake is visible on the PR without
    // opening the junit/log.
    for (nodeid, attempts) in &run.flaky {
        let Some(entry) = run.tests().get(nodeid) else {
            continue;
        };
        let rel = nodeid.split("::").next().unwrap_or(nodeid);
        let file = match &prefix {
            Some(p) => format!("{p}/{rel}"),
            None => rel.to_string(),
        };
        let mut props = format!("file={},title={}", gh_prop(&file), gh_prop(nodeid));
        if let Some(l) = entry.lineno {
            props.push_str(&format!(",line={}", l + 1));
        }
        println!(
            "::warning {props}::flaky: passed only after {attempts} rerun{}",
            if *attempts > 1 { "s" } else { "" }
        );
    }
}

/// Emit Azure Pipelines `##vso[task.logissue ...]` commands per failed test,
/// which Azure renders as inline issues on the PR (same mapping as GitHub).
/// Flaky-passed tests follow as `type=warning`; messages collapse to one line.
fn print_azure_annotations(run: &report::Run) {
    let prefix = std::env::var("RSTEST_MONO_PROJECT")
        .ok()
        .filter(|p| !p.is_empty());
    let source = |nodeid: &str| -> String {
        let rel = nodeid.split("::").next().unwrap_or(nodeid);
        match &prefix {
            Some(p) => format!("{p}/{rel}"),
            None => rel.to_string(),
        }
    };
    for (nodeid, entry) in run.tests() {
        let failed = entry.call.as_deref() == Some("failed")
            || entry.setup.as_deref() == Some("failed")
            || entry.teardown.as_deref() == Some("failed");
        if !failed {
            continue;
        }
        let mut props = format!("type=error;sourcepath={}", az_prop(&source(nodeid)));
        if let Some(l) = entry.lineno {
            props.push_str(&format!(";linenumber={}", l + 1));
        }
        let msg = entry.longrepr.as_deref().unwrap_or("test failed");
        println!("##vso[task.logissue {props}]{}: {}", nodeid, az_line(msg));
    }
    for (nodeid, attempts) in &run.flaky {
        let Some(entry) = run.tests().get(nodeid) else {
            continue;
        };
        let mut props = format!("type=warning;sourcepath={}", az_prop(&source(nodeid)));
        if let Some(l) = entry.lineno {
            props.push_str(&format!(";linenumber={}", l + 1));
        }
        println!(
            "##vso[task.logissue {props}]{nodeid}: flaky, passed only after {attempts} rerun{}",
            if *attempts > 1 { "s" } else { "" }
        );
    }
}

/// Azure logissue property value: `;` and `]` would end the property list /
/// command, newlines would split the log line.
fn az_prop(s: &str) -> String {
    az_line(s).replace(';', "%3B").replace(']', "%5D")
}

/// Collapse to the first line for a single-line Azure log message.
fn az_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// Buildkite: surface flaky-passed tests as a `warning` annotation on the
/// build page, best-effort (a missing/failing `buildkite-agent` must not fail
/// the run). No-op off Buildkite or when nothing flaked.
fn buildkite_flaky_annotate(run: &report::Run) {
    if run.flaky.is_empty()
        || std::env::var("BUILDKITE")
            .ok()
            .filter(|v| !v.is_empty())
            .is_none()
    {
        return;
    }
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut md = String::from("**Flaky tests** (passed only after reruns):\n\n");
    for (nodeid, attempts) in &run.flaky {
        md.push_str(&format!(
            "- `{nodeid}` — {attempts} rerun{}\n",
            if *attempts > 1 { "s" } else { "" }
        ));
    }
    let child = Command::new("buildkite-agent")
        .args([
            "annotate",
            "--style",
            "warning",
            "--context",
            "rstest-flaky",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rstest: skipping Buildkite flaky annotation (buildkite-agent: {e})");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(md.as_bytes());
    }
    if let Err(e) = child.wait() {
        eprintln!("rstest: buildkite-agent annotate failed: {e}");
    }
}

/// Escape a GitHub workflow-command message (the part after `::`).
fn gh_data(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escape a workflow-command property value (stricter: `:` and `,` too).
fn gh_prop(s: &str) -> String {
    gh_data(s).replace(':', "%3A").replace(',', "%2C")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gh_escaping_covers_command_metacharacters() {
        // message (data): only % \r \n are special
        assert_eq!(gh_data("a%b\nc\rd"), "a%25b%0Ac%0Dd");
        // property: also : and , so the key=value list can't be broken
        assert_eq!(gh_prop("pkg::test[a,b]"), "pkg%3A%3Atest[a%2Cb]");
        // % must escape first, or the other escapes' %XX would double-encode
        assert_eq!(gh_data("100%"), "100%25");
    }

    #[test]
    fn azure_logissue_escaping() {
        // property value: ; and ] would break the command; newlines collapse
        assert_eq!(az_prop("a;b]c"), "a%3Bb%5Dc");
        // message keeps only the first line, trimmed
        assert_eq!(az_line("  first line  \nsecond\nthird"), "first line");
        assert_eq!(az_line(""), "");
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
