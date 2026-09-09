//! Run orchestration: the single-project pipeline (`execute`) and its helpers.
//! `execute` is the entry point `main` and `watch` call. The monorepo driver
//! lives in [`monorepo`], the post-run gates/reports in [`gates`], and the
//! `--collect-only` discovery doc in [`discovery`].

mod discovery;
mod gates;
mod monorepo;

use std::io::{IsTerminal, Write};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::cli::{is_collect_only, needs_passthrough_io, parse_durations, parse_maxfail, Cli};
use crate::reporting::sink::Sink;
use crate::reporting::{color, flakes, progress, report};
use crate::scheduling::{durations, lazy, pool, proto, shard, worker};
use crate::{
    collect, config, coverage_skip, discover, doctor, incremental, migrate, mono, remote, select,
};

/// Resolve the effective `--changed` base rev: the flag's value, or `HEAD` when
/// `--changed-strict` implies it, run through git rev resolution. `None` = no
/// changed-selection requested.
fn resolve_changed_base(cli: &Cli, sink: &mut Sink) -> Result<Option<String>> {
    cli.changed
        .clone()
        .or_else(|| cli.changed_strict.then(|| "HEAD".to_string()))
        .map(|rev| select::resolve_base_rev(&rev, sink))
        .transpose()
}

/// `HEAD` means "diff the working tree" (no explicit rev); any other rev is the
/// diff base. The git-diff helpers take `Option<&str>` with that convention.
fn head_to_none(rev: &str) -> Option<&str> {
    (rev != "HEAD").then_some(rev)
}

/// `--changed`/`--since-green` selection: resolve the effective diff base
/// (`--since-green`'s last-green baseline overrides the `--changed-strict` HEAD
/// implication), then narrow `args` to the affected test targets.
/// `ControlFlow::Continue(args)` carries the narrowed (or unchanged) args on;
/// `ControlFlow::Break(code)` means nothing is affected — the caller returns
/// `code` as the process exit status (advancing the green baseline first under
/// `--since-green`), so the single `process::exit` stays in `main`.
/// `since_green`/`head`/`env_fp` are computed by the caller (they outlive
/// selection, feeding the post-run green-baseline record).
fn apply_selection(
    cli: &Cli,
    mut args: Vec<String>,
    since_green: bool,
    head: &Option<String>,
    env_fp: &str,
    sink: &mut Sink,
) -> Result<ControlFlow<i32, Vec<String>>> {
    let mut effective_changed = resolve_changed_base(cli, sink)?;
    if since_green {
        // --since-green owns the diff base: its last-green baseline drives
        // selection, OVERRIDING the "HEAD" base that --changed-strict would
        // otherwise imply (changed_strict is a gating modifier here, not a base;
        // an explicit --changed is already excluded by `since_green`). No
        // baseline yet -> a full run to establish one.
        match incremental::baseline(&std::env::current_dir()?, env_fp) {
            Some(sha) => {
                sink.warn(&format!(
                    "rstest: --since-green: selecting changes since last green run ({})",
                    &sha[..sha.len().min(12)]
                ));
                effective_changed = Some(sha);
            }
            None => {
                sink.warn(
                    "rstest: --since-green: no prior green run recorded; \
                     running everything to establish the baseline",
                );
                effective_changed = None;
            }
        }
    }
    if let Some(rev) = &effective_changed {
        let rev = head_to_none(rev);
        let cwd = std::env::current_dir()?;
        let project = config::discover(&cwd, sink.err());
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
                sink.warn(&format!(
                    "rstest: --changed falling back to full run ({reason})"
                ));
            }
            select::Selection::Tests(tests) if tests.is_empty() => {
                sink.out_line(&format!(
                    "rstest: no tests affected by {} changed file(s)",
                    changes.len()
                ));
                // Nothing affected since the last green run is itself a green
                // outcome: advance the baseline to HEAD so unrelated commits
                // don't force a re-run next time.
                if since_green {
                    if let Some(h) = &head {
                        incremental::record_green(&cwd, h, env_fp);
                    }
                }
                // Strict gating still wins on the exit code: it needs to
                // DISTINGUISH "ran nothing" from "everything passed" (pytest's
                // nothing-collected code), even under --since-green. Break with
                // the sentinel; main owns the actual process::exit.
                return Ok(ControlFlow::Break(if cli.changed_strict { 5 } else { 0 }));
            }
            select::Selection::Tests(tests) => {
                sink.warn(&format!(
                    "rstest: {} changed file(s) -> {} affected test target(s)",
                    changes.len(),
                    tests.len()
                ));
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
    Ok(ControlFlow::Continue(args))
}

/// Run-time context threaded into [`run_post_gates`]: the timing/cache/selection
/// state the post-run gates need that isn't part of the up-front [`RunConfig`]
/// (it depends on the actual run — start time, the resolved cache remote, the
/// incremental snapshot taken around the run).
struct PostRun<'a> {
    start: Instant,
    started_epoch: u64,
    run_uid: &'a str,
    /// Resolved `--cache-remote` (flag or env), already validated non-empty.
    cache_remote: Option<&'a str>,
    shard: Option<(usize, usize)>,
    since_green: bool,
    head: &'a Option<String>,
    env_fp: &'a str,
    incremental_active: bool,
    config_fp: &'a str,
    /// Coverage index snapshotted BEFORE the run (drives carry-forward after).
    prev_index: &'a select::CoverageIndex,
    baseline: &'a coverage_skip::Baseline,
}

/// The resolved run configuration for a single (non-watch) run: everything
/// derived from `cli` + `[tool.rstest]` + the forwarded pytest args, computed
/// once up front and handed to the dispatch and post-run stages. Selection
/// (`--changed`/shard/shuffle) and the incremental skip set are computed later,
/// so they stay out of here.
struct RunConfig {
    /// Raw `--numprocesses` value (e.g. "auto"/"4"), kept for banner text.
    numprocesses: String,
    /// Resolved worker count (forced to 1 for the single-worker rerun pool).
    n: usize,
    /// `--dist` name, validated but kept as a string (lazy/each check it).
    dist_name: String,
    reruns: u32,
    known_flaky: Option<std::collections::HashSet<String>>,
    worker_timeout: Option<u64>,
    passthrough: bool,
    single_worker_reruns: bool,
    very_verbose: bool,
    mode: progress::Mode,
    durations: Option<(usize, f64)>,
    doctor: bool,
    doctor_gate: Vec<doctor::GateCondition>,
    worker_env: worker::WorkerEnv,
    scope: PathBuf,
    python: PathBuf,
}

/// Resolve [`RunConfig`] from the CLI, `[tool.rstest]` settings, and forwarded
/// pytest args (CLI > settings > built-in defaults). Validates `--dist` and
/// `--doctor-fail-on` up front and resolves the interpreter, so a bad value or a
/// missing Python aborts before any worker spawns.
fn resolve_run_config(
    cli: &Cli,
    settings: &config::RstestSettings,
    args: &[String],
    run_uid: &str,
    sink: &mut Sink,
) -> Result<RunConfig> {
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
    // suite size, not slip through the lazy/small-suite path silently. The name
    // stays a string downstream (lazy/each checks); dispatch_run re-parses it to
    // the enum via the same `FromStr`.
    dist_name
        .parse::<pool::Dist>()
        .map_err(|e| anyhow::anyhow!(e))?;
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
    warn_windows_timeout(sink.err(), cfg!(windows), cli.timeout, worker_timeout);
    let n = parse_numprocesses(&numprocesses)?;
    // `--debug` runs one worker with inherited stdio (like --pdb) so debugpy
    // owns a single process and its console; route it through the passthrough
    // path regardless of the session flags.
    let passthrough = needs_passthrough_io(args) || cli.debug.is_some();
    // Honor `--reruns` in single-worker mode via a degenerate one-worker pool:
    // the rerun loop is orchestrator-side (rerunfailures neutralized inside).
    // Passthrough can't be pooled, so reruns stay inert there.
    let single_worker_reruns = reruns > 0 && n <= 1 && !passthrough;
    // A one-worker rerun pool is 1 worker everywhere downstream (banner,
    // doctor, report-json meta), never 0.
    let n = if single_worker_reruns { 1 } else { n };
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
            sink.warn(&format!(
                "rstest: unknown --output '{other}' \
                 (use dots|verbose|bar|github|gitlab|buildkite|teamcity|azure|tap|json); using dots"
            ));
            progress::Mode::Dots
        }
        None if verbose => progress::Mode::Verbose,
        None if std::io::stdout().is_terminal() => progress::Mode::Bar,
        None => progress::Mode::Dots,
    };
    let durations = parse_durations(args);
    // Validate `--doctor-fail-on` conditions up front: a typo'd metric or a
    // missing operator aborts now, never silently as a gate that can't fire.
    let doctor_gate = doctor::parse_conditions(&cli.doctor_fail_on, sink)?;
    let doctor = cli.doctor
        || cli.doctor_json.is_some()
        || cli.doctor_md.is_some()
        || !doctor_gate.is_empty();
    // Run-wide worker params (testrun uid + doctor instrumentation) travel via
    // each worker's environment at spawn (thread-safe), never this process's
    // global env.
    // Leak measurement runs under doctor OR --fail-on-leak (doctor already
    // instruments; --fail-on-leak needs the deltas without the full report).
    let leakcheck = doctor || cli.fail_on_leak;
    let worker_env = worker::WorkerEnv {
        run_uid: run_uid.to_string(),
        doctor,
        timeout: cli.timeout,
        leakcheck,
        send_ids: false,
        debug_port: cli.debug.clone(),
        // Ship captured stdout/stderr on every report (not just failures) when a
        // live JSON consumer is attached, so editors get per-passing-test output.
        stream_output: mode == progress::Mode::Json || cli.stream_json.is_some(),
    };

    // Session args forward verbatim: the vendored core owns ini semantics
    // (python_files, testpaths, rootdir) and collection, so session
    // behavior is exactly pytest's.
    let scope = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let python = discover::resolve(&scope, cli.python.as_deref())?;
    Ok(RunConfig {
        numprocesses,
        n,
        dist_name,
        reruns,
        known_flaky,
        worker_timeout,
        passthrough,
        single_worker_reruns,
        very_verbose,
        mode,
        durations,
        doctor,
        doctor_gate,
        worker_env,
        scope,
        python,
    })
}

/// Wire up the `--stream-json FILE` side channel: open FILE for truncating
/// write (creating it if absent) and attach it to `sink`, or warn to stderr if
/// it can't be opened. FILE may be a regular file or a named pipe the editor
/// already opened for reading. Open failure is non-fatal — the run continues
/// without the side channel.
fn attach_stream_json(sink: &mut Sink, path: &std::path::Path) {
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(f) => sink.attach_stream(Box::new(f)),
        Err(e) => sink.warn(&format!("rstest: --stream-json {}: {e}", path.display())),
    }
}

/// The crate's main entry point for a single (non-watch) run: resolves the
/// run configuration from `cli` + forwarded pytest `args`, dispatches to the
/// worker pool (or the monorepo driver), runs post-run reports and gates
/// (doctor, junit, lastfailed, duration-regression, cache push, report-json),
/// and returns the process exit status.
pub fn execute(cli: &Cli, args: &[String]) -> Result<i32> {
    let args = args.to_vec();
    // The single output sink for this run: owns stdout/stderr and the resolved
    // palette. Built up front so every diagnostic below (cache maintenance,
    // selection, banner) flows through it. `--color` resolution matches
    // `resolve_run_config`'s (both call `Palette::detect`).
    let mut sink = Sink::stdio(color::Palette::detect(&args));
    // `--stream-json FILE`: attach the live per-test NDJSON side channel. FILE
    // may be a regular file or a named pipe the editor already opened for
    // reading (opening a fifo for write blocks until that reader is present).
    if let Some(path) = &cli.stream_json {
        attach_stream_json(&mut sink, path);
    }
    let start = Instant::now();
    let started_epoch = crate::time::now_epoch_secs();
    // One uid per test run, shared by every worker (xdist's testrun_uid
    // contract). A monorepo child inherits the root's (passed explicitly on the
    // child's command); a top-level run generates one. Held as a typed value and
    // handed to workers via their environment — never process-global set_var.
    let run_uid = std::env::var("RSTEST_RUN_UID").unwrap_or_else(|_| {
        let nanos = crate::time::now_epoch_nanos();
        format!("{nanos:x}{:x}", std::process::id())
    });
    // Shared-cache backend: resolve the remote (flag or env) and, if asked,
    // run maintenance / warm the local cache BEFORE anything reads it.
    let cache_remote = cli
        .cache_remote
        .clone()
        .or_else(|| std::env::var("RSTEST_CACHE_REMOTE").ok())
        .filter(|s| !s.is_empty());
    preflight_cache(cli, &cache_remote, &mut sink)?;

    // CLI > [tool.rstest] > built-in defaults.
    let settings = config::rstest_settings(&std::env::current_dir()?, sink.err());

    // Monorepo: cwd has no pytest config of its own but subdirectories do —
    // dispatch to the per-project driver and return its exit code.
    if let ControlFlow::Break(code) =
        maybe_dispatch_monorepo(cli, &args, &settings, &run_uid, &mut sink)?
    {
        return Ok(code);
    }
    // --cache-pull: warm the local cache from the remote BEFORE anything reads
    // it (scheduling, selection, the regression baseline). Placed after the
    // monorepo guard so a monorepo run is rejected rather than pulling into the
    // wrong (root) cache and printing a misleading success line first.
    pull_shared_cache(cli, &cache_remote, &mut sink)?;
    let cfg = resolve_run_config(cli, &settings, &args, &run_uid, &mut sink)?;
    // Lift the resolved config into the local names the rest of the pipeline
    // reads. Copy fields copy; the few owned fields clone once (cheap) so their
    // types match the original locals exactly, leaving `cfg` intact to hand to
    // `dispatch_run` as one bundle. `known_flaky`/`worker_env` are read only via
    // that bundle, so they stay in `cfg`.
    let RunConfig {
        n,
        reruns,
        passthrough,
        single_worker_reruns,
        very_verbose,
        mode,
        durations,
        ..
    } = cfg;
    let scope = cfg.scope.clone();
    let python = cfg.python.clone();
    // `--collect-only --report-json <p>` writes a structured discovery doc
    // (nodeid + abs file + 0-based line + markers), the machine-readable
    // surface editors/CI consume. Own single-session path (NOT passthrough).
    if is_collect_only(&args) {
        if let Some(out) = &cli.report_json {
            return discovery::run_collect_discovery(&python, &args, out, &run_uid);
        }
    }
    check_require_baseline(cli, passthrough)?;
    print_run_banner(mode, passthrough, single_worker_reruns, n, &mut sink);
    // Incremental testing: --since-green feeds --changed's selection from the
    // last green run's commit. An explicit --changed always wins. `head` is
    // captured up front (it can't change mid-run) so a green run can record it.
    let since_green = cli.since_green && cli.changed.is_none();
    // Only shell out to git / hash the env when --since-green is actually
    // active, so the default run path pays nothing.
    let head = since_green.then(incremental::head_sha).flatten();
    let env_fp = if since_green {
        incremental::env_fingerprint(&scope, &python)
    } else {
        String::new()
    };
    // Narrow args to the affected test targets. Nothing affected => Break with
    // the sentinel exit code, returned up so main owns the single process::exit.
    let args = match apply_selection(cli, args, since_green, &head, &env_fp, &mut sink)? {
        ControlFlow::Continue(args) => args,
        ControlFlow::Break(code) => return Ok(code),
    };
    warn_run_modes(
        reruns,
        passthrough,
        single_worker_reruns,
        &cfg.numprocesses,
        &mut sink,
    );
    // Resolve dispatch selection (--shuffle/--shard) and the --incremental skip
    // set in one phase.
    let inc = resolve_incremental(&cfg, cli, &settings, &args, since_green, &mut sink)?;
    let mut outcome = dispatch_run(
        &cfg,
        cli,
        &settings,
        &args,
        &inc.skip_ids,
        DispatchSelection {
            shuffle_seed: inc.shuffle_seed,
            shard: inc.shard,
        },
        &mut sink,
    )?;

    apply_quarantine(cli, &mut outcome, passthrough, &mut sink)?;
    gates::finalize_output(
        &mut outcome,
        passthrough,
        mode,
        durations,
        very_verbose,
        start,
        &mut sink,
    );

    let post = PostRun {
        start,
        started_epoch,
        run_uid: &run_uid,
        cache_remote: cache_remote.as_deref(),
        shard: inc.shard,
        since_green,
        head: &head,
        env_fp: &env_fp,
        incremental_active: inc.active,
        config_fp: &inc.config_fp,
        prev_index: &inc.prev_index,
        baseline: &inc.baseline,
    };
    gates::run_post_gates(&cfg, cli, &mut outcome, &args, &post, &mut sink)
}

/// Dispatch a run-less subcommand (`rstest verify-vendor` / `try` /
/// `migrate-check` / `cache-compact`). Returns `Some(exit)` when a subcommand
/// ran, `None` for a normal run (the caller falls through to watch/`execute`).
/// These modes bypass the run pipeline, so the interpreter is resolved here
/// rather than pulled through [`resolve_run_config`].
pub fn dispatch_command(cli: &Cli, args: &[String]) -> Result<Option<i32>> {
    use crate::cli::Command;
    let Some(command) = &cli.command else {
        return Ok(None);
    };
    let mut sink = Sink::stdio(color::Palette::detect(args));
    // cache-compact is interpreter-free; the rest resolve Python first.
    if let Command::CacheCompact = command {
        return Ok(Some(run_cache_compact(cli, &mut sink)?));
    }
    let scope = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let python = discover::resolve(&scope, cli.python.as_deref())?;
    let code = match command {
        // Verify the vendored pytest tree against the packaged manifest.
        Command::VerifyVendor => crate::vendor::run_verify(&python)?,
        // Zero-config "should I switch?" proof: pytest baseline vs rstest -n auto.
        Command::Try => migrate::run_try(&python, args, &mut sink)?,
        // Parallel-readiness preflight: its own collect-twice path, not a run.
        Command::MigrateCheck => migrate::run_migrate_check(
            &python,
            args,
            cli.migrate_check_json.as_deref(),
            &cli.migrate_allow,
            &mut sink,
        )?,
        Command::CacheCompact => unreachable!("handled above"),
    };
    Ok(Some(code))
}

/// `cache-compact` subcommand: fold all remote segments into a fresh base and
/// prune them, then exit without running tests. Resolves the remote from the
/// `--cache-remote` flag or `RSTEST_CACHE_REMOTE`.
fn run_cache_compact(cli: &Cli, sink: &mut Sink) -> Result<i32> {
    let remote = cli
        .cache_remote
        .clone()
        .or_else(|| std::env::var("RSTEST_CACHE_REMOTE").ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("cache-compact needs --cache-remote (or RSTEST_CACHE_REMOTE)")
        })?;
    let t = remote::transport_for(&remote)?;
    let folded = remote::compact_remote(t.as_ref(), sink)
        .with_context(|| format!("compacting shared cache at {remote}"))?;
    sink.warn(&format!(
        "rstest: cache: compacted {folded} segment(s) into base at {remote}"
    ));
    Ok(0)
}

/// Warm the local cache from the remote (`--cache-pull`) BEFORE anything reads
/// it (scheduling, selection, the regression baseline). No-op without the flag.
fn pull_shared_cache(cli: &Cli, cache_remote: &Option<String>, sink: &mut Sink) -> Result<()> {
    if !cli.cache_pull {
        return Ok(());
    }
    let remote = cache_remote.as_deref().unwrap(); // validated by preflight_cache
    let t = remote::transport_for(remote)?;
    let merged = remote::pull(t.as_ref(), sink)
        .with_context(|| format!("pulling shared cache from {remote}"))?;
    sink.warn(&format!(
        "rstest: cache: pulled {} duration(s), {} flake record(s) from {remote}",
        merged.durations.len(),
        merged.flakes.len()
    ));
    remote::write_local(&merged);
    Ok(())
}

/// Monorepo dispatch: when the cwd has no pytest config of its own but
/// subdirectories do, each subproject runs as its own session group (cwd
/// switched, so rootdir/ini/conftest match pytest-in-that-dir). Explicit paths
/// stay single. Returns `Break(code)` when the per-project driver ran, else
/// `Continue(())` to fall through to the single-project run.
fn maybe_dispatch_monorepo(
    cli: &Cli,
    args: &[String],
    settings: &config::RstestSettings,
    run_uid: &str,
    sink: &mut Sink,
) -> Result<ControlFlow<i32>> {
    if std::env::var_os("RSTEST_MONO_PROJECT").is_some() {
        return Ok(ControlFlow::Continue(()));
    }
    let cwd = std::env::current_dir()?;
    let path_args = args
        .iter()
        .any(|a| !a.starts_with('-') && std::path::Path::new(a).exists());
    if path_args || config::has_pytest_config(&cwd, sink.err()) {
        return Ok(ControlFlow::Continue(()));
    }
    let projects = mono::discover_projects(&cwd, settings.projects.as_deref());
    let threshold = if settings.projects.is_some() { 1 } else { 2 };
    if projects.len() < threshold {
        return Ok(ControlFlow::Continue(()));
    }
    // Each project keeps its OWN .rstest_cache (cache::file_in), and the per-run
    // push/pull wiring lives in the single-project path that execute_monorepo
    // bypasses — so a cache flag here would silently no-op (push) or warm the
    // wrong root cache (pull). Fail loud; run rstest per project for shared caching.
    if cli.cache_pull || cli.cache_push {
        anyhow::bail!(
            "--cache-pull/--cache-push are not supported in monorepo mode \
             (each project has its own .rstest_cache); run rstest per project"
        );
    }
    monorepo::execute_monorepo(cli, args, &cwd, projects, run_uid, sink).map(ControlFlow::Break)
}

/// require-baseline: with the durations-regress gate active, an absent baseline
/// (cold remote, nothing restored/pulled) is a hard error rather than the silent
/// skip the gate would otherwise do — the dead-gate guard. Gated on
/// `!passthrough`, since the regression gate only runs on a real in-process run;
/// the cache pull has already warmed the baseline it checks.
fn check_require_baseline(cli: &Cli, passthrough: bool) -> Result<()> {
    if cli.require_baseline
        && cli.durations_regress.is_some()
        && !passthrough
        && durations::load().is_empty()
    {
        anyhow::bail!(
            "--require-baseline: --durations-regress needs a duration baseline in \
             .rstest_cache, but none is present (cold cache — nothing restored or pulled)"
        );
    }
    Ok(())
}

/// The one-line run banner. Json/Tap keep stdout a pure machine stream: no
/// banner (TAP gets its version header instead).
fn print_run_banner(
    mode: progress::Mode,
    passthrough: bool,
    single_worker_reruns: bool,
    n: usize,
    sink: &mut Sink,
) {
    if passthrough {
        return;
    }
    if mode == progress::Mode::Tap {
        sink.out_line("TAP version 13");
        return;
    }
    if mode == progress::Mode::Json {
        return;
    }
    let worker_desc = if single_worker_reruns {
        "single worker (rerun pool; not byte-exact)".to_string()
    } else if n <= 1 {
        "single worker (pytest-exact mode)".to_string()
    } else {
        format!("{n} workers (parallel by default; -n 0 for single-worker mode)")
    };
    sink.out_line(&format!(
        "rstest {} — {worker_desc}",
        env!("CARGO_PKG_VERSION")
    ));
}

/// Pre-run mode warnings: `--reruns` inert under passthrough, and the byte-exact
/// -> one-worker-rerun-pool switch (announced so log scrapers see it).
fn warn_run_modes(
    reruns: u32,
    passthrough: bool,
    single_worker_reruns: bool,
    numprocesses: &str,
    sink: &mut Sink,
) {
    if reruns > 0 && passthrough {
        sink.warn(
            "rstest: --reruns is ignored under -s/--pdb/--co \
             (interactive single session); drop those flags to enable reruns",
        );
    }
    if single_worker_reruns {
        sink.warn(&format!(
            "rstest: --reruns at -n {numprocesses} runs a one-worker rerun pool \
             (not byte-exact); use -n 0/1 without --reruns for the byte-exact session"
        ));
    }
}

/// Resolved dispatch selection (`--shuffle`/`--shard`) plus the `--incremental`
/// dispatch-level skip set and the state the post-run carry-forward needs.
struct Incremental {
    shuffle_seed: Option<u64>,
    shard: Option<(usize, usize)>,
    /// `--incremental` is actually in effect this run (all preconditions met).
    active: bool,
    config_fp: String,
    /// Coverage index snapshotted BEFORE the run (drives skip now, carry-forward after).
    prev_index: select::CoverageIndex,
    baseline: coverage_skip::Baseline,
    skip_ids: std::collections::HashSet<String>,
}

/// Resolve `--shuffle`/`--shard` and compute the `--incremental` skip set in one
/// phase. `--incremental` skipping is restricted to the eager parallel pool on
/// `--dist load` with full collection; incompatible modes disable it with a note
/// (an explicit `--changed`/`--since-green` already narrows selection, so
/// dispatch-level skipping on top is excluded — it would record a PARTIAL green
/// baseline). The coverage index / baseline / skip set are loaded only when the
/// feature is actually active, so the default run path pays nothing.
fn resolve_incremental(
    cfg: &RunConfig,
    cli: &Cli,
    settings: &config::RstestSettings,
    args: &[String],
    since_green: bool,
    sink: &mut Sink,
) -> Result<Incremental> {
    let RunConfig {
        n,
        passthrough,
        single_worker_reruns,
        ref dist_name,
        ref scope,
        ..
    } = *cfg;
    // --shuffle reorders the orchestrator's dispatch queue, so it needs the
    // full-collection pool. Refusing (not ignoring) matters: a user probing for
    // order dependence must not get a silently ordered run. `is_lazy` is computed
    // once and reused by the incremental gate below.
    let is_lazy = collect_lazy(cli, settings, dist_name, args, sink)?;
    let shuffle_seed = resolve_shuffle_seed(
        cli.shuffle.as_deref(),
        n,
        passthrough,
        single_worker_reruns,
        is_lazy,
        dist_name,
        sink,
    )?;
    // --shard K/N: partition the suite and keep bucket K. Purely an
    // orchestrator-side node-id (or, in lazy mode, file) filter.
    let shard = resolve_shard(
        cli.shard.as_deref(),
        n,
        passthrough,
        single_worker_reruns,
        shuffle_seed.is_some(),
        dist_name,
    )?;
    let active = cli.incremental
        && !since_green
        && cli.changed.is_none()
        && dist_name.as_str() == "load"
        && !passthrough
        && n >= 2
        && shard.is_none()
        && shuffle_seed.is_none()
        && !is_lazy;
    // The config fingerprint is only consumed under `active`; computing it
    // unconditionally would walk the whole project tree for conftests.
    let config_fp = if active {
        coverage_skip::config_fingerprint(scope)
    } else {
        String::new()
    };
    warn_incremental_conflicts(
        sink.err(),
        cli.incremental,
        since_green,
        cli.changed.is_some(),
        active,
    );
    // --incremental relies on the coverage index advancing every run; without
    // --cov this run covtool never rewrites it, so a changed test re-runs on
    // every invocation until a coverage run refreshes the index.
    if active && !coverage_skip::coverage_requested(args) {
        sink.warn(
            "rstest: --incremental without --cov: the coverage index won't be \
             refreshed this run, so changed tests keep re-running until a --cov run",
        );
    }
    // A narrowed --cov=<pkg> makes first-party source OUTSIDE the scope
    // coverage-invisible: editing it won't bust the skip, so a test depending on
    // it can be wrongly cached (stale false-green). Warn; --cov=. closes the gap.
    if active && coverage_skip::cov_scope_narrowed(args) {
        sink.warn(
            "rstest: --incremental with a scoped --cov: edits to first-party source \
             outside the coverage scope are undetectable and may leave a test cached \
             on a stale pass; use --cov=. to cover the whole tree",
        );
    }
    // Snapshot the index BEFORE the run: it drives the skip decision now, and
    // post-run it supplies the cached tests' coverage to fold back in (covtool
    // rewrites the index from only the tests that ran).
    let prev_index = if active {
        remote::load_local_cov_index()
    } else {
        select::CoverageIndex::default()
    };
    // The baseline is loaded once and kept: it drives the skip set now, and its
    // recorded def lines restore the cached (not-run) entries' source line after
    // the run (a cached test has no pytest report to supply one).
    let baseline = if active {
        coverage_skip::load(scope, &config_fp)
    } else {
        coverage_skip::Baseline::default()
    };
    let skip_ids = if active {
        coverage_skip::skippable_now(&prev_index, &baseline)
    } else {
        std::collections::HashSet::new()
    };
    Ok(Incremental {
        shuffle_seed,
        shard,
        active,
        config_fp,
        prev_index,
        baseline,
        skip_ids,
    })
}

/// Apply `--quarantine` BEFORE any output or exit-code consumer: classification,
/// counts, junit, report-json, and the sessionfinish envelope must all see the
/// demoted outcomes consistently. Inert under passthrough (no aggregate Run).
fn apply_quarantine(
    cli: &Cli,
    outcome: &mut pool::PoolOutcome,
    passthrough: bool,
    sink: &mut Sink,
) -> Result<()> {
    let Some(qpath) = &cli.quarantine else {
        return Ok(());
    };
    if passthrough {
        warn_quarantine_passthrough(sink.err());
        return Ok(());
    }
    let matcher = gates::quarantine_matcher(qpath, sink)?;
    let demoted = outcome.run.quarantine(|id| matcher.is_match(id));
    // pytest exit 1 = tests failed; if every failure was quarantined the run is
    // green by policy. Exit codes 2+ (usage/internal errors) are never touched.
    if !demoted.is_empty() && outcome.exitstatus == 1 && outcome.run.all_passed() {
        outcome.exitstatus = 0;
    }
    Ok(())
}

/// Dispatch-time selection modifiers, bundled so [`dispatch_run`] stays within
/// the argument budget: the resolved `--shuffle` seed and `--shard` bucket.
struct DispatchSelection {
    shuffle_seed: Option<u64>,
    shard: Option<(usize, usize)>,
}

fn dispatch_run(
    cfg: &RunConfig,
    cli: &Cli,
    settings: &config::RstestSettings,
    args: &[String],
    skip_ids: &std::collections::HashSet<String>,
    selection: DispatchSelection,
    sink: &mut Sink,
) -> Result<pool::PoolOutcome> {
    let DispatchSelection {
        shuffle_seed,
        shard,
    } = selection;
    let RunConfig {
        n,
        reruns,
        worker_timeout,
        passthrough,
        single_worker_reruns,
        mode,
        durations,
        ref dist_name,
        ref known_flaky,
        ref worker_env,
        ref python,
        ..
    } = *cfg;
    let python = python.as_path();
    let worker_env: &worker::WorkerEnv = worker_env;
    let known_flaky = known_flaky.as_ref();
    let watchdog = watchdog_duration(worker_timeout, cli.timeout);
    // Compiled once and shared by both pool paths (a config-struct field, so it
    // must outlive the borrow); the passthrough path below ignores it.
    let only_rerun = cli
        .only_rerun
        .iter()
        .map(|p| regex::Regex::new(p))
        .collect::<Result<Vec<_>, _>>()?;
    let base_cfg = pool::PoolConfig {
        python,
        n,
        args,
        mode,
        maxfail: parse_maxfail(args),
        reruns,
        only_rerun: &only_rerun,
        worker_timeout: watchdog,
        known_flaky,
        worker_env,
    };
    Ok(if passthrough || (n <= 1 && !single_worker_reruns) {
        let io = if passthrough {
            worker::Stdio::Inherit
        } else {
            worker::Stdio::Null
        };
        let mut w = worker::Worker::spawn_with_io(python, None, io, worker_env)?;
        w.send(&proto::Command::RunTests {
            args: args.to_vec(),
        })?;
        let mut run = report::Run::default();
        run.track_phase_durations = durations.is_some();
        let mut prog = progress::Progress::default();
        prog.set_mode(mode);
        let mut fixtures: Vec<proto::FixtureStat> = Vec::new();
        let mut warnings: Vec<proto::WarningEntry> = Vec::new();
        let exitstatus = loop {
            if let Some(code) = fold_run_event(
                w.recv()?,
                passthrough,
                &mut run,
                &mut prog,
                &mut fixtures,
                &mut warnings,
                sink,
            ) {
                break code;
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
    } else if collect_lazy(cli, settings, dist_name, args, sink)? {
        let cwd = std::env::current_dir()?;
        let project = config::discover(&cwd, sink.err());
        let paths: Vec<PathBuf> = args
            .iter()
            .filter(|a| !a.starts_with('-') && std::path::Path::new(a).exists())
            .map(PathBuf::from)
            .collect();
        let mut files = collect::collect_test_files(&paths, &project)?;
        if let Some((k, total)) = shard {
            let before = files.len();
            files = shard::shard_files(&files, &durations::load(), &cwd, k, total);
            sink.warn(&format!(
                "rstest: shard {k}/{total} -> {} of {before} test file(s)",
                files.len()
            ));
        }
        let cfg = pool::PoolConfig {
            n: n.min(files.len().max(1)),
            ..base_cfg
        };
        lazy::run_lazy_pool(
            &cfg,
            files,
            // Steal (split files across workers) only on an EXPLICIT --dist
            // load: lazy defaults to strict file affinity, since stealing
            // exposes cross-file/in-file order dependence affinity doesn't.
            lazy_should_steal(cli.dist.as_deref(), settings.dist.as_deref()),
            sink,
        )?
    } else {
        let dist = dist_name
            .parse::<pool::Dist>()
            .map_err(|e| anyhow::anyhow!(e))?;
        if dist == pool::Dist::Each && reruns > 0 {
            anyhow::bail!(
                "--reruns is not supported with --dist each (every worker runs the \
                 full suite; rerun-on-another-worker semantics do not apply)"
            );
        }
        pool::run_pool(
            &base_cfg,
            dist,
            durations.is_some(),
            shuffle_seed,
            shard,
            skip_ids,
            sink,
        )?
    })
}

/// Resolve the collection strategy (CLI > [tool.rstest] > "full") and
/// validate lazy-mode constraints.
fn collect_lazy(
    cli: &Cli,
    settings: &config::RstestSettings,
    dist_name: &str,
    args: &[String],
    sink: &mut Sink,
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
                sink.warn(
                    "rstest: nodeid/--pyargs arguments given; --collect lazy falls back \
                     to full collection",
                );
                return Ok(false);
            }
            Ok(true)
        }
        other => anyhow::bail!("unknown --collect mode: {other} (use full|lazy)"),
    }
}

/// Warn (once, to `w`) when `--timeout` is asked for on Windows: interrupting a
/// blocked test in-process needs SIGALRM firing inside the stuck syscall, which
/// Windows lacks, so the per-test deadline can't be enforced. Silent when the
/// user already set `--worker-timeout` (they have an explicit hang backstop) or
/// off Windows. Takes `is_windows` as a param (not `cfg!`) so both branches are
/// exercised under coverage on any host.
fn warn_windows_timeout(
    w: &mut dyn std::io::Write,
    is_windows: bool,
    timeout: Option<f64>,
    worker_timeout: Option<u64>,
) {
    if is_windows && timeout.is_some() && worker_timeout.is_none() {
        let _ = writeln!(
            w,
            "rstest: warning: --timeout can't interrupt a blocked test in-process on Windows \
             (no SIGALRM). rstest auto-arms the coarser --worker-timeout watchdog from it, which \
             kills the whole worker (not just the stuck test) once the deadline is well exceeded; \
             set --worker-timeout SECS to tune that cap."
        );
    }
}

/// Watchdog duration for a run: explicit `--worker-timeout` wins; otherwise
/// auto-arm from `--timeout` at a generous multiple, so the worker's in-process
/// interrupt fires first and the watchdog only catches a C-ext deadlock the
/// signal can't reach (the test never returns to the interpreter). Only a
/// positive, finite `--timeout` arms it — mirroring the worker's
/// `_parse_timeout` (0/negative/NaN = disabled) — and the computed duration is
/// clamped so a huge or near-overflow value can't panic `from_secs_f64`. A bad
/// `--timeout` must not crash the run.
fn watchdog_duration(
    worker_timeout: Option<u64>,
    timeout: Option<f64>,
) -> Option<std::time::Duration> {
    worker_timeout
        .map(std::time::Duration::from_secs)
        .or_else(|| {
            timeout.filter(|t| t.is_finite() && *t > 0.0).map(|t| {
                std::time::Duration::try_from_secs_f64(t * 3.0 + 10.0)
                    .unwrap_or(std::time::Duration::MAX)
            })
        })
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
        // Worker-sizing has no run Sink; a malformed-config note here is a dup of
        // the one the real discover emits through the Sink, so send it to stderr.
        let project = config::discover(&cwd, &mut std::io::stderr());
        if let Ok(files) = collect::collect_test_files(&[], &project) {
            n = cap_workers_by_files(n, files.len());
        }
    }

    let cache = durations::load();
    if !cache.is_empty() {
        n = cap_workers_by_time(n, cache.values().sum());
    }

    n.max(1)
}

/// Cap the worker count by test-file count: never more workers than files. An
/// empty walk (count 0) leaves `n` unchanged — the other signals still apply.
fn cap_workers_by_files(n: usize, file_count: usize) -> usize {
    if file_count > 0 {
        n.min(file_count)
    } else {
        n
    }
}

/// Cap by suite time from the duration cache: ~2s of test time amortizes one
/// worker's startup, so a few-second suite needs only a couple. Never drops
/// below 1 worker.
fn cap_workers_by_time(n: usize, total_secs: f64) -> usize {
    let by_time = (total_secs / 2.0).ceil() as usize;
    n.min(by_time.max(1))
}

/// The shared-cache flags need a resolved remote. (`cache-compact` is now a
/// run-less subcommand whose combination with `--cache-pull/--cache-push` is
/// rejected by clap, since those flags aren't global.)
fn validate_cache_flags(pull: bool, push: bool, remote_present: bool) -> Result<()> {
    if (pull || push) && !remote_present {
        anyhow::bail!("--cache-pull/--cache-push need --cache-remote (or RSTEST_CACHE_REMOTE)");
    }
    Ok(())
}

/// Cache-flag preflight for a normal run: validate that `--cache-pull` /
/// `--cache-push` have a resolved remote, then warn when the `--cache-remote`
/// FLAG is set with neither requested (it would silently do nothing). Gate the
/// warn on the flag, NOT the env-resolved value: `RSTEST_CACHE_REMOTE` is
/// ambient config a CI sets once, and plain runs that don't opt into pull/push
/// must not be nagged every invocation.
fn preflight_cache(cli: &Cli, cache_remote: &Option<String>, sink: &mut Sink) -> Result<()> {
    validate_cache_flags(cli.cache_pull, cli.cache_push, cache_remote.is_some())?;
    if cli.cache_remote.is_some() && !cli.cache_pull && !cli.cache_push {
        sink.warn(
            "rstest: cache: --cache-remote is set but no --cache-pull/--cache-push \
             was requested; the shared cache is not being used",
        );
    }
    Ok(())
}

/// Resolve `--shuffle` into an optional dispatch seed. `--shuffle` needs the
/// parallel pool (`-n >= 2`, not passthrough), can't ride the file-affine lazy
/// collector, and is meaningless under `--dist each`; a `random` value stamps a
/// time+pid seed, otherwise the value must parse as `u64`. Prints the resolved
/// seed to `w` so a shuffled run is reproducible.
fn resolve_shuffle_seed(
    shuffle: Option<&str>,
    n: usize,
    passthrough: bool,
    single_worker_reruns: bool,
    is_lazy: bool,
    dist_name: &str,
    sink: &mut Sink,
) -> Result<Option<u64>> {
    let Some(v) = shuffle else { return Ok(None) };
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
    if is_lazy {
        anyhow::bail!("--shuffle is not supported with --collect lazy");
    }
    if dist_name == "each" {
        anyhow::bail!(
            "--shuffle is not supported with --dist each (workers run the \
             full suite in session order)"
        );
    }
    let seed = if v == "random" {
        crate::time::now_epoch_nanos() as u64 ^ u64::from(std::process::id())
    } else {
        v.parse()
            .map_err(|_| anyhow::anyhow!("--shuffle seed must be an unsigned integer, got '{v}'"))?
    };
    sink.warn(&format!(
        "rstest: shuffle seed {seed} (reproduce with --shuffle={seed})"
    ));
    Ok(Some(seed))
}

/// Resolve `--shard K/N` into an optional `(k, total)` dispatch filter. `1/1` is
/// the whole suite (no-op). Otherwise it needs the parallel pool, can't combine
/// with `--shuffle` (shards must partition identically on every machine), and is
/// meaningless under `--dist each`.
fn resolve_shard(
    shard: Option<&str>,
    n: usize,
    passthrough: bool,
    single_worker_reruns: bool,
    has_shuffle: bool,
    dist_name: &str,
) -> Result<Option<(usize, usize)>> {
    let Some(spec) = shard else { return Ok(None) };
    let (k, total) = shard::parse_shard(spec)?;
    if total == 1 {
        return Ok(None); // 1/1 is the whole suite: no-op.
    }
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
    if has_shuffle {
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
    Ok(Some((k, total)))
}

/// Warn (to `w`) when `--incremental` can't run as requested: both incremental
/// modes select on the same run, so `--since-green`/`--changed` take precedence,
/// and dispatch-level skipping needs the parallel full-collection `--dist load`
/// pool. At most one note fires (the first applicable), mirroring the
/// precedence order.
fn warn_incremental_conflicts(
    w: &mut dyn Write,
    incremental: bool,
    since_green: bool,
    changed_some: bool,
    incremental_active: bool,
) {
    if incremental && since_green {
        let _ = writeln!(
            w,
            "rstest: --incremental and --since-green are mutually exclusive; \
             --since-green takes precedence this run"
        );
    } else if incremental && changed_some {
        let _ = writeln!(
            w,
            "rstest: --incremental and --changed are mutually exclusive; \
             --changed owns selection this run"
        );
    } else if incremental && !incremental_active {
        let _ = writeln!(
            w,
            "rstest: --incremental needs the parallel pool with full collection and \
             --dist load (not -n 0/1, --dist each/affinity, --collect lazy, --shard, or \
             --shuffle); running everything this time"
        );
    }
}

/// Note (to `w`) that `--quarantine` is inert under passthrough IO (-s/--pdb/--co):
/// there is no aggregate Run to demote outcomes in.
fn warn_quarantine_passthrough(w: &mut dyn Write) {
    let _ = writeln!(
        w,
        "rstest: --quarantine has no effect in passthrough mode; ignoring"
    );
}

/// Steal (split files across workers) only on an EXPLICIT `--dist load`: lazy
/// collection defaults to strict file affinity, since stealing exposes the
/// cross-file / in-file order dependence that affinity hides.
fn lazy_should_steal(cli_dist: Option<&str>, settings_dist: Option<&str>) -> bool {
    cli_dist == Some("load") || settings_dist == Some("load")
}

/// Fold one worker event into the single-session accumulators (the byte-exact /
/// passthrough / one-worker-rerun path). Returns `Some(exitstatus)` on `Done`.
/// Reports drive progress (suppressed under passthrough, whose IO is inherited)
/// and the run record; collect errors/skips, doctor fixtures, and warnings
/// accumulate. Scheduling / lazy events are no-ops in a single session —
/// enumerated (not `_`) so a new event type forces a decision here.
fn fold_run_event(
    event: proto::Event,
    passthrough: bool,
    run: &mut report::Run,
    prog: &mut progress::Progress,
    fixtures: &mut Vec<proto::FixtureStat>,
    warnings: &mut Vec<proto::WarningEntry>,
    sink: &mut Sink,
) -> Option<i32> {
    match event {
        proto::Event::Report(r) => {
            if !passthrough {
                prog.on_report(sink, None, &r);
            }
            sink.emit_report(None, &r);
            run.record(None, r);
            None
        }
        proto::Event::CollectError { path, longrepr } => {
            prog.on_collect_error(sink, &path, &longrepr);
            sink.emit_collect_error(&path, &longrepr);
            run.collect_error(path, longrepr);
            None
        }
        proto::Event::CollectSkip { .. } => {
            run.collect_skips += 1;
            None
        }
        proto::Event::DoctorFixtures { fixtures: fx } => {
            fixtures.extend(fx);
            None
        }
        proto::Event::Warnings { entries } => {
            warnings.extend(entries);
            None
        }
        proto::Event::CollectionDone { .. }
        | proto::Event::NodeInput { .. }
        | proto::Event::ItemStart { .. }
        | proto::Event::ItemDone { .. }
        | proto::Event::Stopped { .. }
        | proto::Event::LazyReady { .. }
        | proto::Event::FileCollected { .. }
        | proto::Event::ItemStartId { .. }
        | proto::Event::ItemDoneId { .. }
        | proto::Event::StoppedIds { .. } => None,
        proto::Event::Done { exitstatus } => Some(exitstatus),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        attach_stream_json, cap_workers_by_files, cap_workers_by_time, collect_lazy,
        fold_run_event, head_to_none, lazy_should_steal, parse_numprocesses, resolve_changed_base,
        resolve_shard, resolve_shuffle_seed, validate_cache_flags, warn_incremental_conflicts,
        warn_quarantine_passthrough, warn_windows_timeout, watchdog_duration,
    };
    use crate::cli::Cli;
    use crate::config::RstestSettings;
    use crate::reporting::sink::Sink;
    use crate::reporting::{progress, report};
    use crate::scheduling::proto;
    use clap::Parser;

    fn cli() -> Cli {
        Cli::parse_from(["rstest"])
    }

    #[test]
    fn head_to_none_maps_head_to_working_tree() {
        // HEAD is the "diff the working tree" sentinel => None for the git helpers.
        assert_eq!(head_to_none("HEAD"), None);
        assert_eq!(head_to_none("origin/main"), Some("origin/main"));
        assert_eq!(head_to_none("HEAD~3"), Some("HEAD~3"));
    }

    fn timeout_warning(
        is_windows: bool,
        timeout: Option<f64>,
        worker_timeout: Option<u64>,
    ) -> String {
        let mut buf = Vec::new();
        warn_windows_timeout(&mut buf, is_windows, timeout, worker_timeout);
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn warn_windows_timeout_fires_only_when_unbacked_on_windows() {
        // Windows + --timeout + no --worker-timeout: the one case that warns.
        let msg = timeout_warning(true, Some(1.0), None);
        assert!(msg.contains("can't interrupt a blocked test in-process on Windows"));
        assert!(msg.contains("--worker-timeout"));
        // Same platform, but an explicit --worker-timeout backstop => silent.
        assert_eq!(timeout_warning(true, Some(1.0), Some(5)), "");
        // No --timeout requested => nothing to warn about.
        assert_eq!(timeout_warning(true, None, None), "");
        // Off Windows: SIGALRM works, so no warning regardless of flags.
        assert_eq!(timeout_warning(false, Some(1.0), None), "");
    }

    #[test]
    fn watchdog_explicit_worker_timeout_wins() {
        // Explicit --worker-timeout always wins, ignoring --timeout.
        assert_eq!(
            watchdog_duration(Some(30), Some(2.0)),
            Some(std::time::Duration::from_secs(30))
        );
        assert_eq!(
            watchdog_duration(Some(30), None),
            Some(std::time::Duration::from_secs(30))
        );
    }

    #[test]
    fn watchdog_auto_arms_from_positive_timeout() {
        // Auto-arm at t*3 + 10 when only --timeout is set.
        assert_eq!(
            watchdog_duration(None, Some(2.0)),
            Some(std::time::Duration::from_secs_f64(16.0))
        );
    }

    #[test]
    fn watchdog_disabled_for_non_positive_or_non_finite_timeout() {
        // 0/negative/NaN/inf disable the auto-arm instead of panicking
        // from_secs_f64 (mirrors the worker's _parse_timeout).
        assert_eq!(watchdog_duration(None, None), None);
        assert_eq!(watchdog_duration(None, Some(0.0)), None);
        assert_eq!(watchdog_duration(None, Some(-4.0)), None);
        assert_eq!(watchdog_duration(None, Some(f64::NAN)), None);
        assert_eq!(watchdog_duration(None, Some(f64::INFINITY)), None);
    }

    #[test]
    fn watchdog_clamps_overflowing_timeout_instead_of_panicking() {
        // A finite-but-enormous --timeout must clamp to MAX, not panic.
        assert_eq!(
            watchdog_duration(None, Some(f64::MAX)),
            Some(std::time::Duration::MAX)
        );
    }

    #[test]
    fn parse_numprocesses_parses_and_rejects() {
        assert_eq!(parse_numprocesses("4").unwrap(), 4);
        assert_eq!(parse_numprocesses("0").unwrap(), 0);
        assert!(parse_numprocesses("abc").is_err());
        assert!(parse_numprocesses("-1").is_err());
    }

    #[test]
    fn resolve_changed_base_is_none_without_request() {
        // No --changed and no --changed-strict => no changed-selection, and
        // crucially no git shell-out (kept hermetic).
        assert!(resolve_changed_base(&cli(), &mut Sink::captured().0)
            .unwrap()
            .is_none());
    }

    fn settings_collect(mode: Option<&str>) -> RstestSettings {
        RstestSettings {
            collect: mode.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn collect_lazy_defaults_to_full() {
        // No CLI flag, no setting => "full" => not lazy.
        assert!(!collect_lazy(
            &cli(),
            &settings_collect(None),
            "load",
            &[],
            &mut Sink::captured().0
        )
        .unwrap());
    }

    #[test]
    fn collect_lazy_enabled_for_file_affine_dist() {
        let s = settings_collect(Some("lazy"));
        assert!(collect_lazy(&cli(), &s, "load", &[], &mut Sink::captured().0).unwrap());
        assert!(collect_lazy(&cli(), &s, "loadfile", &[], &mut Sink::captured().0).unwrap());
    }

    #[test]
    fn collect_lazy_rejects_incompatible_dist() {
        let s = settings_collect(Some("lazy"));
        // loadscope/loadgroup need a global id list; lazy is file-affine.
        assert!(collect_lazy(&cli(), &s, "loadscope", &[], &mut Sink::captured().0).is_err());
        assert!(collect_lazy(&cli(), &s, "loadgroup", &[], &mut Sink::captured().0).is_err());
    }

    #[test]
    fn collect_lazy_falls_back_on_nodeid_or_pyargs() {
        let s = settings_collect(Some("lazy"));
        // Explicit nodeid selection can't ride the file walk => full.
        assert!(!collect_lazy(
            &cli(),
            &s,
            "load",
            &["test_x.py::test_a".to_string()],
            &mut Sink::captured().0
        )
        .unwrap());
        // --pyargs selects by import path => full.
        assert!(!collect_lazy(
            &cli(),
            &s,
            "load",
            &["--pyargs".to_string()],
            &mut Sink::captured().0
        )
        .unwrap());
    }

    #[test]
    fn collect_lazy_rejects_unknown_mode() {
        assert!(collect_lazy(
            &cli(),
            &settings_collect(Some("sometimes")),
            "load",
            &[],
            &mut Sink::captured().0
        )
        .is_err());
    }

    #[test]
    fn validate_cache_flags_requires_remote() {
        // No cache flags => always fine, remote or not.
        assert!(validate_cache_flags(false, false, false).is_ok());
        // A cache action with a resolved remote => fine.
        assert!(validate_cache_flags(true, false, true).is_ok());
        // A cache action with no remote => hard error.
        let err = validate_cache_flags(false, true, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("need --cache-remote"), "got {err}");
    }

    #[test]
    fn resolve_shuffle_seed_none_and_happy_path() {
        // No flag => no seed, no error.
        assert_eq!(
            resolve_shuffle_seed(
                None,
                4,
                false,
                false,
                false,
                "load",
                &mut Sink::captured().0
            )
            .unwrap(),
            None
        );
        // A numeric seed parses through on the parallel pool.
        assert_eq!(
            resolve_shuffle_seed(
                Some("42"),
                4,
                false,
                false,
                false,
                "load",
                &mut Sink::captured().0
            )
            .unwrap(),
            Some(42)
        );
        // `random` yields *some* seed (nondeterministic value).
        assert!(resolve_shuffle_seed(
            Some("random"),
            4,
            false,
            false,
            false,
            "load",
            &mut Sink::captured().0
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn resolve_shuffle_seed_rejects_incompatible_modes() {
        // Single-worker rerun pool: its own tailored message.
        assert!(resolve_shuffle_seed(
            Some("1"),
            1,
            false,
            true,
            false,
            "load",
            &mut Sink::captured().0
        )
        .unwrap_err()
        .to_string()
        .contains("one-worker rerun pool"));
        // Plain single-worker / passthrough.
        assert!(resolve_shuffle_seed(
            Some("1"),
            1,
            false,
            false,
            false,
            "load",
            &mut Sink::captured().0
        )
        .unwrap_err()
        .to_string()
        .contains("needs the parallel pool"));
        assert!(resolve_shuffle_seed(
            Some("1"),
            4,
            true,
            false,
            false,
            "load",
            &mut Sink::captured().0
        )
        .is_err());
        // Lazy collection and --dist each are unsupported.
        assert!(resolve_shuffle_seed(
            Some("1"),
            4,
            false,
            false,
            true,
            "load",
            &mut Sink::captured().0
        )
        .unwrap_err()
        .to_string()
        .contains("--collect lazy"));
        assert!(resolve_shuffle_seed(
            Some("1"),
            4,
            false,
            false,
            false,
            "each",
            &mut Sink::captured().0
        )
        .unwrap_err()
        .to_string()
        .contains("--dist each"));
        // A non-numeric seed is rejected.
        assert!(resolve_shuffle_seed(
            Some("abc"),
            4,
            false,
            false,
            false,
            "load",
            &mut Sink::captured().0
        )
        .unwrap_err()
        .to_string()
        .contains("must be an unsigned integer"));
    }

    #[test]
    fn resolve_shard_none_noop_and_happy_path() {
        assert_eq!(
            resolve_shard(None, 4, false, false, false, "load").unwrap(),
            None
        );
        // 1/1 is the whole suite => no filter.
        assert_eq!(
            resolve_shard(Some("1/1"), 4, false, false, false, "load").unwrap(),
            None
        );
        assert_eq!(
            resolve_shard(Some("2/3"), 4, false, false, false, "load").unwrap(),
            Some((2, 3))
        );
    }

    #[test]
    fn resolve_shard_rejects_incompatible_modes() {
        assert!(resolve_shard(Some("2/3"), 1, false, true, false, "load")
            .unwrap_err()
            .to_string()
            .contains("one-worker rerun pool"));
        assert!(resolve_shard(Some("2/3"), 1, false, false, false, "load")
            .unwrap_err()
            .to_string()
            .contains("needs the parallel pool"));
        // --shuffle active: shards must be machine-stable.
        assert!(resolve_shard(Some("2/3"), 4, false, false, true, "load")
            .unwrap_err()
            .to_string()
            .contains("not supported with --shuffle"));
        assert!(resolve_shard(Some("2/3"), 4, false, false, false, "each")
            .unwrap_err()
            .to_string()
            .contains("--dist each"));
    }

    #[test]
    fn warn_incremental_conflicts_picks_the_first_applicable_note() {
        let note = |inc, sg, ch, active| {
            let mut buf = Vec::new();
            warn_incremental_conflicts(&mut buf, inc, sg, ch, active);
            String::from_utf8(buf).unwrap()
        };
        // --since-green wins first.
        assert!(note(true, true, true, false).contains("--since-green takes precedence"));
        // Then --changed.
        assert!(note(true, false, true, false).contains("--changed owns selection"));
        // Then the not-active fallback.
        assert!(note(true, false, false, false).contains("needs the parallel pool"));
        // Active + no conflict => silent; --incremental off => silent.
        assert!(note(true, false, false, true).is_empty());
        assert!(note(false, true, true, false).is_empty());
    }

    #[test]
    fn warn_quarantine_passthrough_writes_the_note() {
        let mut buf = Vec::new();
        warn_quarantine_passthrough(&mut buf);
        assert!(String::from_utf8(buf)
            .unwrap()
            .contains("--quarantine has no effect in passthrough mode"));
    }

    #[test]
    fn lazy_should_steal_only_on_explicit_load() {
        assert!(lazy_should_steal(Some("load"), None));
        assert!(lazy_should_steal(None, Some("load")));
        // Default (no explicit load) keeps strict file affinity.
        assert!(!lazy_should_steal(None, None));
        assert!(!lazy_should_steal(Some("loadfile"), Some("loadscope")));
    }

    #[test]
    fn cap_workers_helpers_shrink_but_never_below_one() {
        // Files: never more workers than files; an empty walk is a no-op.
        assert_eq!(cap_workers_by_files(8, 3), 3);
        assert_eq!(cap_workers_by_files(8, 0), 8);
        assert_eq!(cap_workers_by_files(2, 5), 2);
        // Time: ~2s per worker, floored at 1.
        assert_eq!(cap_workers_by_time(8, 10.0), 5); // ceil(10/2)=5
        assert_eq!(cap_workers_by_time(8, 1.0), 1); // ceil(0.5)=1, max(1)
        assert_eq!(cap_workers_by_time(8, 0.0), 1); // never below 1
    }

    fn report(nodeid: &str, outcome: &str) -> proto::Report {
        proto::Report {
            nodeid: nodeid.into(),
            when: "call".into(),
            outcome: outcome.into(),
            duration: 0.1,
            longrepr: None,
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            sections: Vec::new(),
            lineno: None,
            thread_delta: None,
            fd_delta: None,
        }
    }

    #[test]
    fn fold_run_event_records_reports_errors_and_terminates_on_done() {
        let mut run = report::Run::default();
        let mut prog = progress::Progress::default();
        let mut fixtures = Vec::new();
        let mut warnings = Vec::new();
        let (mut sink, _cap) = Sink::captured();
        let mut fold = |ev| {
            fold_run_event(
                ev,
                false,
                &mut run,
                &mut prog,
                &mut fixtures,
                &mut warnings,
                &mut sink,
            )
        };

        assert_eq!(
            fold(proto::Event::Report(report("t.py::a", "passed"))),
            None
        );
        assert_eq!(
            fold(proto::Event::CollectError {
                path: "bad.py".into(),
                longrepr: "boom".into(),
            }),
            None
        );
        assert_eq!(
            fold(proto::Event::CollectSkip {
                path: "m.py".into()
            }),
            None
        );
        assert_eq!(
            fold(proto::Event::DoctorFixtures {
                fixtures: vec![proto::FixtureStat {
                    name: "db".into(),
                    scope: "session".into(),
                    count: 1,
                    total: 0.5,
                }]
            }),
            None
        );
        assert_eq!(
            fold(proto::Event::Warnings {
                entries: vec![proto::WarningEntry {
                    when: "runtest".into(),
                    category: "DeprecationWarning".into(),
                    message: "old".into(),
                    filename: "t.py".into(),
                    lineno: 1,
                    count: 1,
                }]
            }),
            None
        );
        // A scheduling-only event is a no-op in a single session.
        assert_eq!(fold(proto::Event::ItemStart { index: 0 }), None);
        // Done terminates with the exit status.
        assert_eq!(fold(proto::Event::Done { exitstatus: 1 }), Some(1));

        assert_eq!(run.collect_skips, 1);
        assert_eq!(warnings.len(), 1);
        assert_eq!(fixtures.len(), 1);
    }

    #[test]
    fn fold_run_event_suppresses_progress_under_passthrough() {
        // Passthrough owns the tty; a Report must still be recorded but not
        // drive the progress renderer.
        let mut run = report::Run::default();
        let mut prog = progress::Progress::default();
        let mut fixtures = Vec::new();
        let mut warnings = Vec::new();
        let code = fold_run_event(
            proto::Event::Report(report("t.py::a", "passed")),
            true,
            &mut run,
            &mut prog,
            &mut fixtures,
            &mut warnings,
            &mut Sink::captured().0,
        );
        assert_eq!(code, None);
        assert_eq!(run.counts()["passed"], 1);
    }

    #[test]
    fn fold_run_event_streams_reports_and_collect_errors() {
        // With a --stream-json sink attached, a Report and a CollectError each
        // emit one NDJSON line on the side channel (the wiring behind the live
        // Test Explorer feed for the single/passthrough path).
        let mut run = report::Run::default();
        let mut prog = progress::Progress::default();
        let mut fixtures = Vec::new();
        let mut warnings = Vec::new();
        let (mut sink, _cap) = Sink::captured();
        let stream = sink.attach_captured_stream();
        let mut fold = |ev, sink: &mut Sink| {
            fold_run_event(
                ev,
                false,
                &mut run,
                &mut prog,
                &mut fixtures,
                &mut warnings,
                sink,
            )
        };
        fold(proto::Event::Report(report("t.py::a", "passed")), &mut sink);
        fold(
            proto::Event::CollectError {
                path: "bad.py".into(),
                longrepr: "boom".into(),
            },
            &mut sink,
        );

        let text = String::from_utf8(stream.lock().unwrap().clone()).unwrap();
        let events: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], "testreport");
        assert_eq!(events[0]["nodeid"], "t.py::a");
        assert_eq!(events[1]["event"], "collecterror");
        assert_eq!(events[1]["path"], "bad.py");
        assert_eq!(events[1]["longrepr"], "boom");
    }

    #[test]
    fn attach_stream_json_opens_file_and_streams_events() {
        // Happy path: the file opens, gets attached, and emitted NDJSON lands in
        // it (truncating whatever was there before).
        let path =
            std::env::temp_dir().join(format!("rstest-streamjson-{}.ndjson", std::process::id()));
        std::fs::write(&path, b"stale contents that must be truncated\n").unwrap();

        let (mut sink, captured) = Sink::captured();
        attach_stream_json(&mut sink, &path);
        // Open failure would warn to stderr; success must not.
        assert_eq!(captured.err(), "");
        sink.emit_event(serde_json::json!({"event": "sessionfinish"}));

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "{\"event\":\"sessionfinish\"}\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn attach_stream_json_warns_when_open_fails() {
        // A path under a nonexistent directory can't be created => warn, no panic.
        let path = std::env::temp_dir()
            .join(format!("rstest-streamjson-missing-{}", std::process::id()))
            .join("nope")
            .join("out.ndjson");

        let (mut sink, captured) = Sink::captured();
        attach_stream_json(&mut sink, &path);

        let err = captured.err();
        assert!(err.contains("rstest: --stream-json"), "got: {err}");
        assert!(err.contains(&path.display().to_string()), "got: {err}");
        // Nothing was attached, so emitting is a no-op (no panic writing to a
        // closed/absent stream).
        sink.emit_event(serde_json::json!({"event": "sessionfinish"}));
    }
}
