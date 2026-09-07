//! Run orchestration: the single-project pipeline (`execute`) and its helpers.
//! `execute` is the entry point `main` and `watch` call. The monorepo driver
//! lives in [`monorepo`], the post-run gates/reports in [`gates`], and the
//! `--collect-only` discovery doc in [`discovery`].

mod discovery;
mod gates;
mod monorepo;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::cli::{is_collect_only, needs_passthrough_io, parse_durations, parse_maxfail, Cli};
use crate::reporting::{color, flakes, progress, report};
use crate::scheduling::{durations, lazy, pool, proto, shard, worker};
use crate::{
    collect, config, coverage_skip, discover, doctor, incremental, migrate, mono, remote, select,
};

/// Resolve the effective `--changed` base rev: the flag's value, or `HEAD` when
/// `--changed-strict` implies it, run through git rev resolution. `None` = no
/// changed-selection requested.
fn resolve_changed_base(cli: &Cli) -> Result<Option<String>> {
    cli.changed
        .clone()
        .or_else(|| cli.changed_strict.then(|| "HEAD".to_string()))
        .map(|rev| select::resolve_base_rev(&rev))
        .transpose()
}

/// `HEAD` means "diff the working tree" (no explicit rev); any other rev is the
/// diff base. The git-diff helpers take `Option<&str>` with that convention.
fn head_to_none(rev: &str) -> Option<&str> {
    (rev != "HEAD").then_some(rev)
}

/// `--changed`/`--since-green` selection: resolve the effective diff base
/// (`--since-green`'s last-green baseline overrides the `--changed-strict` HEAD
/// implication), then narrow `args` to the affected test targets. Returns the
/// original `args` unchanged when no changed-selection is requested or the diff
/// falls back to a full run. May `exit()` when nothing is affected (advancing
/// the green baseline first under `--since-green`). `since_green`/`head`/`env_fp`
/// are computed by the caller (they outlive selection, feeding the post-run
/// green-baseline record).
fn apply_selection(
    cli: &Cli,
    mut args: Vec<String>,
    since_green: bool,
    head: &Option<String>,
    env_fp: &str,
) -> Result<Vec<String>> {
    let mut effective_changed = resolve_changed_base(cli)?;
    if since_green {
        // --since-green owns the diff base: its last-green baseline drives
        // selection, OVERRIDING the "HEAD" base that --changed-strict would
        // otherwise imply (changed_strict is a gating modifier here, not a base;
        // an explicit --changed is already excluded by `since_green`). No
        // baseline yet -> a full run to establish one.
        match incremental::baseline(&std::env::current_dir()?, env_fp) {
            Some(sha) => {
                eprintln!(
                    "rstest: --since-green: selecting changes since last green run ({})",
                    &sha[..sha.len().min(12)]
                );
                effective_changed = Some(sha);
            }
            None => {
                eprintln!(
                    "rstest: --since-green: no prior green run recorded; \
                     running everything to establish the baseline"
                );
                effective_changed = None;
            }
        }
    }
    if let Some(rev) = &effective_changed {
        let rev = head_to_none(rev);
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
                // nothing-collected code), even under --since-green.
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
    Ok(args)
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
    palette: color::Palette,
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
    warn_windows_timeout(
        &mut std::io::stderr(),
        cfg!(windows),
        cli.timeout,
        worker_timeout,
    );
    let n = parse_numprocesses(&numprocesses)?;
    let passthrough = needs_passthrough_io(args);
    // Honor `--reruns` in single-worker mode via a degenerate one-worker pool:
    // the rerun loop is orchestrator-side (rerunfailures neutralized inside).
    // Passthrough can't be pooled, so reruns stay inert there.
    let single_worker_reruns = reruns > 0 && n <= 1 && !passthrough;
    // A one-worker rerun pool is 1 worker everywhere downstream (banner,
    // doctor, report-json meta), never 0.
    let n = if single_worker_reruns { 1 } else { n };
    let palette = color::Palette::detect(args);
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
    let durations = parse_durations(args);
    // Validate `--doctor-fail-on` conditions up front: a typo'd metric or a
    // missing operator aborts now, never silently as a gate that can't fire.
    let doctor_gate = doctor::parse_conditions(&cli.doctor_fail_on)?;
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
        palette,
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

/// The crate's main entry point for a single (non-watch) run: resolves the
/// run configuration from `cli` + forwarded pytest `args`, dispatches to the
/// worker pool (or the monorepo driver), runs post-run reports and gates
/// (doctor, junit, lastfailed, duration-regression, cache push, report-json),
/// and returns the process exit status.
pub fn execute(cli: &Cli, args: &[String]) -> Result<i32> {
    let args = args.to_vec();
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
    if (cli.cache_pull || cli.cache_push || cli.cache_compact) && cache_remote.is_none() {
        anyhow::bail!(
            "--cache-pull/--cache-push/--cache-compact need --cache-remote \
             (or RSTEST_CACHE_REMOTE)"
        );
    }
    // --cache-compact is a run-less maintenance mode that exits before the run;
    // combining it with the run-time cache flags would silently skip them (and
    // the tests), reporting green having done neither. Reject the combination.
    if cli.cache_compact && (cli.cache_pull || cli.cache_push) {
        anyhow::bail!(
            "--cache-compact is a run-less maintenance mode; run it on its own, \
             not combined with --cache-pull/--cache-push"
        );
    }
    if cli.cache_compact {
        let remote = cache_remote.as_deref().unwrap(); // validated above
        let t = remote::transport_for(remote)?;
        let folded = remote::compact_remote(t.as_ref())
            .with_context(|| format!("compacting shared cache at {remote}"))?;
        eprintln!("rstest: cache: compacted {folded} segment(s) into base at {remote}");
        return Ok(0);
    }
    // An explicit --cache-remote FLAG with no pull/push/compact does nothing;
    // warn rather than silently ignore it. Gate on the flag, NOT the env-resolved
    // value: RSTEST_CACHE_REMOTE is ambient config a CI sets once, and plain runs
    // that don't opt into pull/push must not be nagged every invocation.
    // (cache_compact already returned above, so it can't be the requested action.)
    if cli.cache_remote.is_some() && !cli.cache_pull && !cli.cache_push {
        eprintln!(
            "rstest: cache: --cache-remote is set but no --cache-pull/--cache-push \
             (or --cache-compact) was requested; the shared cache is not being used"
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
                // Each project keeps its OWN .rstest_cache (cache::file_in), and
                // the per-run push/pull wiring lives in the single-project path
                // that execute_monorepo bypasses — so a cache flag here would
                // silently no-op (push) or warm the wrong root cache (pull).
                // Fail loud instead; run rstest per project for shared caching.
                if cli.cache_pull || cli.cache_push {
                    anyhow::bail!(
                        "--cache-pull/--cache-push are not supported in monorepo mode \
                         (each project has its own .rstest_cache); run rstest per project"
                    );
                }
                return monorepo::execute_monorepo(cli, &args, &cwd, projects, &run_uid);
            }
        }
    }
    // --cache-pull: warm the local cache from the remote BEFORE anything reads
    // it (scheduling, selection, the regression baseline). Placed after the
    // monorepo guard so a monorepo run is rejected rather than pulling into the
    // wrong (root) cache and printing a misleading success line first.
    if cli.cache_pull {
        let remote = cache_remote.as_deref().unwrap(); // validated at entry
        let t = remote::transport_for(remote)?;
        let merged = remote::pull(t.as_ref())
            .with_context(|| format!("pulling shared cache from {remote}"))?;
        eprintln!(
            "rstest: cache: pulled {} duration(s), {} flake record(s) from {remote}",
            merged.durations.len(),
            merged.flakes.len()
        );
        remote::write_local(&merged);
    }
    let cfg = resolve_run_config(cli, &settings, &args, &run_uid)?;
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
        palette,
        very_verbose,
        mode,
        durations,
        ..
    } = cfg;
    let numprocesses = cfg.numprocesses.clone();
    let dist_name = cfg.dist_name.clone();
    let scope = cfg.scope.clone();
    let python = cfg.python.clone();
    // Run-less: verify the vendored pytest tree against the packaged manifest.
    if cli.verify_vendor {
        return crate::vendor::run_verify(&python);
    }
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
            let code = discovery::run_collect_discovery(&python, &args, out, &run_uid)?;
            std::process::exit(code);
        }
    }
    // require-baseline: with the durations-regress gate active, an absent baseline
    // (cold remote, nothing restored/pulled) is a hard error rather than the silent
    // skip the gate would otherwise do — the dead-gate guard. Placed after the
    // non-gating early exits (monorepo, migrate-check, collect-only) and gated on
    // !passthrough, since the regression gate only runs on a real in-process run;
    // pull (above) has already warmed the baseline it checks.
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
    // Narrow args to the affected test targets (may exit if nothing is affected).
    let args = apply_selection(cli, args, since_green, &head, &env_fp)?;
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
                crate::time::now_epoch_nanos() as u64 ^ u64::from(std::process::id())
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
    // --incremental: compute the dispatch-level skip set now (before the pool
    // collects) from the coverage index + last green set, gated by a config
    // fingerprint. Restricted to the eager pool on --dist load with full
    // collection; incompatible modes disable it with a note. An explicit
    // --changed (like --since-green) already narrows selection, so dispatch-level
    // skipping on top is excluded — it would record a PARTIAL green baseline and
    // count cached passes against a partial collection.
    let incremental_active = cli.incremental
        && !since_green
        && cli.changed.is_none()
        && dist_name == "load"
        && !passthrough
        && n >= 2
        && shard.is_none()
        && shuffle_seed.is_none()
        && !collect_lazy(cli, &settings, &dist_name, &args)?;
    // The config fingerprint is only consumed under `incremental_active` (the
    // skip-set load and the green-baseline record). Computing it unconditionally
    // would walk the whole project tree for conftests even when the feature is
    // off (e.g. -n 1, --collect lazy), so gate it on the same condition.
    let config_fp = if incremental_active {
        coverage_skip::config_fingerprint(&scope)
    } else {
        String::new()
    };
    if cli.incremental && since_green {
        // Both incremental modes select on the same run; --since-green already
        // narrows to the changed subset, so dispatch-level skipping on top would
        // account the cached passes against a partial suite. --since-green wins.
        eprintln!(
            "rstest: --incremental and --since-green are mutually exclusive; \
             --since-green takes precedence this run"
        );
    } else if cli.incremental && cli.changed.is_some() {
        // Explicit --changed owns selection: it narrows args to the changed
        // subset, so dispatch-level skipping on top would clobber the full green
        // baseline with a partial one and report cached passes against a partial
        // collection. --changed wins this run.
        eprintln!(
            "rstest: --incremental and --changed are mutually exclusive; \
             --changed owns selection this run"
        );
    } else if cli.incremental && !incremental_active {
        eprintln!(
            "rstest: --incremental needs the parallel pool with full collection and \
             --dist load (not -n 0/1, --dist each/affinity, --collect lazy, --shard, or \
             --shuffle); running everything this time"
        );
    }
    // --incremental relies on the coverage index advancing every run; without
    // --cov this run covtool never rewrites it, so a changed test re-runs on
    // every invocation until a coverage run refreshes the index.
    if incremental_active && !coverage_skip::coverage_requested(&args) {
        eprintln!(
            "rstest: --incremental without --cov: the coverage index won't be \
             refreshed this run, so changed tests keep re-running until a --cov run"
        );
    }
    // A narrowed --cov=<pkg> makes first-party source OUTSIDE the scope
    // coverage-invisible: editing it won't bust the skip, so a test depending on
    // it can be wrongly cached (stale false-green). Warn; --cov=. closes the gap.
    if incremental_active && coverage_skip::cov_scope_narrowed(&args) {
        eprintln!(
            "rstest: --incremental with a scoped --cov: edits to first-party source \
             outside the coverage scope are undetectable and may leave a test cached \
             on a stale pass; use --cov=. to cover the whole tree"
        );
    }
    // Snapshot the index BEFORE the run: it drives the skip decision now, and
    // post-run it supplies the cached tests' coverage to fold back in (covtool
    // rewrites the index from only the tests that ran).
    let prev_index = if incremental_active {
        remote::load_local_cov_index()
    } else {
        select::CoverageIndex::default()
    };
    // The baseline is loaded once and kept: it drives the skip set now, and its
    // recorded def lines restore the cached (not-run) entries' source line after
    // the run (a cached test has no pytest report to supply one).
    let baseline = if incremental_active {
        coverage_skip::load(&scope, &config_fp)
    } else {
        coverage_skip::Baseline::default()
    };
    let skip_ids: std::collections::HashSet<String> = if incremental_active {
        coverage_skip::skippable_now(&prev_index, &baseline)
    } else {
        std::collections::HashSet::new()
    };
    let mut outcome = dispatch_run(&cfg, cli, &settings, &args, &skip_ids, shuffle_seed, shard)?;

    // Quarantine BEFORE any output or exit-code consumer: classification,
    // counts, junit, report-json, and the sessionfinish envelope must all
    // see the demoted outcomes consistently.
    if let Some(qpath) = &cli.quarantine {
        if passthrough {
            eprintln!("rstest: --quarantine has no effect in passthrough mode; ignoring");
        } else {
            let matcher = gates::quarantine_matcher(qpath)?;
            let demoted = outcome.run.quarantine(|id| matcher.is_match(id));
            // pytest exit 1 = tests failed; if every failure was
            // quarantined the run is green by policy. Exit codes 2+
            // (usage/internal errors) are never touched.
            if !demoted.is_empty() && outcome.exitstatus == 1 && outcome.run.all_passed() {
                outcome.exitstatus = 0;
            }
        }
    }
    gates::finalize_output(
        &mut outcome,
        passthrough,
        mode,
        palette,
        durations,
        very_verbose,
        start,
    );

    let post = PostRun {
        start,
        started_epoch,
        run_uid: &run_uid,
        cache_remote: cache_remote.as_deref(),
        shard,
        since_green,
        head: &head,
        env_fp: &env_fp,
        incremental_active,
        config_fp: &config_fp,
        prev_index: &prev_index,
        baseline: &baseline,
    };
    gates::run_post_gates(&cfg, cli, &mut outcome, &args, &post)
}

fn dispatch_run(
    cfg: &RunConfig,
    cli: &Cli,
    settings: &config::RstestSettings,
    args: &[String],
    skip_ids: &std::collections::HashSet<String>,
    shuffle_seed: Option<u64>,
    shard: Option<(usize, usize)>,
) -> Result<pool::PoolOutcome> {
    let RunConfig {
        n,
        reruns,
        worker_timeout,
        passthrough,
        single_worker_reruns,
        palette,
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
        palette,
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
                | proto::Event::StoppedIds { .. }
                | proto::Event::ServeReady { .. }
                | proto::Event::ServeReport { .. }
                | proto::Event::ServeRunDone { .. } => {}
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
    } else if collect_lazy(cli, settings, dist_name, args)? {
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
            cli.dist.as_deref() == Some("load") || settings.dist.as_deref() == Some("load"),
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

/// Warn (once, to `w`) when `--timeout` is asked for on Windows: interrupting a
/// blocked test in-process needs SIGALRM firing inside the stuck syscall, which
/// Windows lacks, so the per-test deadline can't be enforced. Silent when the
/// user already set `--worker-timeout` (they have an explicit hang backstop) or
/// off Windows. Takes `is_windows` as a param (not `cfg!`) so both branches are
/// exercised under coverage on any host.
fn warn_windows_timeout(
    w: &mut impl std::io::Write,
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

#[cfg(test)]
mod tests {
    use super::{
        collect_lazy, head_to_none, parse_numprocesses, resolve_changed_base, warn_windows_timeout,
        watchdog_duration,
    };
    use crate::cli::Cli;
    use crate::config::RstestSettings;
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
        assert!(resolve_changed_base(&cli()).unwrap().is_none());
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
        assert!(!collect_lazy(&cli(), &settings_collect(None), "load", &[]).unwrap());
    }

    #[test]
    fn collect_lazy_enabled_for_file_affine_dist() {
        let s = settings_collect(Some("lazy"));
        assert!(collect_lazy(&cli(), &s, "load", &[]).unwrap());
        assert!(collect_lazy(&cli(), &s, "loadfile", &[]).unwrap());
    }

    #[test]
    fn collect_lazy_rejects_incompatible_dist() {
        let s = settings_collect(Some("lazy"));
        // loadscope/loadgroup need a global id list; lazy is file-affine.
        assert!(collect_lazy(&cli(), &s, "loadscope", &[]).is_err());
        assert!(collect_lazy(&cli(), &s, "loadgroup", &[]).is_err());
    }

    #[test]
    fn collect_lazy_falls_back_on_nodeid_or_pyargs() {
        let s = settings_collect(Some("lazy"));
        // Explicit nodeid selection can't ride the file walk => full.
        assert!(!collect_lazy(&cli(), &s, "load", &["test_x.py::test_a".to_string()]).unwrap());
        // --pyargs selects by import path => full.
        assert!(!collect_lazy(&cli(), &s, "load", &["--pyargs".to_string()]).unwrap());
    }

    #[test]
    fn collect_lazy_rejects_unknown_mode() {
        assert!(collect_lazy(&cli(), &settings_collect(Some("sometimes")), "load", &[]).is_err());
    }
}
