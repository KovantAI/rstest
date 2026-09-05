//! Run orchestration: the single-project pipeline (`execute`), the monorepo
//! driver (`execute_monorepo`), and their helpers. `execute` is the entry
//! point `main` and `watch` call.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::cli::{is_collect_only, needs_passthrough_io, parse_durations, parse_maxfail, Cli};
use crate::reporting::ci::{
    buildkite_flaky_annotate, print_azure_annotations, print_github_annotations,
};
use crate::reporting::{color, flakes, html, junit, progress, report, status};
use crate::scheduling::{durations, lazy, pool, proto, shard, worker};
use crate::{cache, collect, config, discover, doctor, migrate, mono, remote, select};

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
    run_uid: &str,
) -> Result<i32> {
    // The lone worker ships the full id+location payload from
    // `pytest_collection_finish` (single session, so no per-worker designate).
    let env = worker::WorkerEnv {
        run_uid: run_uid.to_string(),
        doctor: false,
        leakcheck: false,
        send_ids: true,
    };
    let mut w = worker::Worker::spawn_with_io(python, None, worker::Stdio::Null, &env)?;
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

/// Assemble the report-json run metadata; `duration_seconds`/`argv` are the same
/// for every run path, only exit status / start epoch / worker count vary.
fn build_run_meta(
    start: Instant,
    exitstatus: i32,
    started_at_epoch: u64,
    workers: usize,
) -> report::RunMeta {
    report::RunMeta {
        exitstatus,
        duration_seconds: start.elapsed().as_secs_f64(),
        started_at_epoch,
        workers,
        argv: std::env::args().collect(),
    }
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
                return execute_monorepo(cli, &args, &cwd, projects, &run_uid);
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
        run_uid: run_uid.clone(),
        doctor,
        leakcheck,
        send_ids: false,
    };

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
            let code = run_collect_discovery(&python, &args, out, &run_uid)?;
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
    let mut args = args;
    let effective_changed = resolve_changed_base(cli)?;
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
    let mut outcome = dispatch_run(
        cli,
        &settings,
        &python,
        &args,
        n,
        mode,
        palette,
        &dist_name,
        durations,
        reruns,
        worker_timeout,
        known_flaky.as_ref(),
        shuffle_seed,
        shard,
        passthrough,
        single_worker_reruns,
        &worker_env,
    )?;

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
    finalize_output(
        &mut outcome,
        passthrough,
        mode,
        palette,
        durations,
        very_verbose,
        start,
    );

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
    if let Some(path) = &cli.html {
        html::write(
            path,
            &outcome.run,
            &build_run_meta(start, outcome.exitstatus, started_epoch, n),
        )?;
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
        // Only write when serialization succeeds: a serialize error must not
        // clobber pytest's lastfailed cache with an empty `{}`.
        if let (Ok(()), Ok(bytes)) = (std::fs::create_dir_all(&dir), serde_json::to_vec(&failed)) {
            let _ = std::fs::write(dir.join("lastfailed"), bytes);
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
    // Before publishing, drop any coverage index left by --cache-pull: only an
    // index covtool writes for THIS run may be pushed. Unconditional on push (not
    // gated by --cov) so a run that produces no fresh index — no --cov at all, no
    // --cov-context, or an empty shard — pushes an empty slice rather than
    // re-publishing the pulled merged index as its own. Selection already
    // consumed the pulled index earlier, so removing it now is safe.
    if cli.cache_push {
        let _ = std::fs::remove_file(cache::file(select::COVERAGE_INDEX_FILE));
    }
    // Coverage: workers save suffixed data files (pytest-cov worker mode);
    // the orchestrator plays the xdist-master role, so combine and report.
    // Runs BEFORE the cache-push below so this run's coverage-index slice is
    // materialized (covtool overwrites the local index) in time to be published.
    let mut exitstatus = outcome.exitstatus;
    if !passthrough && args.iter().any(|a| a == "--cov" || a.starts_with("--cov=")) {
        println!();
        let status = std::process::Command::new(&python)
            .args(["-m", "rstest_worker.covtool"])
            .args(&args)
            .env("PYTHONPATH", worker::worker_pythonpath())
            // Same cache dir the Rust side reads (cache::dir()) so the index
            // lands where load_coverage_index / --cache-push look for it.
            .env("RSTEST_CACHE", cache::dir())
            .status();
        match status {
            Ok(s) if !s.success() && exitstatus == 0 => exitstatus = 1,
            Ok(_) => {}
            Err(e) => eprintln!("rstest: coverage reporting failed to run: {e}"),
        }
    }
    // Each-mode ids carry the [gwN] suffix and every test ran N times, so
    // they would poison the duration cache used for LPT scheduling.
    if dist_name != "each" {
        durations::save(&outcome.run);
        // Flake history rides the same cadence (and the same [gwN]-key
        // poisoning concern rules out each-mode).
        flakes::record(&outcome.run);
        // --cache-push: publish THIS run's contribution as one immutable
        // segment (from the in-memory Run, not the merged local cache). A push
        // failure warns but never fails an otherwise-green run.
        if cli.cache_push {
            let remote = cache_remote.as_deref().unwrap(); // validated at entry
            let uid = run_uid.clone();
            let shard_suffix = shard.map(|(k, n)| format!("-{k}of{n}")).unwrap_or_default();
            // This run's coverage slice (covtool wrote it just above); empty for
            // non-coverage runs. Published as the segment's cov_index.
            let cov = remote::load_local_cov_index();
            let seg = remote::segment_from_run(
                format!("{uid}{shard_suffix}"),
                started_epoch,
                &outcome.run,
                cov,
            );
            match remote::transport_for(remote).and_then(|t| remote::push(t.as_ref(), &seg)) {
                Ok(()) => eprintln!(
                    "rstest: cache: pushed segment ({} duration(s), {} event(s), {} covered file(s)) to {remote}",
                    seg.durations.len(),
                    seg.flake_events.len(),
                    seg.cov_index.files.len()
                ),
                Err(e) => eprintln!("rstest: cache: push failed: {e:#}"),
            }
        }
    }
    if let Some(path) = &cli.report_json {
        outcome.run.write_snapshot(
            path,
            &build_run_meta(start, outcome.exitstatus, started_epoch, n),
        )?;
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
    // --fail-on-leak: gate on any test that leaked a thread/fd. Printed on
    // stderr so --output json/tap keep stdout a pure machine stream.
    if cli.fail_on_leak && passthrough {
        // Passthrough (-s/--pdb/--co) has no worker instrumentation, so no
        // deltas are measured. Warn instead of silently exiting 0 (matches the
        // --quarantine passthrough behavior).
        eprintln!(
            "rstest: --fail-on-leak has no effect in passthrough mode \
             (-s/--pdb/--co); ignoring"
        );
    } else if cli.fail_on_leak {
        let leaks = doctor::detect_leaks(&outcome.run);
        if leaks.is_empty() {
            // Note the blind spot: the first test each worker runs is an
            // unchecked warm-up (first-touch imports aren't a per-test leak),
            // so a clean gate does not prove those tests are leak-free.
            eprintln!(
                "rstest: --fail-on-leak: no thread/fd leaks detected \
                 (first test per worker runs as an unchecked warm-up)"
            );
        } else {
            // Under --doctor the RESOURCE LEAKS section already listed these;
            // only gate + summarize here to avoid printing the table twice.
            if !doctor {
                eprintln!(
                    "\n{}",
                    palette.bold_red("=========== resource leaks ===========")
                );
                for l in leaks.iter().take(20) {
                    eprintln!("  {}  {}", doctor::leak_delta(l), l.nodeid);
                }
            }
            eprintln!(
                "rstest: --fail-on-leak: {} test(s) leaked threads/fds",
                leaks.len()
            );
            if exitstatus == 0 {
                exitstatus = 1;
            }
        }
    }
    Ok(exitstatus)
}

#[allow(clippy::too_many_arguments)]
fn dispatch_run(
    cli: &Cli,
    settings: &config::RstestSettings,
    python: &std::path::Path,
    args: &[String],
    n: usize,
    mode: progress::Mode,
    palette: color::Palette,
    dist_name: &str,
    durations: Option<(usize, f64)>,
    reruns: u32,
    worker_timeout: Option<u64>,
    known_flaky: Option<&std::collections::HashSet<String>>,
    shuffle_seed: Option<u64>,
    shard: Option<(usize, usize)>,
    passthrough: bool,
    single_worker_reruns: bool,
    worker_env: &worker::WorkerEnv,
) -> Result<pool::PoolOutcome> {
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
        lazy::run_lazy_pool(
            python,
            n.min(files.len().max(1)),
            args,
            files,
            mode,
            palette,
            // Steal (split files across workers) only on an EXPLICIT --dist
            // load: lazy defaults to strict file affinity, since stealing
            // exposes cross-file/in-file order dependence affinity doesn't.
            cli.dist.as_deref() == Some("load") || settings.dist.as_deref() == Some("load"),
            parse_maxfail(args),
            reruns,
            &cli.only_rerun
                .iter()
                .map(|p| regex::Regex::new(p))
                .collect::<Result<Vec<_>, _>>()?,
            worker_timeout.map(std::time::Duration::from_secs),
            known_flaky,
            worker_env,
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
            python,
            n,
            args,
            mode,
            durations.is_some(),
            palette,
            dist,
            parse_maxfail(args),
            reruns,
            &cli.only_rerun
                .iter()
                .map(|p| regex::Regex::new(p))
                .collect::<Result<Vec<_>, _>>()?,
            worker_timeout.map(std::time::Duration::from_secs),
            shuffle_seed,
            shard,
            known_flaky,
            worker_env,
        )?
    })
}

fn finalize_output(
    outcome: &mut pool::PoolOutcome,
    passthrough: bool,
    mode: progress::Mode,
    palette: color::Palette,
    durations: Option<(usize, f64)>,
    very_verbose: bool,
    start: Instant,
) {
    // Loaded before this run's events are recorded, so the history
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
}

/// Sequential session groups, one per subproject (monorepo P0).
fn execute_monorepo(
    cli: &Cli,
    args: &[String],
    root: &std::path::Path,
    projects: Vec<PathBuf>,
    run_uid: &str,
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
    let mono_changed = resolve_changed_base(cli)?;
    let impacts: Option<Vec<mono::ChangeImpact>> = match &mono_changed {
        Some(rev) => {
            let rev = head_to_none(rev);
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
            // Children inherit the root's run uid (one testrun), passed
            // explicitly rather than through the parent's process env.
            .env("RSTEST_RUN_UID", run_uid)
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
            cmd.arg("--report-json")
                .arg(report_part_path(&slug, run_uid));
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
            Some(report_part_path(&slug, run_uid)),
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
        let started_at_epoch =
            crate::time::now_epoch_secs().saturating_sub(start.elapsed().as_secs());
        let run_meta = build_run_meta(start, merged, started_at_epoch, budget);
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
fn report_part_path(slug: &str, run_uid: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rstest-mono-{run_uid}-{slug}.json"))
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

#[cfg(test)]
mod tests {
    use super::{
        build_run_meta, collect_lazy, head_to_none, merge_fixtures, parse_numprocesses,
        quarantine_matcher, report_part_path, resolve_changed_base, strip_verbatim,
    };
    use crate::cli::Cli;
    use crate::config::RstestSettings;
    use crate::scheduling::proto::FixtureStat;
    use clap::Parser;
    use std::time::Instant;

    fn cli() -> Cli {
        Cli::parse_from(["rstest"])
    }

    #[test]
    fn strip_verbatim_removes_windows_extended_prefix() {
        // The `\\?\` extended-length prefix is dropped so paths render as
        // editor-usable URIs; anything else is returned untouched.
        assert_eq!(
            strip_verbatim(r"\\?\C:\foo\bar".into()),
            std::path::PathBuf::from(r"C:\foo\bar")
        );
        assert_eq!(
            strip_verbatim("/home/u/proj".into()),
            std::path::PathBuf::from("/home/u/proj")
        );
        // A `?` that is not the exact prefix must not be stripped.
        assert_eq!(
            strip_verbatim("a/?b".into()),
            std::path::PathBuf::from("a/?b")
        );
    }

    #[test]
    fn head_to_none_maps_head_to_working_tree() {
        // HEAD is the "diff the working tree" sentinel => None for the git helpers.
        assert_eq!(head_to_none("HEAD"), None);
        assert_eq!(head_to_none("origin/main"), Some("origin/main"));
        assert_eq!(head_to_none("HEAD~3"), Some("HEAD~3"));
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

    #[test]
    fn build_run_meta_passes_through_fields() {
        let m = build_run_meta(Instant::now(), 7, 1_700_000_000, 4);
        assert_eq!(m.exitstatus, 7);
        assert_eq!(m.workers, 4);
        assert_eq!(m.started_at_epoch, 1_700_000_000);
        assert!(m.duration_seconds >= 0.0);
        assert!(!m.argv.is_empty());
    }

    #[test]
    fn merge_fixtures_sums_by_name_and_scope() {
        let stat = |name: &str, scope: &str, count, total| FixtureStat {
            name: name.into(),
            scope: scope.into(),
            count,
            total,
        };
        let merged = merge_fixtures(vec![
            stat("db", "session", 2, 1.0),
            stat("db", "session", 3, 0.5),   // same key => summed
            stat("db", "function", 1, 0.25), // different scope => distinct
            stat("cache", "session", 4, 2.0),
        ]);
        assert_eq!(merged.len(), 3);
        let db_session = merged
            .iter()
            .find(|f| f.name == "db" && f.scope == "session")
            .unwrap();
        assert_eq!(db_session.count, 5);
        assert!((db_session.total - 1.5).abs() < 1e-9);
    }

    #[test]
    fn report_part_path_names_a_json_file_for_slug() {
        let p = report_part_path("collect", "testuid");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("rstest-mono-"), "got {name}");
        assert!(name.contains("collect"), "got {name}");
        assert!(name.ends_with(".json"), "got {name}");
    }

    fn write_quarantine(suffix: &str, body: &str) -> std::path::PathBuf {
        // Unique per-test name (pid + suffix) so parallel tests never collide.
        let path = std::env::temp_dir().join(format!(
            "rstest-quarantine-test-{}-{suffix}.txt",
            std::process::id()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn quarantine_matcher_handles_exact_globs_comments_blanks() {
        let path = write_quarantine(
            "mixed",
            "# a comment\n\ntest_foo.py::test_a\ntest_bar.py::*\n",
        );
        let set = quarantine_matcher(&path).unwrap();
        assert_eq!(set.len(), 2); // comment + blank line skipped
        assert!(set.is_match("test_foo.py::test_a")); // exact
        assert!(!set.is_match("test_foo.py::test_ab")); // anchored: no substring match
        assert!(set.is_match("test_bar.py::test_z")); // glob
        assert!(!set.is_match("other.py::test_a"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn quarantine_matcher_empty_when_only_comments() {
        let path = write_quarantine("empty", "# nothing here\n\n");
        let set = quarantine_matcher(&path).unwrap();
        assert_eq!(set.len(), 0);
        assert!(!set.is_match("test_foo.py::test_a"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn quarantine_matcher_errors_on_missing_file() {
        let path = std::env::temp_dir().join("rstest-quarantine-does-not-exist-xyz.txt");
        assert!(quarantine_matcher(&path).is_err());
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
