//! Run orchestration: the single-project pipeline (`execute`) and its helpers.
//! `execute` is the entry point `main` and `watch` call. The monorepo driver
//! lives in [`monorepo`], the post-run gates/reports in [`gates`], and the
//! `--collect-only` discovery doc in [`discovery`]. Worker-count sizing is in
//! [`workers`], `--incremental` in [`incremental`], the lazy-collection choice
//! in [`lazy`], cache maintenance in [`cache`], and the hand-off of a resolved
//! run to its execution path in [`dispatch`], and `--quarantine` in
//! [`quarantine`].

mod cache;
pub(crate) mod discovery;
mod dispatch;
mod gates;
mod incremental;
mod lazy;
mod monorepo;
mod quarantine;
mod workers;

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;

use crate::cli::{
    is_collect_only, needs_passthrough_io, parse_durations, passthrough_trigger, Cli,
};
use crate::reporting::sink::Sink;
use crate::reporting::{color, flakes, progress, report};
use crate::scheduling::{durations, pool, warm, worker};
use crate::{config, coverage_skip, discover, doctor, migrate, mono, select};
use cache::{preflight_cache, pull_shared_cache, run_cache_compact};
use dispatch::{dispatch_run, resolve_order, DispatchSelection};
use incremental::resolve_incremental;
use lazy::DurationCache;
pub(crate) use workers::check_workers;
use workers::parse_numprocesses;

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
/// `changed_base` ([`resolve_changed_base`]) and `since_green`/`head`/`env_fp`
/// are computed by the caller (they outlive selection: the base feeds the
/// diff-coverage gate, the rest the post-run green-baseline record).
fn apply_selection(
    cli: &Cli,
    mut args: Vec<String>,
    changed_base: Option<String>,
    since_green: bool,
    head: &Option<String>,
    env_fp: &str,
    sink: &mut Sink,
) -> Result<ControlFlow<i32, Vec<String>>> {
    let mut effective_changed = changed_base;
    if since_green {
        // --since-green owns the diff base: its last-green baseline drives
        // selection, OVERRIDING the "HEAD" base that --changed-strict would
        // otherwise imply (changed_strict is a gating modifier here, not a base;
        // an explicit --changed is already excluded by `since_green`). No
        // baseline yet -> a full run to establish one.
        match crate::incremental::baseline(&std::env::current_dir()?, env_fp) {
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
        let mut project = config::discover(&cwd, sink.err());
        // With no config file, discover() falls back to the cwd, which from a
        // subdirectory (`cd q && rstest --changed ../tests`) limits the diff
        // to that subtree. Use pytest's rootdir for the args instead, the one
        // the cache is anchored at.
        if project.inifile.is_none() {
            project.rootdir = crate::cache::base_dir();
            // Run from below the repo with no config (`cd tests && rstest
            // --changed`), pytest's rootdir is that subdirectory too and a
            // change outside it is invisible. Say so rather than report a
            // quiet "no tests affected".
            let below_toplevel = select::git_toplevel(&project.rootdir)
                .and_then(|top| crate::cache::relative_to(&project.rootdir, &top))
                .is_some_and(|rel| !rel.as_os_str().is_empty());
            if below_toplevel {
                sink.warn(&format!(
                    "rstest: --changed: no pytest config file, so the rootdir is {} and \
                     changes outside it are not seen; add a pytest.ini or pyproject.toml \
                     at the project root",
                    project.rootdir.display()
                ));
            }
        }
        // Coverage-aware selection: uses the line->test index when it is warm
        // (any --cov-context=test run writes it), else falls back per-file to
        // import-graph reachability, so --changed only ever gets tighter.
        // Paths are rootdir-relative and cover the whole project, whichever
        // subdirectory rstest was started from.
        let changes = select::changed_line_ranges(&project.rootdir, rev)?;
        // Coverage-map health, for the "of K mapped" ratio and the cold-map hint.
        let mapped = select::mapped_test_count(&project.rootdir);
        let selection = select::affected_with_coverage(
            &project.rootdir,
            &project,
            &changes,
            cli.changed_strict,
            rev,
        );
        let selection = selection?;
        // One-line nudge when the map is cold BUT a source (non-test) .py file
        // changed — exactly the case where a warm map would have selected fewer
        // tests than the import graph is about to. Silent when coverage wouldn't
        // help (test-only / config / non-Python changes), so it never nags a
        // user who doesn't run coverage - including when such a change forces
        // a full run alongside a source edit (the graph isn't used then).
        if mapped.is_none()
            && !matches!(selection, select::Selection::FullRun(_))
            && changes.keys().any(|f| {
                f.extension().and_then(|e| e.to_str()) == Some("py")
                    && !crate::collect::is_test_file(&project.rootdir.join(f), &project)
                    // A warm map routes conftest.py to the graph too.
                    && f.file_name().and_then(|n| n.to_str()) != Some("conftest.py")
            })
        {
            sink.warn(
                "rstest: --changed is using the import graph (no coverage map). A prior \
                 `--cov --cov-context=test` run enables coverage-precise selection \
                 (usually fewer tests).",
            );
        }
        match selection {
            select::Selection::FullRun(reason) => {
                sink.warn(&format!(
                    "rstest: --changed falling back to full run ({reason})"
                ));
            }
            select::Selection::Tests(tests) if tests.is_empty() => {
                match mapped {
                    // Keep the cold-map wording as the prefix: scripts grep for it.
                    Some(m) => sink.out_line(&format!(
                        "rstest: no tests affected by {} changed file(s) (0 of {m} mapped)",
                        changes.len()
                    )),
                    None => sink.out_line(&format!(
                        "rstest: no tests affected by {} changed file(s)",
                        changes.len()
                    )),
                }
                // Nothing affected since the last green run is itself a green
                // outcome: advance the baseline to HEAD so unrelated commits
                // don't force a re-run next time. Clean tree only: uncommitted
                // edits can mask a red HEAD (a local revert of a bad commit
                // diffs as "nothing changed" against the old baseline).
                if since_green {
                    if let Some(h) = &head {
                        if !crate::incremental::record_green_if_clean(
                            &cwd,
                            &cli.run_outputs(),
                            h,
                            env_fp,
                        ) {
                            sink.warn(crate::incremental::DIRTY_TREE_NOTICE);
                        }
                    }
                }
                // Strict gating still wins on the exit code: it needs to
                // DISTINGUISH "ran nothing" from "everything passed" (pytest's
                // nothing-collected code), even under --since-green. Break with
                // the sentinel; main owns the actual process::exit.
                return Ok(ControlFlow::Break(if cli.changed_strict { 5 } else { 0 }));
            }
            select::Selection::Tests(tests) => {
                // With a warm map, report the savings ratio (affected of mapped);
                // cold, just the affected-target count. Only `file::test` targets
                // came from the map, so they alone are "of K mapped"; whole-file
                // targets (graph fallback, changed test files) are counted apart.
                match mapped {
                    Some(m) => {
                        let from_map = tests
                            .iter()
                            .filter(|t| t.to_string_lossy().contains("::"))
                            .count();
                        let files = tests.len() - from_map;
                        let extra = if files > 0 {
                            format!(" + {files} whole-file target(s)")
                        } else {
                            String::new()
                        };
                        sink.warn(&format!(
                            "rstest: {} changed file(s) -> {from_map} of {m} mapped test(s) \
                             affected{extra}",
                            changes.len(),
                        ));
                    }
                    None => sink.warn(&format!(
                        "rstest: {} changed file(s) -> {} affected test target(s)",
                        changes.len(),
                        tests.len()
                    )),
                }
                let mut selected = select::targets_as_args(&project.rootdir, &cwd, &tests);
                // Keep the user's flags; drop any explicit path args in
                // favor of the selection.
                selected.extend(crate::cli::without_path_args(&args));
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
    /// `--changed`/`--changed-strict` diff base, as resolved for selection.
    changed_base: Option<&'a str>,
    since_green: bool,
    /// The user narrowed a `--since-green` run (`-k`/`-m`/a path/`--deselect`/
    /// `--budget`): its green proves nothing about the tests left out, so no
    /// baseline.
    since_green_narrowed: bool,
    head: &'a Option<String>,
    env_fp: &'a str,
    history_commit: Option<&'a str>,
    incremental_active: bool,
    config: &'a coverage_skip::ConfigState,
    /// Coverage index snapshotted BEFORE the run (drives carry-forward after).
    prev_index: &'a select::CoverageIndex,
    baseline: &'a coverage_skip::Baseline,
    /// `rstest replay`: a diagnostic re-run, not a data point. It skips the
    /// duration / wall / flake-history writes (a debugging loop would pile
    /// `failed` counts onto one test, and a forced CI worker count on a smaller
    /// machine skews the duration baseline).
    replay: bool,
}

/// What a run is stamped with: `HEAD` (`--since-green`'s record and the
/// per-test history's commit), the history commit itself (`-dirty` like `git
/// describe --dirty` when the checkout, not just the rootdir, has uncommitted
/// changes; `None` under `replay`, which records no history), and the
/// environment fingerprint (`explain`'s env_changed). The run's own report
/// files (`outputs`: --junitxml, --html), left over from an earlier run, don't
/// count as changes.
struct RunStamp {
    head: Option<String>,
    history_commit: Option<String>,
    env_fp: String,
}

fn run_stamp(scope: &Path, python: &Path, outputs: &[PathBuf], replay: bool) -> RunStamp {
    let head = crate::incremental::head_sha();
    let history_commit = head.as_ref().filter(|_| !replay).map(|h| {
        let outputs: Vec<&Path> = outputs.iter().map(PathBuf::as_path).collect();
        if std::env::current_dir()
            .map_or(true, |d| crate::incremental::checkout_dirty(&d, &outputs))
        {
            format!("{h}-dirty")
        } else {
            h.clone()
        }
    });
    RunStamp {
        head,
        history_commit,
        env_fp: crate::incremental::env_fingerprint(scope, python),
    }
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
    /// Resolved `--order` (validated up front like `--dist`).
    order: pool::Order,
    /// Whether `--order` came from the flag or `[tool.rstest]` (vs the auto
    /// pick), so conflicts with an explicit request warn or refuse.
    order_explicit: bool,
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
    let n = parse_numprocesses(&numprocesses, args)?;
    // `--debug` runs one worker with inherited stdio (like --pdb) so debugpy
    // owns a single process and its console; route it through the passthrough
    // path regardless of the session flags.
    let passthrough = needs_passthrough_io(args) || cli.debug.is_some();
    // Honor `--reruns` in single-worker mode via a degenerate one-worker pool:
    // the rerun loop is orchestrator-side (rerunfailures neutralized inside).
    // Passthrough can't be pooled, so reruns stay inert there.
    let single_worker_reruns = reruns > 0 && n <= 1 && !passthrough;
    // Resolve `--order` here, not at pool dispatch, so a bad value errors on
    // every run path (same rule as `--dist` above).
    let order = resolve_order(cli, settings)?;
    let order_explicit = cli.order.is_some() || settings.order.is_some();
    // A one-worker rerun pool is 1 worker everywhere downstream (banner,
    // doctor, report-json meta), never 0.
    let n = if single_worker_reruns { 1 } else { n };
    // Heads-up when this parallel run pairs with a plugin flag that goes dark
    // under the pool (silent/empty/racy report), pointing at the native
    // parallel-safe path. Argv-driven, so it fires whether or not the plugin is
    // installed — passing the flag is the intent signal.
    for warning in silent_master_plugin_warnings(n, args) {
        sink.warn(&warning);
    }
    let verbosity = verbosity(args.iter().map(String::as_str));
    let verbose = verbosity >= 1;
    // -vv (or more): pytest shows ALL durations, no hidden-cutoff note.
    let very_verbose = verbosity >= 2;
    // Output style: --output > [tool.rstest] output > (byte-exact mode ?
    // pytest's own terminal : -v ? verbose : tty ? bar : dots). Auto-promote
    // to the sugar bar on an interactive terminal (`Palette::live`: a color
    // tty outside CI, not TERM=dumb), stay on plain dots elsewhere so logs
    // stay byte-stable (the live footer self-disables there).
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
        // Only `[tool.rstest] output` gets here: an unknown `--output` value on
        // the command line is a plugin's flag and went to the session.
        Some(other) => {
            sink.warn(&format!(
                "rstest: unknown [tool.rstest] output '{other}' (use {}); using dots",
                crate::cli::OUTPUT_STYLES.join("|")
            ));
            progress::Mode::Dots
        }
        None if n <= 1 && !single_worker_reruns && !passthrough => progress::Mode::Pytest,
        None if verbose => progress::Mode::Verbose,
        None if sink.palette().live() => progress::Mode::Bar,
        None => progress::Mode::Dots,
    };
    let durations = parse_durations(args);
    // Validate `--doctor-fail-on` conditions up front: a typo'd metric or a
    // missing operator aborts now, never silently as a gate that can't fire.
    let doctor_gate = doctor::parse_conditions(&cli.doctor_fail_on, sink)?;
    // Same for the --durations-regress ratio: refuse it before the suite runs.
    if let Some(ratio) = cli.durations_regress {
        gates::validate_regress_ratio(ratio)?;
    }
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
    // A parent rstest (migrate-check's classifier runs) can ask for the worker
    // instrumentation alone via the hidden --instrument-workers flag, without
    // the doctor report. Never read from the environment: an exported
    // RSTEST_DOCTOR in a shell or CI must not switch it on.
    let instrument = doctor || cli.instrument_workers;
    let worker_env = worker::WorkerEnv {
        run_uid: run_uid.to_string(),
        doctor: instrument,
        timeout: cli.timeout,
        leakcheck,
        warm: None,
        dead_master_scan: cli.warn_on_dead_master_path,
        send_ids: false,
        debug_port: cli.debug.clone(),
        // Ship captured stdout/stderr on every report (not just failures) when a
        // live JSON consumer is attached, so editors get per-passing-test output.
        stream_output: mode == progress::Mode::Json || cli.stream_json.is_some(),
        junitxml: cli.junitxml.as_ref().map(|p| p.display().to_string()),
        reruns: reruns > 0,
        // Filled in once the --quarantine list is loaded (before dispatch).
        quarantine: None,
        network: network_guard(settings, instrument),
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
        order,
        order_explicit,
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
/// A fresh run uid in xdist's `testrun_uid` format (`uuid.uuid4().hex`): 32
/// lowercase hex digits with the uuid4 version and variant bits set, so
/// `uuid.UUID(testrun_uid)` parses. The 122 random bits come from std's
/// OS-seeded `RandomState` keys, mixed with the clock and pid (no crate).
pub(crate) fn new_run_uid() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let nanos = crate::time::now_epoch_nanos();
    let half = |salt: u8| {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_u32(std::process::id());
        h.write_u8(salt);
        h.finish()
    };
    let bits = (u128::from(half(0)) << 64) | u128::from(half(1));
    format_uuid4(bits)
}

/// Format 128 bits as a uuid4 hex string: version nibble 4, RFC 4122 variant.
fn format_uuid4(bits: u128) -> String {
    let bits = (bits & !(0xf << 76) & !(0x3 << 62)) | (0x4 << 76) | (0x2 << 62);
    format!("{bits:032x}")
}

fn attach_stream_json(sink: &mut Sink, path: &std::path::Path) {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
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
    execute_inner(cli, args, None)
}

/// pytest counts every -v, clustered (`-sv`) or repeated (`-v -v`).
fn verbosity<'a>(opts: impl Iterator<Item = &'a str>) -> usize {
    opts.map(|a| {
        if a == "--verbose" {
            1
        } else {
            crate::cli::short_switches(a)
                .iter()
                .filter(|&&c| c == 'v')
                .count()
        }
    })
    .sum()
}

/// pytest's effective `--tb` style, `-r` chars and `-v` count: ini
/// `addopts`, then `PYTEST_ADDOPTS`, then the command line, the last one
/// winning (pytest's own order).
fn output_opts(args: &[String]) -> (report::TbStyle, String, usize) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = config::discover(&cwd, &mut std::io::sink());
    let env_addopts = std::env::var("PYTEST_ADDOPTS").unwrap_or_default();
    let opts = || {
        project
            .addopts
            .iter()
            .map(String::as_str)
            .chain(env_addopts.split_whitespace())
            .chain(args.iter().map(String::as_str))
    };
    (
        report::TbStyle::from_opts(opts()),
        report::report_chars(opts()),
        verbosity(opts()),
    )
}

/// The run pipeline. `pinned` is `Some` only under `rstest replay`, carrying the
/// recorded per-worker schedule to re-pin; a normal run passes `None`.
pub(crate) fn execute_inner(
    cli: &Cli,
    args: &[String],
    pinned: Option<&crate::replay::PinnedSchedule>,
) -> Result<i32> {
    let args = args.to_vec();
    // The single output sink for this run: owns stdout/stderr and the resolved
    // palette. Built up front so every diagnostic below (cache maintenance,
    // selection, banner) flows through it. `--color` resolution matches
    // `resolve_run_config`'s (both call `Palette::detect`).
    let mut sink = Sink::stdio(color::Palette::detect(&args));
    let (tb, report_chars, verbosity) = output_opts(&args);
    sink.set_tb_style(tb);
    sink.set_report_chars(report_chars);
    sink.set_very_verbose(verbosity >= 2);
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
    let run_uid = std::env::var("RSTEST_RUN_UID").unwrap_or_else(|_| new_run_uid());
    // Shared-cache backend: resolve the remote (flag or env) and, if asked,
    // run maintenance / warm the local cache BEFORE anything reads it.
    let cache_remote = cli
        .cache_remote
        .clone()
        .or_else(|| std::env::var("RSTEST_CACHE_REMOTE").ok())
        .filter(|s| !s.is_empty());
    preflight_cache(cli, &cache_remote, &mut sink)?;
    reject_looponfail(&args)?;

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
    let mut cfg = resolve_run_config(cli, &settings, &args, &run_uid, &mut sink)?;
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
    // The run's stamp (four git calls plus the env fingerprint's site-packages
    // walk) on a thread, overlapping selection and setup. Joined before
    // dispatch so the `-dirty` probe sees the tree before any test writes to
    // it, or early under --since-green, whose selection reads head/env_fp.
    let mut stamp_thread = Some({
        let (scope, python) = (scope.clone(), python.clone());
        let outputs: Vec<PathBuf> = cli.run_outputs().into_iter().map(PathBuf::from).collect();
        let replay = pinned.is_some();
        std::thread::spawn(move || run_stamp(&scope, &python, &outputs, replay))
    });
    let mut join_stamp = || {
        stamp_thread
            .take()
            .map(|t| t.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
    };
    let early_stamp = if since_green { join_stamp() } else { None };
    // Narrow args to the affected test targets. Nothing affected => Break with
    // the sentinel exit code, returned up so main owns the single process::exit.
    // Resolved once: selection and the post-run diff-coverage gate share it, so
    // a base that fails to resolve aborts here, before any test runs.
    let changed_base = resolve_changed_base(cli, &mut sink)?;
    // Judged on the user's args, before selection swaps in the affected paths.
    // A --budget run leaves tests out too.
    let since_green_narrowed = since_green
        && (cli.budget.is_some() || crate::since_green::run_narrows_suite(&args, &scope));
    let args = match apply_selection(
        cli,
        args,
        changed_base.clone(),
        since_green,
        // Read only under --since-green, which joined the stamp above.
        early_stamp.as_ref().map_or(&None, |s| &s.head),
        early_stamp.as_ref().map_or("", |s| &s.env_fp),
        &mut sink,
    )? {
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
    // An explicit -n that passthrough silently collapses to one process: say
    // so, or `rstest -n 4 -s` looks like a parallel run that isn't.
    let n_explicit = cli.numprocesses.is_some() || settings.numprocesses.is_some();
    let trigger = if cli.debug.is_some() {
        Some("--debug")
    } else {
        passthrough_trigger(&args)
    };
    if let Some(msg) = passthrough_n_warning(n_explicit, cfg.n, trigger, is_collect_only(&args)) {
        sink.warn(&msg);
    }
    // Resolve dispatch selection (--shuffle/--shard) and the --incremental skip
    // set in one phase.
    let duration_cache = DurationCache::default();
    let inc = resolve_incremental(
        &cfg,
        cli,
        &settings,
        &args,
        since_green,
        &duration_cache,
        &mut sink,
    )?;
    // Load the --quarantine list once, before dispatch: fail-fast ordering needs
    // it (a quarantined test must not lead the queue and trip -x), and the
    // post-run demotion reuses it. Inert under passthrough. Replay never
    // auto-quarantines: the flip history would hide the very failure it
    // re-runs to reproduce.
    let mut history = crate::reporting::history::RunHistory::default();
    let quarantine = if passthrough {
        None
    } else {
        quarantine::quarantine_set(
            cli.quarantine.as_deref().or(settings.quarantine.as_deref()),
            if pinned.is_some() {
                0
            } else {
                cli.auto_quarantine
                    .or(settings.auto_quarantine)
                    .unwrap_or(crate::cli::AUTO_QUARANTINE)
            },
            cli.auto_quarantine_max
                .or(settings.auto_quarantine_max)
                .unwrap_or(crate::cli::AUTO_QUARANTINE_MAX),
            &mut history,
            &mut sink,
        )?
    };
    // Sessions keep quarantined failures out of their own -x / --maxfail count
    // (the pool's count skips them too), so a quarantined test can't stop the
    // run; auto-quarantined ones count again past the cap, as post-run.
    cfg.worker_env.quarantine =
        quarantine
            .as_ref()
            .filter(|q| !q.all.is_empty())
            .map(|q| worker::WorkerQuarantine {
                file: q.file.patterns().join("\n"),
                auto: q
                    .auto
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
                max_pct: q.max_pct,
            });
    // --budget costs each test its call time plus its median setup from the
    // run history, which the quarantine set above may already have read.
    let budget = inc.budget.map(|secs| pool::Budget {
        secs: secs as f64,
        label: cli.budget.clone().unwrap_or_default(),
        setup: history.get().median_setup(),
    });
    let RunStamp {
        head,
        history_commit,
        env_fp,
    } = early_stamp
        .or_else(join_stamp)
        .expect("run stamp joined once");
    // `--fork-pool=app`: only a pool run has a zygote to warm.
    let warm_run = (cli.fork_pool == Some(warm::ForkPool::App)
        && !passthrough
        && (n > 1 || single_worker_reruns))
        .then(|| {
            let key = warm::Key {
                python: python.display().to_string(),
                rstest: env!("CARGO_PKG_VERSION").to_string(),
                env_fp: env_fp.clone(),
                rootdir: scope.display().to_string(),
            };
            warm::prepare(key, &args, &mut sink)
        })
        .flatten();
    cfg.worker_env.warm = warm_run.as_ref().map(|w| w.env.clone());
    let selection = DispatchSelection {
        shuffle_seed: inc.shuffle_seed,
        shard: inc.shard,
        budget: budget.as_ref(),
        lazy: inc.lazy,
        quarantine: quarantine.as_ref(),
        durations: &duration_cache,
        env_fp: &env_fp,
    };
    let mut outcome = dispatch_run(
        &cfg,
        cli,
        &settings,
        &args,
        &inc.skip_ids,
        selection,
        pinned,
        &mut sink,
    )?;
    // `--fork-pool=app`: every worker forked from the warm template died
    // before any test reported (agent-platform-backend on macOS: a library's
    // background thread hit the Objective-C fork-safety abort). Nothing ran,
    // so run it again with plain workers instead of failing it; `finish`
    // below still resets the template. Once a test has reported, rerunning
    // would repeat its side effects, so that run fails as before.
    if warm_run.as_ref().is_some_and(|w| w.env.manifest.is_some())
        && outcome.warm_record.imported.is_empty()
        && outcome.run.tests().is_empty()
    {
        sink.warn(
            "rstest: note: --fork-pool=app: every worker failed before any test ran with \
             the warm template; running again without it",
        );
        outcome.reap_workers();
        cfg.worker_env.warm = None;
        outcome = dispatch_run(
            &cfg,
            cli,
            &settings,
            &args,
            &inc.skip_ids,
            selection,
            pinned,
            &mut sink,
        )?;
    }

    quarantine::apply_quarantine(
        cli,
        quarantine.as_ref(),
        &mut outcome,
        passthrough,
        &mut sink,
    )?;
    gates::finalize_output(
        &mut outcome,
        passthrough,
        mode,
        durations,
        very_verbose,
        start,
        &mut sink,
    );
    if let Some(w) = warm_run {
        let record = std::mem::take(&mut outcome.warm_record);
        outcome.warm = warm::finish(w, record, &mut sink);
    }
    if !passthrough && cli.python.is_none() {
        if let Some(hint) = interpreter_hint(&outcome.run, &python, &scope) {
            sink.warn(&hint);
        }
    }

    let post = PostRun {
        start,
        started_epoch,
        run_uid: &run_uid,
        cache_remote: cache_remote.as_deref(),
        shard: inc.shard,
        changed_base: changed_base.as_deref(),
        since_green,
        since_green_narrowed,
        head: &head,
        env_fp: &env_fp,
        history_commit: history_commit.as_deref(),
        incremental_active: inc.active,
        config: &inc.config,
        prev_index: &inc.prev_index,
        baseline: &inc.baseline,
        replay: pinned.is_some(),
    };
    let exitstatus =
        gates::run_post_gates(&cfg, cli, &mut outcome, &args, &post, history, &mut sink);
    // Never return before the workers do: a caller (or the next --watch run)
    // may read what they write on exit.
    outcome.reap_workers();
    exitstatus
}

/// Dispatch a subcommand (`rstest verify-vendor` / `try` / `migrate-check` /
/// `cache-compact` / `shard-verify` / `explain` / `flaky` / `install-skills` /
/// `replay`).
/// Returns `Some(exit)` when a subcommand ran, `None` for a normal run (the
/// caller falls through to watch/`execute`). The interpreter-free ones
/// (`cache-compact`, `shard-verify`, `explain`, `flaky`, `install-skills`) return before
/// Python is resolved; `replay` runs the full pipeline (resolving its own
/// interpreter through `execute`) with the recorded schedule pinned; the rest
/// resolve Python here.
pub fn dispatch_command(cli: &Cli, args: &[String]) -> Result<Option<i32>> {
    use crate::cli::Command;
    let Some(command) = &cli.command else {
        return Ok(None);
    };
    let mut sink = Sink::stdio(color::Palette::detect(args));
    // cache-compact, shard-verify and explain are interpreter-free (they only touch
    // cache/report files); the rest resolve Python first, lazily, so the
    // interpreter-free modes never probe one.
    let scope = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let python = || discover::resolve(&scope, cli.python.as_deref());
    // An empty entry (`--migrate-allow "$UNSET"`) is a substring of every id:
    // it would allow every finding, so it allows none.
    let migrate_allow: Vec<String> = cli
        .migrate_allow
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect();
    let result = match command {
        Command::CacheCompact { keep_last, max_age } => {
            run_cache_compact(cli, &mut sink, *keep_last, max_age.as_deref())
        }
        Command::ShardVerify { reports } => {
            crate::shardverify::run_shard_verify(&mut sink, reports)
        }
        Command::Explain { nodeid, json } => crate::explain::run_explain(&mut sink, nodeid, *json),
        Command::Flaky { top, json } => crate::flaky::run_flaky(&mut sink, *top, *json),
        Command::Report { out } => crate::dashboard::run_report(&mut sink, out.as_deref()),
        // Needs the interpreter's path (its venv's packages), not a session.
        Command::SinceGreen { json } => {
            python().and_then(|py| crate::since_green::run_since_green(&mut sink, &py, *json))
        }
        Command::InstallSkills {
            user,
            agents,
            dir,
            force,
        } => crate::skills::target_dir(dir.as_deref(), *user, *agents)
            .and_then(|target| crate::skills::run_install(&mut sink, &target, *force)),
        // Verify the vendored pytest tree against the packaged manifest.
        Command::VerifyVendor => python().and_then(|py| crate::vendor::run_verify(&py)),
        // Zero-config "should I switch?" proof: pytest baseline vs rstest -n auto.
        Command::Try => python().and_then(|py| migrate::run_try(&py, args, &mut sink)),
        // Parallel-readiness preflight: its own collect-twice path, not a run.
        Command::MigrateCheck => python().and_then(|py| {
            migrate::run_migrate_check(
                &py,
                args,
                cli.migrate_check_json.as_deref(),
                &migrate_allow,
                &mut sink,
            )
        }),
        // Uninstall-pytest-xdist readiness: static scan, optional trial run.
        Command::XdistRemovalCheck => python().and_then(|py| {
            migrate::run_xdist_removal_check(
                &py,
                args,
                cli.xdist_removal_json.as_deref(),
                &migrate_allow,
                cli.xdist_trial,
                &mut sink,
            )
        }),
        // Auto parallel-safety audit: repeat -n auto, diff vs -n 0, serial fix-list.
        Command::Audit => python().and_then(|py| {
            migrate::run_audit(
                &py,
                args,
                cli.audit_repeat.unwrap_or(1),
                cli.audit_json.as_deref(),
                &mut sink,
            )
        }),
        // Order-dependency bisect: delta-debug the predecessor set at -n 0.
        Command::Bisect {
            nodeid,
            pytest_args,
        } => python().and_then(|py| {
            migrate::run_bisect(
                &py,
                cli.python.as_deref(),
                nodeid,
                pytest_args,
                cli.bisect_json.as_deref(),
                &mut sink,
            )
        }),
        // `replay` runs the suite (it resolves its own interpreter through the
        // run pipeline) and returns the run's exit code.
        Command::Replay { run_id, journal } => {
            crate::replay::run_replay(cli, run_id.as_deref(), journal.as_deref(), &mut sink)
        }
    };
    // Verdict subcommands spend exit 1 on "found something", so an error
    // inside them (no interpreter, a failed spawn) must not read as a finding:
    // report it the way `main` would and exit 2, their "couldn't run" code.
    let verdict = matches!(
        command,
        Command::Try
            | Command::MigrateCheck
            | Command::XdistRemovalCheck
            | Command::Audit
            | Command::Bisect { .. }
    );
    let code = match result {
        Err(e) if verdict => {
            eprintln!("Error: {e:?}");
            record_verdict_error(cli, command, &e);
            2
        }
        other => other?,
    };
    Ok(Some(code))
}

/// A verdict subcommand failed before (or outside) its own JSON bookkeeping,
/// e.g. no usable interpreter: leave its `--*-json` document saying so, never a
/// stale result from an earlier run that a CI gate would read as current.
fn record_verdict_error(cli: &Cli, command: &crate::cli::Command, e: &anyhow::Error) {
    use crate::cli::Command;
    match command {
        Command::Bisect { nodeid, .. } => {
            let _ = migrate::write_bisect_error_json(
                cli.bisect_json.as_deref(),
                nodeid,
                &format!("{e:#}"),
            );
        }
        Command::Audit => {
            if let Some(path) = cli.audit_json.as_deref() {
                let doc =
                    serde_json::to_string_pretty(&migrate::audit_not_run_doc()).unwrap_or_default();
                let _ = crate::reporting::write_output(path, doc);
            }
        }
        Command::MigrateCheck => {
            if let Some(path) = cli.migrate_check_json.as_deref() {
                let _ = std::fs::remove_file(path);
            }
        }
        // A stale `"ready": true` would let a CI gate uninstall pytest-xdist
        // on the strength of a run that never happened.
        Command::XdistRemovalCheck => {
            if let Some(path) = cli.xdist_removal_json.as_deref() {
                let _ = std::fs::remove_file(path);
            }
        }
        _ => {}
    }
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
    if names_a_selection(args) || config::has_pytest_config(&cwd, sink.err()) {
        return Ok(ControlFlow::Continue(()));
    }
    let projects = mono::discover_projects(&cwd, settings.projects.as_deref());
    let threshold = if settings.projects.is_some() { 1 } else { 2 };
    if projects.len() < threshold {
        return Ok(ControlFlow::Continue(()));
    }
    // Each project keeps its OWN cache dir (cache::mono_override), and the per-run
    // push/pull wiring lives in the single-project path that execute_monorepo
    // bypasses — so a cache flag here would silently no-op (push) or warm the
    // wrong root cache (pull). Fail loud; run rstest per project for shared caching.
    if cli.cache_pull || cli.cache_push {
        anyhow::bail!(
            "--cache-pull/--cache-push are not supported in monorepo mode \
             (each project has its own .rstest_cache); run rstest per project"
        );
    }
    // The root never reaches resolve_run_config, so validate --order here: a
    // bad value errors once, not once per child. Children get it forwarded.
    if let Some(o) = &cli.order {
        o.parse::<pool::Order>().map_err(|e| anyhow::anyhow!(e))?;
    }
    if let Some(ratio) = cli.durations_regress {
        gates::validate_regress_ratio(ratio)?;
    }
    monorepo::execute_monorepo(cli, args, &cwd, projects, run_uid, sink).map(ControlFlow::Break)
}

/// Do the session args name tests explicitly? An existing path does, and so
/// does a pytest `@argsfile` (its lines are the selection, e.g. `bisect`'s
/// child runs): either keeps the run single-project instead of fanning out
/// over every subproject and ignoring what was asked for.
fn names_a_selection(args: &[String]) -> bool {
    !crate::cli::path_args(args).is_empty()
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
        && durations::load_baseline().is_empty()
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
    if matches!(mode, progress::Mode::Json | progress::Mode::Pytest) {
        return;
    }
    let worker_desc = if single_worker_reruns {
        "single worker (rerun pool; not byte-exact)".to_string()
    } else if n <= 1 {
        "single-worker mode".to_string()
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

/// `-n N` (N > 1, set on the command line or in `[tool.rstest]`) is ignored when
/// a passthrough flag (`-s`, `--capture=…`, `--pdb`, `--trace`, stepwise,
/// `--debug`) routes the run to one process with pytest's own terminal. Warn,
/// naming the flag. Quiet for the default `-n auto` (plain `rstest -s` is an
/// ordinary request for pytest's `-s`) and for `--co`, which runs no tests.
fn passthrough_n_warning(
    n_explicit: bool,
    n: usize,
    trigger: Option<&str>,
    collect_only: bool,
) -> Option<String> {
    let trigger = trigger?;
    if !n_explicit || n <= 1 || collect_only {
        return None;
    }
    Some(format!(
        "rstest: {trigger} runs the session in a single process with pytest's own \
         output, so -n {n} is ignored (no parallel workers); drop {trigger} to \
         run in parallel"
    ))
}

/// The outbound-network recorder for this run's workers: on under doctor
/// instrumentation, else off. It never blocks (pytest-socket does that).
fn network_guard(settings: &config::RstestSettings, record: bool) -> Option<worker::NetworkGuard> {
    record.then(|| worker::NetworkGuard {
        allow: settings.network_allow.clone().unwrap_or_default(),
    })
}

/// A pointer at the interpreter when a run failed on missing imports and
/// discovery passed over the project's `.venv` (pre-commit's `language:
/// python` hooks set `VIRTUAL_ENV` to the hook env, which has rstest and pytest
/// but none of the project's deps). Shown only when both hold, never as a
/// blanket "VIRTUAL_ENV differs from .venv" notice: under tox/nox that
/// difference is the normal, intended setup, and a warning on every green run
/// there would be noise. An import error is the symptom of the wrong env, so
/// that is when naming the two envs pays off.
fn interpreter_hint(
    run: &report::Run,
    python: &std::path::Path,
    scope: &std::path::Path,
) -> Option<String> {
    let import_failure = |text: &str| {
        text.contains("ModuleNotFoundError") || text.contains("ImportError while importing")
    };
    let failed_on_import = run.collect_errors().iter().any(|(_, t)| import_failure(t))
        || run
            .history_failed_nodeids()
            .filter_map(|id| run.failure_text(id))
            .any(import_failure);
    if !failed_on_import {
        return None;
    }
    let (venv, from_virtual_env) = discover::skipped_project_venv(scope, python)?;
    let source = if from_virtual_env {
        " (from $VIRTUAL_ENV)"
    } else {
        ""
    };
    Some(format!(
        "rstest: hint: imports failed under {}{source}, not the project's virtualenv {}. \
         If the project's dependencies are installed there, pass --python {} \
         (or unset VIRTUAL_ENV).",
        python.display(),
        venv.display(),
        venv.display()
    ))
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
             (no SIGALRM). Only the coarser per-test hang watchdog applies (3x the test's timeout \
             + 10s), and it kills the whole worker, not just the stuck test; set --worker-timeout \
             SECS for a fixed, tighter cap."
        );
    }
}

/// Session flags whose owning plugin produces **no usable artifact** under the
/// parallel pool: each writes one whole-suite file gated on the xdist master,
/// which rstest has none of, so at `-n ≥ 2` the file is missing, empty, or a
/// racy per-worker fragment (see `docs/reference/top-100-plugins.md`). Format:
/// `(flag, plugin, suggestion)`.
///
/// rstest OWNS `--html` / `--junitxml` / `--report-json` and renders them from
/// merged results (parallel-safe), so those are deliberately absent — passing
/// them is the *fix*, not a hazard. `--json-report` (pytest-json-report) is a
/// distinct flag from rstest's `--report-json` and IS forwarded, hence listed.
const SILENT_MASTER_FLAGS: &[(&str, &str, &str)] = &[
    (
        "--json-report",
        "pytest-json-report",
        "use rstest's native --report-json, or run -n 0",
    ),
    (
        "--report-log",
        "pytest-reportlog",
        "use rstest's native --report-json, or run -n 0",
    ),
    (
        "--ctrf",
        "pytest-json-ctrf",
        "use rstest's native --report-json, or run -n 0",
    ),
    (
        "--nunit-xml",
        "pytest-nunit",
        "use rstest's native --junitxml, or run -n 0",
    ),
    (
        "--md",
        "pytest-md",
        "run -n 0 (its report is empty under the pool)",
    ),
    (
        "--csv",
        "pytest-csv",
        "use rstest's native --report-json, or run -n 0 (its CSV is racy under the pool)",
    ),
];

/// True when `flag` appears in `args` as a bare token (`--report-log out.x`) or
/// its `=`-joined form (`--report-log=out.x`).
fn session_flag_present(args: &[String], flag: &str) -> bool {
    let eq = format!("{flag}=");
    args.iter().any(|a| a == flag || a.starts_with(&eq))
}

/// Warn when a run pairs `-n ≥ 2` with a session flag whose plugin goes dark
/// under the pool, naming the parallel-safe alternative. Pure (returns the
/// lines) so the mapping is unit-tested without a Sink; the caller emits each
/// via `sink.warn`. Empty at `n < 2` (single-worker — the plugin's own master
/// branch runs, so its artifact is produced normally).
fn silent_master_plugin_warnings(n: usize, args: &[String]) -> Vec<String> {
    if n < 2 {
        return Vec::new();
    }
    let mut out: Vec<String> = SILENT_MASTER_FLAGS
        .iter()
        .filter(|(flag, _, _)| session_flag_present(args, flag))
        .map(|(flag, plugin, suggest)| {
            format!(
                "rstest: warning: {flag} ({plugin}) produces no usable report under the parallel \
                 pool — it aggregates on the xdist master, which rstest has none of; {suggest}. \
                 See docs/reference/top-100-plugins.md"
            )
        })
        .collect();
    // pytest-benchmark disables itself at -n >= 2 (it detects the pool as xdist)
    // rather than writing a partial file; any --benchmark* flag is the signal.
    if args.iter().any(|a| a.starts_with("--benchmark")) {
        out.push(
            "rstest: warning: pytest-benchmark auto-disables under the parallel pool; measure at \
             -n 0 and read --benchmark-json. See docs/reference/top-100-plugins.md"
                .to_string(),
        );
    }
    out
}

/// pytest-xdist's `--looponfail` / `-f` (alone or clustered, e.g. `-fv`)
/// would reach every worker session, where xdist's loop-on-fail mode takes
/// the session over and the run hangs. Refuse it up front and point at
/// `--watch`. (The same flag from ini `addopts` is switched off in the worker.)
fn reject_looponfail(args: &[String]) -> Result<()> {
    let flagged = args
        .iter()
        .zip(crate::cli::positional_mask(args))
        .find(|(a, positional)| {
            !positional && (a.as_str() == "--looponfail" || crate::cli::has_short(a, 'f'))
        });
    if let Some((a, _)) = flagged {
        anyhow::bail!(
            "{a} is pytest-xdist's loop-on-fail mode, which rstest does not run \
             (it would take over every worker session and hang); use --watch, \
             which reruns on change with import-graph selection"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::cache::{
        parse_duration_secs, resolve_retention_policy, run_cache_compact, validate_cache_flags,
    };
    use super::dispatch::{
        check_order_shuffle, fold_run_event, mark_session_flaky, order_ignored_warning,
        resolve_order, resolve_shard, resolve_shuffle_seed, RunPath,
    };
    use super::incremental::{incremental_config, resolve_budget, warn_incremental_conflicts};
    use super::lazy::{
        auto_lazy, collect_lazy, lazy_layout_fits, lazy_measured_slower, lazy_should_steal,
        requests_doctests, DurationCache, AUTO_LAZY_MIN_TESTS,
    };
    use super::workers::{
        cap_workers_by_files, cap_workers_by_time, names_existing_path, parse_numprocesses,
    };
    use super::{
        attach_stream_json, check_require_baseline, dispatch_command, head_to_none,
        names_a_selection, print_run_banner, reject_looponfail, resolve_changed_base,
        silent_master_plugin_warnings, warn_windows_timeout,
    };
    use crate::cli::Cli;
    use crate::config::RstestSettings;
    use crate::remote;
    use crate::reporting::sink::Sink;
    use crate::reporting::{progress, report};
    use crate::scheduling::pool;
    use crate::scheduling::proto;
    use clap::Parser;

    #[test]
    fn run_uid_is_uuid4_hex() {
        let a = super::new_run_uid();
        let b = super::new_run_uid();
        assert_ne!(a, b, "each run gets its own uid");
        for u in [&a, &b] {
            // No `{u}` in the messages: CodeQL flags printing a `*uid*` as
            // cleartext logging of sensitive data.
            assert!(u.len() == 32);
            assert!(u.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
            assert!(u.as_bytes()[12] == b'4', "uuid4 version nibble");
            assert!(b"89ab".contains(&u.as_bytes()[16]), "RFC 4122 variant");
        }
        assert_eq!(super::format_uuid4(0), "00000000000040008000000000000000");
        assert_eq!(
            super::format_uuid4(u128::MAX),
            "ffffffffffff4fffbfffffffffffffff"
        );
    }

    #[test]
    fn an_argsfile_counts_as_an_explicit_selection() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("rstest-sel-{}.txt", std::process::id()));
        std::fs::write(&file, "t.py::a\n").unwrap();
        let at = |p: &std::path::Path| vec![format!("@{}", p.display())];
        let has_file = names_a_selection(&at(&file));
        let missing = names_a_selection(&at(&dir.join("rstest-no-such-argsfile.txt")));
        let _ = std::fs::remove_file(&file);
        assert!(has_file, "an existing @argsfile names tests");
        assert!(!missing, "a missing @argsfile names nothing");
        assert!(names_a_selection(&[dir.display().to_string()]));
        assert!(!names_a_selection(&["-k".into(), "smoke".into()]));
    }

    #[cfg(unix)]
    #[test]
    fn single_worker_interrupts_drop_does_not_wait_out_the_poll_tick() {
        // A 1-test session reports Done a few ms after spawn; dropping the guard
        // used to join a forwarder mid-`sleep(50ms)`, stalling every short run.
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let guard = super::dispatch::SingleWorkerInterrupts::install(Some(child.id()));
        std::thread::sleep(std::time::Duration::from_millis(5));
        let t = std::time::Instant::now();
        drop(guard);
        let took = t.elapsed();
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            took < std::time::Duration::from_millis(25),
            "drop took {took:?}"
        );
    }

    fn cli() -> Cli {
        Cli::parse_from(["rstest"])
    }

    use crate::test_env;

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

    fn sv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn silent_master_warnings_fire_only_under_the_pool() {
        // -n >= 2 + a dark-plugin flag => one warning naming the plugin + native path.
        let w = silent_master_plugin_warnings(2, &sv(&["--report-log=out.jsonl", "tests/"]));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("pytest-reportlog"));
        assert!(w[0].contains("--report-json")); // the parallel-safe alternative
                                                 // Bare-token form (value is the following argv item) matches too.
        assert_eq!(
            silent_master_plugin_warnings(4, &sv(&["--report-log", "out.jsonl"])).len(),
            1
        );
        // Single-worker: the plugin's own master branch runs => no warning.
        assert!(silent_master_plugin_warnings(1, &sv(&["--report-log=out.jsonl"])).is_empty());
        assert!(silent_master_plugin_warnings(0, &sv(&["--csv=r.csv"])).is_empty());
    }

    #[test]
    fn incremental_config_threads_cov_scope() {
        // Wiring guard for resolve_incremental's config seam: under a narrowed
        // --cov — on the CLI or only in the ini addopts — the fingerprint MUST
        // fold unmeasured first-party source. If the scope is ever dropped,
        // editing coverage-invisible code would not move the fp -> a silent
        // false-green.
        let scope = std::env::temp_dir().join(format!("rstest-wiring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
        std::fs::create_dir_all(scope.join("pkg")).unwrap();
        std::fs::create_dir_all(scope.join("other")).unwrap();
        std::fs::write(scope.join("pkg/mod.py"), b"x = 1\n").unwrap();
        std::fs::write(scope.join("other/mod.py"), b"y = 1\n").unwrap();
        let fp = |args: &[&str]| incremental_config(&scope, &scope, &sv(args)).1.fp;

        // Under --cov=pkg, editing out-of-scope other/mod.py MUST move the fp.
        let before = fp(&["--cov=pkg"]);
        std::fs::write(scope.join("other/mod.py"), b"y = 2\n").unwrap();
        assert_ne!(
            before,
            fp(&["--cov=pkg"]),
            "out-of-scope edit must bust under --cov=pkg"
        );

        // Sanity: no --cov does NOT fold out-of-scope source, so the same edit
        // leaves the fp stable — proving the difference above is scoping.
        let wt = fp(&[]);
        std::fs::write(scope.join("other/mod.py"), b"y = 3\n").unwrap();
        assert_eq!(wt, fp(&[]), "no --cov: out-of-scope source is not folded");

        // --cov=pkg only in the ini addopts narrows just the same.
        std::fs::write(scope.join("pytest.ini"), b"[pytest]\naddopts = --cov=pkg\n").unwrap();
        let (cov, _) = incremental_config(&scope, &scope, &sv(&[]));
        assert_eq!(cov.sources, vec!["pkg"], "addopts --cov must be seen");
        let before = fp(&[]);
        std::fs::write(scope.join("other/mod.py"), b"y = 4\n").unwrap();
        assert_ne!(before, fp(&[]), "addopts-only --cov=pkg must fold other/");

        // A later run WITHOUT --cov reuses the recorded scope of the run that
        // wrote the index: other/ still folds. With nothing recorded, the
        // index's scope is unknown and the state says so.
        std::fs::remove_file(scope.join("pytest.ini")).unwrap();
        let (_, state) = incremental_config(&scope, &scope, &sv(&[]));
        assert_eq!(state.index_cov_args, None, "nothing recorded yet");
        let (_, cov_run) = incremental_config(&scope, &scope, &sv(&["--cov=pkg"]));
        crate::coverage_skip::record(
            &scope,
            &cov_run,
            Default::default(),
            Default::default(),
            &Default::default(),
            None,
        );
        let (cov, plain) = incremental_config(&scope, &scope, &sv(&[]));
        assert_eq!(plain.index_cov_args, Some(sv(&["--cov=pkg"])));
        assert_eq!(cov.sources, vec!["pkg"]);
        std::fs::write(scope.join("other/mod.py"), b"y = 5\n").unwrap();
        assert_ne!(plain.fp, fp(&[]), "no-cov run still folds other/");
        let _ = std::fs::remove_dir_all(&scope);
    }

    #[test]
    fn silent_master_warnings_ignore_rstest_owned_report_flags() {
        // rstest OWNS --html/--junitxml/--report-json (rendered from merged
        // results) — passing them is the fix, so never warn.
        assert!(silent_master_plugin_warnings(4, &sv(&["--html=r.html"])).is_empty());
        assert!(silent_master_plugin_warnings(4, &sv(&["--junitxml=r.xml"])).is_empty());
        assert!(silent_master_plugin_warnings(4, &sv(&["--report-json=r.json"])).is_empty());
        // ...but --json-report (pytest-json-report, a DIFFERENT flag) does warn.
        assert_eq!(
            silent_master_plugin_warnings(4, &sv(&["--json-report"])).len(),
            1
        );
    }

    #[test]
    fn silent_master_warnings_cover_benchmark_and_multiple_flags() {
        // Any --benchmark* flag => the auto-disable heads-up.
        let b = silent_master_plugin_warnings(2, &sv(&["--benchmark-only"]));
        assert_eq!(b.len(), 1);
        assert!(b[0].contains("pytest-benchmark"));
        // Several dark flags in one run => one warning each.
        let many = silent_master_plugin_warnings(
            2,
            &sv(&["--csv=r.csv", "--md=r.md", "--nunit-xml=n.xml"]),
        );
        assert_eq!(many.len(), 3);
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
    fn parse_numprocesses_parses_and_rejects() {
        assert_eq!(parse_numprocesses("4", &[]).unwrap(), 4);
        assert_eq!(parse_numprocesses("0", &[]).unwrap(), 0);
        assert!(parse_numprocesses("-1", &[]).is_err());
        // Regression: the error names the bad value and where it comes from,
        // not just a bare `invalid digit found in string`.
        let err = format!("{:#}", parse_numprocesses("abc", &[]).unwrap_err());
        assert!(err.contains("\"abc\""), "{err}");
        assert!(err.contains("--numprocesses"), "{err}");
        assert!(err.contains("non-negative integer or \"auto\""), "{err}");
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
    fn collect_lazy_cold_cache_stays_full() {
        // No CLI flag, no setting, and (in the test cwd) no warm duration
        // cache => auto sees ~0 known tests => full.
        assert!(!collect_lazy(
            &cli(),
            &settings_collect(None),
            "load",
            std::path::Path::new("/nonexistent/python"),
            &[],
            8,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .unwrap());
    }

    #[test]
    fn collect_lazy_enabled_for_file_affine_dist() {
        let s = settings_collect(Some("lazy"));
        assert!(collect_lazy(
            &cli(),
            &s,
            "load",
            std::path::Path::new("/nonexistent/python"),
            &[],
            8,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .unwrap());
        assert!(collect_lazy(
            &cli(),
            &s,
            "loadfile",
            std::path::Path::new("/nonexistent/python"),
            &[],
            8,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .unwrap());
    }

    #[test]
    fn collect_lazy_rejects_incompatible_dist() {
        let s = settings_collect(Some("lazy"));
        // loadscope/loadgroup need a global id list; lazy is file-affine.
        assert!(collect_lazy(
            &cli(),
            &s,
            "loadscope",
            std::path::Path::new("/nonexistent/python"),
            &[],
            8,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .is_err());
        assert!(collect_lazy(
            &cli(),
            &s,
            "loadgroup",
            std::path::Path::new("/nonexistent/python"),
            &[],
            8,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .is_err());
        // each runs the whole suite per worker: no file-level dispatch either.
        let err = collect_lazy(
            &cli(),
            &s,
            "each",
            std::path::Path::new("/nonexistent/python"),
            &[],
            8,
            &DurationCache::default(),
            &mut Sink::captured().0,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("each runs the full suite"),
            "{err}"
        );
    }

    #[test]
    fn collect_lazy_falls_back_on_nodeid_or_pyargs() {
        let s = settings_collect(Some("lazy"));
        // Explicit nodeid selection can't ride the file walk => full.
        assert!(!collect_lazy(
            &cli(),
            &s,
            "load",
            std::path::Path::new("/nonexistent/python"),
            &["test_x.py::test_a".to_string()],
            8,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .unwrap());
        // A --pyargs name the interpreter can't resolve => full.
        assert!(!collect_lazy(
            &cli(),
            &s,
            "load",
            std::path::Path::new("/nonexistent/python"),
            &["--pyargs".to_string(), "no_such_pkg_xyz".to_string()],
            8,
            &DurationCache::default(),
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
            std::path::Path::new("/nonexistent/python"),
            &[],
            8,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .is_err());
    }

    fn settings_order(mode: Option<&str>) -> RstestSettings {
        RstestSettings {
            order: mode.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_order_defaults_throughput_and_watch_auto_failfast() {
        let none = settings_order(None);
        // No flag, no watch => throughput.
        assert_eq!(
            resolve_order(&cli(), &none).unwrap(),
            pool::Order::Throughput
        );
        // --watch auto-selects fail-fast.
        let mut w = cli();
        w.watch = true;
        assert_eq!(resolve_order(&w, &none).unwrap(), pool::Order::FailFast);
    }

    #[test]
    fn resolve_order_explicit_beats_watch_and_config() {
        // Explicit --order throughput overrides the watch auto-pick.
        let mut w = cli();
        w.watch = true;
        w.order = Some("throughput".into());
        assert_eq!(
            resolve_order(&w, &settings_order(None)).unwrap(),
            pool::Order::Throughput
        );
        // Config supplies it when the flag is absent.
        assert_eq!(
            resolve_order(&cli(), &settings_order(Some("fail-fast"))).unwrap(),
            pool::Order::FailFast
        );
    }

    #[test]
    fn resolve_order_budget_auto_failfast_explicit_wins() {
        let mut b = cli();
        b.budget = Some("90s".into());
        assert_eq!(
            resolve_order(&b, &settings_order(None)).unwrap(),
            pool::Order::FailFast
        );
        b.order = Some("throughput".into());
        assert_eq!(
            resolve_order(&b, &settings_order(None)).unwrap(),
            pool::Order::Throughput
        );
    }

    #[test]
    fn resolve_budget_parses_and_refuses_what_it_cannot_pick_from() {
        let ok = |spec| resolve_budget(Some(spec), 4, false, false, "load");
        assert_eq!(resolve_budget(None, 0, true, true, "each").unwrap(), None);
        assert_eq!(ok("90s").unwrap(), Some(90));
        assert_eq!(ok("5m").unwrap(), Some(300));
        assert_eq!(ok("45").unwrap(), Some(45));
        let err = |r: anyhow::Result<Option<u64>>| r.unwrap_err().to_string();
        assert!(err(ok("soon")).starts_with("--budget: invalid duration"));
        assert!(err(ok("0s")).contains("more than 0s"));
        for (n, passthrough) in [(1, false), (0, false), (4, true)] {
            let r = resolve_budget(Some("90s"), n, passthrough, false, "load");
            assert!(err(r).contains("needs the parallel pool"));
        }
        assert!(err(resolve_budget(Some("90s"), 4, false, true, "load")).contains("--collect lazy"));
        assert!(err(resolve_budget(Some("90s"), 4, false, false, "each")).contains("--dist each"));
        // Affinity modes still dispatch tests: allowed.
        assert!(resolve_budget(Some("90s"), 4, false, false, "loadfile").is_ok());
    }

    #[test]
    fn resolve_order_rejects_unknown() {
        assert!(resolve_order(&cli(), &settings_order(Some("sideways"))).is_err());
    }

    #[test]
    fn passthrough_n_warning_only_for_explicit_parallel_n() {
        // Explicit -n 4 with -s: warn, naming the flag and the ignored count.
        let msg = super::passthrough_n_warning(true, 4, Some("-s"), false).unwrap();
        assert!(
            msg.contains("-s") && msg.contains("-n 4 is ignored"),
            "{msg}"
        );
        // --debug is named too.
        assert!(
            super::passthrough_n_warning(true, 2, Some("--debug"), false)
                .unwrap()
                .contains("--debug")
        );
        // No passthrough flag, default -n auto, -n 0/1, or --co: quiet.
        assert_eq!(super::passthrough_n_warning(true, 4, None, false), None);
        assert_eq!(
            super::passthrough_n_warning(false, 8, Some("-s"), false),
            None
        );
        assert_eq!(
            super::passthrough_n_warning(true, 1, Some("-s"), false),
            None
        );
        assert_eq!(
            super::passthrough_n_warning(true, 4, Some("--co"), true),
            None
        );
    }

    #[test]
    fn passthrough_trigger_names_the_first_forcing_flag() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            crate::cli::passthrough_trigger(&args(&["-k", "x", "-s"])),
            Some("-s")
        );
        assert_eq!(
            crate::cli::passthrough_trigger(&args(&["--capture=no", "--pdb"])),
            Some("--capture=no")
        );
        assert_eq!(crate::cli::passthrough_trigger(&args(&["-q", "-x"])), None);
    }

    #[test]
    fn order_ignored_warning_emits_one_actionable_message() {
        use pool::Order::{FailFast, Throughput};
        use RunPath::{Lazy, Passthrough, Pool, SingleWorker};
        let w = |dist, path| order_ignored_warning(FailFast, true, true, dist, path);
        // Passthrough names -s/--pdb, not -n (which may already be >= 2).
        let pt = w("load", Passthrough).unwrap();
        assert!(
            pt.contains("-s/--pdb") && !pt.contains("-n >= 2"),
            "got {pt}"
        );
        assert!(w("loadfile", SingleWorker)
            .unwrap()
            .contains("needs the parallel pool"));
        // --dist each: no dispatch queue, not "affinity order".
        let each = w("each", Pool).unwrap();
        assert!(each.contains("no dispatch queue"), "got {each}");
        // Lazy + affinity dist: ONE message naming both changes.
        let lazy_file = w("loadfile", Lazy).unwrap();
        assert!(
            lazy_file.contains("--collect full --dist load"),
            "got {lazy_file}"
        );
        assert!(!lazy_file.contains("affinity"));
        // Lazy + load: only the collect hint.
        assert!(w("load", Lazy).unwrap().ends_with("use --collect full"));
        // Full collection, affinity dist.
        assert!(w("loadscope", Pool)
            .unwrap()
            .contains("only reorders --dist load"));
        // Pool path on load, auto-pick, or throughput: silent.
        assert!(w("load", Pool).is_none());
        assert!(order_ignored_warning(FailFast, false, false, "load", Lazy).is_none());
        assert!(order_ignored_warning(Throughput, true, true, "load", Lazy).is_none());
    }

    #[test]
    fn order_ignored_warning_single_worker_quiet_for_config_order() {
        use pool::Order::FailFast;
        use RunPath::{Lazy, Passthrough, SingleWorker};
        // [tool.rstest] order (explicit, not from CLI): -n auto capping to 1 or
        // a -s run must not warn every time.
        assert!(order_ignored_warning(FailFast, true, false, "load", SingleWorker).is_none());
        assert!(order_ignored_warning(FailFast, true, false, "load", Passthrough).is_none());
        // A config-level misconfiguration on the pool path still warns.
        assert!(order_ignored_warning(FailFast, true, false, "load", Lazy).is_some());
    }

    #[test]
    fn check_order_shuffle_refuses_explicit_failfast_only() {
        use pool::Order::{FailFast, Throughput};
        assert!(check_order_shuffle(FailFast, true, true)
            .unwrap_err()
            .to_string()
            .contains("--shuffle cannot be combined"));
        // --watch auto-pick yields to --shuffle; no shuffle => no conflict.
        assert!(check_order_shuffle(FailFast, false, true).is_ok());
        assert!(check_order_shuffle(FailFast, true, false).is_ok());
        assert!(check_order_shuffle(Throughput, true, true).is_ok());
    }

    #[test]
    fn auto_lazy_picks_lazy_for_big_parallel_suite() {
        // 4000 tests * 8 workers well past the work floor => lazy.
        assert!(auto_lazy("load", &[], 8, 4000));
        assert!(auto_lazy("loadfile", &[], 8, 4000));
    }

    #[test]
    fn auto_lazy_stays_full_below_thresholds() {
        // Under the test floor, regardless of worker count.
        assert!(!auto_lazy("load", &[], 64, AUTO_LAZY_MIN_TESTS - 1));
        // Over the test floor but under the work floor (too few workers).
        assert!(!auto_lazy("load", &[], 2, AUTO_LAZY_MIN_TESTS + 1));
        // Serial run never goes lazy.
        assert!(!auto_lazy("load", &[], 1, 100_000));
    }

    #[test]
    fn collect_lazy_auto_vetoed_by_shuffle_or_incremental() {
        // Auto must not silently disable --incremental or turn --shuffle (which
        // errors under lazy) into a hard failure, even for a huge suite. No warm
        // cache in the test cwd, so the baseline is already full anyway; assert
        // the veto path returns full without touching the cache count.
        let mut c = cli();
        c.shuffle = Some("random".into());
        assert!(!collect_lazy(
            &c,
            &settings_collect(None),
            "load",
            std::path::Path::new("/nonexistent/python"),
            &[],
            64,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .unwrap());
        let mut c = cli();
        c.shard = Some("1/2".into());
        assert!(!collect_lazy(
            &c,
            &settings_collect(None),
            "load",
            std::path::Path::new("/nonexistent/python"),
            &[],
            64,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .unwrap());
        let mut c = cli();
        c.order = Some("fail-fast".into());
        assert!(!collect_lazy(
            &c,
            &settings_collect(None),
            "load",
            std::path::Path::new("/nonexistent/python"),
            &[],
            64,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .unwrap());
        let mut c = cli();
        c.incremental = true;
        assert!(!collect_lazy(
            &c,
            &settings_collect(None),
            "load",
            std::path::Path::new("/nonexistent/python"),
            &[],
            64,
            &DurationCache::default(),
            &mut Sink::captured().0
        )
        .unwrap());
    }

    #[test]
    fn auto_lazy_declines_when_file_affinity_breaks() {
        // Big suite, but nodeid/--pyargs selection and non-file-affine dists
        // can't ride the file walk => stay full.
        assert!(!auto_lazy("load", &["a.py::t".to_string()], 8, 100_000));
        assert!(!auto_lazy(
            "load",
            &["pkg.mod::t".to_string(), "--pyargs".to_string()],
            8,
            100_000
        ));
        assert!(!auto_lazy("loadscope", &[], 8, 100_000));
        assert!(!auto_lazy("loadgroup", &[], 8, 100_000));
    }

    #[test]
    fn auto_lazy_ignores_double_colons_in_option_values() {
        // R54: a `-W` filter or a `--deselect` nodeid is not a selection.
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for a in [
            args(&["-W", "default::pytest.PytestRemovedIn10Warning"]),
            args(&["--pythonwarnings", "error::DeprecationWarning"]),
            args(&["--deselect", "tests/a.py::t"]),
        ] {
            assert!(auto_lazy("load", &a, 8, 100_000), "{a:?}");
        }
        assert!(!auto_lazy(
            "load",
            &args(&["-W", "x::Y", "tests/a.py::t"]),
            8,
            100_000
        ));
    }

    fn timings(entries: &[(&str, f64)]) -> std::collections::HashMap<String, f64> {
        entries.iter().map(|&(id, s)| (id.to_string(), s)).collect()
    }

    fn walked(files: &[&str]) -> Vec<String> {
        files.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn lazy_layout_fits_an_even_suite_the_walk_reproduces() {
        let cache = timings(&[
            ("tests/test_a.py::t1", 1.0),
            ("tests/test_a.py::t2", 1.0),
            ("tests/test_b.py::t", 2.0),
        ]);
        assert!(lazy_layout_fits(
            &cache,
            &walked(&["tests/test_a.py", "tests/test_b.py"]),
            2,
            None
        ));
    }

    #[test]
    fn lazy_layout_rejects_items_the_walk_cannot_see() {
        // A doctest in a non-test module (or a --doctest-glob text file) was
        // collected eagerly but is not a python_files match: lazy would drop it.
        let cache = timings(&[("tests/test_a.py::t", 1.0), ("pkg/mod.py::pkg.mod.f", 1.0)]);
        assert!(!lazy_layout_fits(
            &cache,
            &walked(&["tests/test_a.py"]),
            1,
            None
        ));
    }

    #[test]
    fn lazy_layout_accepts_walked_files_without_timings() {
        // All-skipped, test-free or new files never get a call timing; the
        // lazy worker reports norecursedirs-pruned ones empty on its own.
        let cache = timings(&[("tests/test_a.py::t", 1.0)]);
        assert!(lazy_layout_fits(
            &cache,
            &walked(&["tests/test_a.py", "tests/test_skipped_on_linux.py"]),
            1,
            None
        ));
    }

    #[test]
    fn lazy_layout_rejects_a_file_longer_than_a_worker_share() {
        // 2 files, 8 workers: strict affinity leaves 6 workers idle.
        let cache = timings(&[("test_a.py::t", 10.0), ("test_b.py::t", 10.0)]);
        assert!(!lazy_layout_fits(
            &cache,
            &walked(&["test_a.py", "test_b.py"]),
            8,
            None
        ));
        // A fast suite's noisy millisecond skew is below the slack: fine.
        let cache = timings(&[("test_a.py::t", 0.03), ("test_b.py::t", 0.001)]);
        assert!(lazy_layout_fits(
            &cache,
            &walked(&["test_a.py", "test_b.py"]),
            8,
            None
        ));
        // One dominant file among many: the long pole serialises the run.
        let cache = timings(&[
            ("test_big.py::t", 50.0),
            ("test_c.py::t", 1.0),
            ("test_d.py::t", 1.0),
        ]);
        assert!(!lazy_layout_fits(
            &cache,
            &walked(&["test_big.py", "test_c.py", "test_d.py"]),
            2,
            None
        ));
    }

    #[test]
    fn lazy_measured_slower_only_on_a_comparable_slower_lazy_run() {
        use crate::scheduling::durations::{ModeWall, ModeWalls};
        let w = |secs: f64, workers: usize, tests: usize| ModeWall {
            secs,
            workers,
            tests,
            epoch: 0,
        };
        let walls = |full, lazy| ModeWalls {
            full: Some(full),
            lazy: Some(lazy),
        };
        // agent-platform-backend at 14 workers: lazy 52s against full 47s.
        assert!(lazy_measured_slower(
            &walls(w(47.0, 14, 10_133), w(52.0, 14, 10_133)),
            14
        ));
        // Within the noise margin, or lazy faster: keep picking lazy.
        assert!(!lazy_measured_slower(
            &walls(w(47.0, 14, 10_133), w(49.0, 14, 10_133)),
            14
        ));
        assert!(!lazy_measured_slower(
            &walls(w(44.0, 8, 193_843), w(34.0, 8, 193_841)),
            8
        ));
        // Another worker count, or a narrowed run: not comparable.
        assert!(!lazy_measured_slower(
            &walls(w(47.0, 14, 10_133), w(52.0, 14, 10_133)),
            8
        ));
        assert!(!lazy_measured_slower(
            &walls(w(47.0, 14, 10_133), w(52.0, 14, 412)),
            14
        ));
        // Nothing recorded for one mode yet: no verdict.
        let only_full = ModeWalls {
            full: Some(w(47.0, 14, 10_133)),
            lazy: None,
        };
        assert!(!lazy_measured_slower(&only_full, 14));
    }

    #[test]
    fn lazy_layout_weighs_files_by_test_count_too() {
        // packaging (corpus, 2026-11-17): 51,517 of 61,573 tests sit in one
        // file and their call times round to ~0. By call time alone the suite
        // looked balanced (0.4s over a share) and auto-lazy pinned 84% of the
        // tests on one worker: 18.1s against 10.4s under full collection.
        let mut cache: std::collections::HashMap<String, f64> = (0..51_517)
            .map(|i| (format!("tests/test_version.py::t[{i}]"), 0.0))
            .collect();
        for f in ["test_requirements", "test_markers", "test_specifiers"] {
            cache.extend((0..3_000).map(|i| (format!("tests/{f}.py::t[{i}]"), 0.0)));
        }
        let files = walked(&[
            "tests/test_version.py",
            "tests/test_requirements.py",
            "tests/test_markers.py",
            "tests/test_specifiers.py",
        ]);
        assert!(!lazy_layout_fits(&cache, &files, 14, None));
    }

    #[test]
    fn lazy_layout_lets_the_long_pole_use_the_collection_lazy_saves() {
        // pandas -n 14: the heaviest file runs 7.8s past its share, yet lazy
        // finished in 28s against 55s full, since each eager worker re-collects
        // the whole suite. With ~10s of per-worker collection recorded, the
        // pole fits the ~9.3s saved; without a record the 1s floor holds.
        let mut ids = vec![("test_big.py::t".to_string(), 17.4)];
        ids.extend((0..13).map(|i| (format!("test_{i}.py::t"), 9.0)));
        let cache: std::collections::HashMap<String, f64> = ids.into_iter().collect();
        let files: Vec<String> = cache
            .keys()
            .map(|id| id.split("::").next().unwrap().to_string())
            .collect();
        assert!(!lazy_layout_fits(&cache, &files, 14, None));
        assert!(lazy_layout_fits(&cache, &files, 14, Some(10.0)));
        // A cheap-to-collect suite saves too little to cover the same pole.
        assert!(!lazy_layout_fits(&cache, &files, 14, Some(2.0)));
    }

    #[test]
    fn requests_doctests_spots_both_doctest_flags() {
        assert!(requests_doctests(["-q", "--doctest-modules"].into_iter()));
        assert!(requests_doctests(["--doctest-glob=*.txt"].into_iter()));
        assert!(requests_doctests(["--doctest-glob", "*.rst"].into_iter()));
        assert!(!requests_doctests(
            ["-q", "--doctest-continue-on-failure"].into_iter()
        ));
    }

    #[test]
    fn auto_lazy_declines_on_path_selection() {
        // The cached count is suite-wide; a path selection (explicit or
        // --changed narrowing) may be one file, which strict affinity would
        // run on a single worker. `src` exists (cwd is the crate dir).
        assert!(!auto_lazy("load", &["src".to_string()], 8, 100_000));
        // Flags with on-disk values are not a selection.
        assert!(auto_lazy(
            "load",
            &["-k".to_string(), "src".to_string()],
            8,
            100_000
        ));
    }

    #[test]
    fn looponfail_is_refused_in_every_spelling() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for bad in [
            &["--looponfail"][..],
            &["-f"],
            &["-fv"],
            &["-vf"],
            &["tests/", "-f"],
        ] {
            let err = reject_looponfail(&args(bad)).unwrap_err().to_string();
            assert!(err.contains("--watch"), "{bad:?}: {err}");
        }
        // `f` as a value (-kf, -k f), a long flag, or after `--` is not the flag.
        for ok in [
            &["-kf"][..],
            &["-k", "f"],
            &["--ff"],
            &["--", "-f"],
            &["-v"],
        ] {
            assert!(reject_looponfail(&args(ok)).is_ok(), "{ok:?}");
        }
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
    fn parse_duration_secs_units_and_errors() {
        assert_eq!(parse_duration_secs("45").unwrap(), 45); // bare = seconds
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("5m").unwrap(), 300);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
        assert_eq!(parse_duration_secs("30d").unwrap(), 30 * 86400);
        assert_eq!(parse_duration_secs("1w").unwrap(), 7 * 86400);
        assert!(parse_duration_secs("10y").is_err()); // unknown unit
        assert!(parse_duration_secs("abc").is_err()); // no number
    }

    fn compact_segment(id: &str, generated_at: u64) -> remote::Segment {
        remote::Segment {
            scope_costs: Default::default(),
            schema: 1,
            id: id.into(),
            generated_at,
            durations: [(format!("{id}::t"), 0.1)].into_iter().collect(),
            flake_events: Vec::new(),
            cov_index: Default::default(),
            history: None,
            green: None,
        }
    }

    #[test]
    fn run_cache_compact_errors_without_a_remote() {
        // No --cache-remote flag and no RSTEST_CACHE_REMOTE => hard error, never
        // a silent no-op.
        let held = test_env::lock();
        let _env = test_env::remove_var(&held, "RSTEST_CACHE_REMOTE");
        let mut c = cli();
        c.cache_remote = None;
        let (mut sink, _cap) = Sink::captured();
        let err = run_cache_compact(&c, &mut sink, None, None).unwrap_err();
        assert!(
            err.to_string().contains("needs --cache-remote"),
            "got: {err}"
        );
    }

    #[test]
    fn run_cache_compact_folds_a_dir_remote_and_reports() {
        // A bare directory path resolves to a DirTransport; over-threshold loose
        // segments fold into a fresh base and are pruned, and the count is
        // reported. No retention window => fold all.
        use crate::remote::transport_for;
        let held = test_env::lock();
        let root = std::env::temp_dir().join(format!("rstest-run-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let remote_str = root.to_str().unwrap().to_string();
        let t = transport_for(&remote_str).unwrap();
        remote::push(t.as_ref(), &compact_segment("a", 1)).unwrap();
        remote::push(t.as_ref(), &compact_segment("b", 2)).unwrap();

        let _env = test_env::remove_var(&held, "RSTEST_CACHE_KEEP_LAST");
        let mut c = cli();
        c.cache_remote = Some(remote_str.clone());
        let (mut sink, cap) = Sink::captured();
        let code = run_cache_compact(&c, &mut sink, None, None).unwrap();
        assert_eq!(code, 0);
        assert!(cap.err().contains("compacted"), "got: {}", cap.err());

        let after = transport_for(&remote_str).unwrap();
        assert!(after.read_base().unwrap().is_some(), "base written");
        assert!(
            after.list_segment_ids().unwrap().is_empty(),
            "all folded (no retention window)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_retention_policy_flags_win_and_parse() {
        let _held = test_env::lock();
        let p = resolve_retention_policy(Some(5), Some("30d")).unwrap();
        assert_eq!(p.keep_last, Some(5));
        assert_eq!(p.max_age, Some(30 * 86400));
        // No flags, no env => fold-all (both None).
        assert_eq!(
            resolve_retention_policy(None, None).unwrap(),
            remote::RetentionPolicy::default()
        );
        // A bad duration flag is a hard error, never a silent fold-all.
        assert!(resolve_retention_policy(None, Some("nope")).is_err());
    }

    #[test]
    fn require_baseline_accepts_the_baseline_the_regress_gate_uses() {
        // B11: an edited test file stales its scheduling timing (`load` prunes
        // it), but `--durations-regress` still compares against it, so
        // `--require-baseline` must not call that a cold cache.
        let held = test_env::lock();
        let dir =
            std::env::temp_dir().join(format!("rstest-require-baseline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _env = test_env::set_var(&held, "RSTEST_CACHE", &dir);
        let cli = Cli::parse_from(["rstest", "--durations-regress", "2", "--require-baseline"]);
        // Cold: no durations.json at all.
        assert!(check_require_baseline(&cli, false).is_err());
        // Passthrough runs no gate, so there is nothing to require.
        assert!(check_require_baseline(&cli, true).is_ok());
        std::fs::write(
            dir.join("durations.json"),
            br#"{"gone.py::t":{"secs":1.0,"src":"rstest-no-such-dir/gone.py","hash":"00"}}"#,
        )
        .unwrap();
        assert!(crate::scheduling::durations::load().is_empty());
        assert!(check_require_baseline(&cli, false).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_retention_policy_reads_keep_last_env() {
        // No `keep_last` flag => fall through to RSTEST_CACHE_KEEP_LAST.
        let held = test_env::lock();
        let _env = test_env::set_var(&held, "RSTEST_CACHE_KEEP_LAST", "7");
        let p = resolve_retention_policy(None, None).unwrap();
        // A non-numeric env is a hard error, never a silent fold-all.
        let _env = test_env::set_var(&held, "RSTEST_CACHE_KEEP_LAST", "notnum");
        let err = resolve_retention_policy(None, None).unwrap_err();
        assert_eq!(p.keep_last, Some(7));
        assert!(err.to_string().contains("invalid RSTEST_CACHE_KEEP_LAST"));
    }

    #[test]
    fn dispatch_shard_verify_surfaces_an_unreadable_report() {
        // shard-verify is interpreter-free: a missing report errors straight
        // through dispatch, no Python resolved.
        let cli = Cli::parse_from(["rstest", "shard-verify", "/nonexistent/rstest-report.json"]);
        let err = dispatch_command(&cli, &[]).expect_err("missing report => error");
        assert!(err.to_string().contains("reading"), "{err}");
    }

    #[test]
    fn dispatch_resolves_python_only_for_the_modes_that_need_it() {
        // verify-vendor needs an interpreter: an unusable --python fails it in
        // dispatch, before the mode runs.
        let cli = Cli::parse_from(["rstest", "verify-vendor", "--python", "/nonexistent/python"]);
        let err = dispatch_command(&cli, &[]).expect_err("no interpreter => error");
        assert!(err.to_string().contains("no usable Python"), "{err}");
    }

    #[test]
    fn dispatch_cache_compact_without_remote_errors() {
        // cache-compact resolves the remote from the flag or RSTEST_CACHE_REMOTE;
        // with neither set it fails before touching Python or any transport.
        let held = test_env::lock();
        let _env = test_env::remove_var(&held, "RSTEST_CACHE_REMOTE");
        let cli = Cli::parse_from(["rstest", "cache-compact"]);
        let err = dispatch_command(&cli, &["rstest".into(), "cache-compact".into()])
            .expect_err("no remote => error");
        assert!(err
            .to_string()
            .contains("cache-compact needs --cache-remote"));
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
    fn network_guard_only_records_under_doctor() {
        let off = crate::config::RstestSettings::default();
        assert_eq!(super::network_guard(&off, false), None);
        let configured = crate::config::RstestSettings {
            network_allow: Some(vec!["redis".into()]),
            ..Default::default()
        };
        // The allowlist alone turns nothing on: there is no blocking mode.
        assert_eq!(super::network_guard(&configured, false), None);
        assert_eq!(
            super::network_guard(&configured, true),
            Some(crate::scheduling::worker::NetworkGuard {
                allow: vec!["redis".into()]
            })
        );
    }

    #[test]
    fn lazy_should_steal_only_on_explicit_load() {
        assert!(lazy_should_steal(Some("load"), None));
        assert!(lazy_should_steal(None, Some("load")));
        // Default (no explicit load) keeps strict file affinity.
        assert!(!lazy_should_steal(None, None));
        assert!(!lazy_should_steal(Some("loadfile"), Some("loadscope")));
        // Either source's `load` enables it; a CLI `loadfile` doesn't veto.
        assert!(lazy_should_steal(Some("loadfile"), Some("load")));
    }

    #[test]
    fn names_existing_path_counts_paths_and_nodeids_not_flags() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // Cargo runs unit tests from the crate root, where Cargo.toml exists.
        assert!(names_existing_path(&s(&["Cargo.toml"])));
        assert!(names_existing_path(&s(&["Cargo.toml::test_x"])));
        assert!(names_existing_path(&s(&["--pyargs", "some.pkg"])));
        assert!(!names_existing_path(&s(&["-q", "--maxfail=1"])));
        assert!(!names_existing_path(&s(&[
            "nope.py::test_x",
            "missing_dir"
        ])));
    }

    #[test]
    fn cap_workers_helpers_shrink_but_never_below_one() {
        // Files: never more workers than files; an empty walk is a no-op.
        assert_eq!(cap_workers_by_files(8, 3), 3);
        assert_eq!(cap_workers_by_files(8, 0), 8);
        assert_eq!(cap_workers_by_files(2, 5), 2);
        // Time: ~0.25s of work per worker, floored at 1.
        assert_eq!(cap_workers_by_time(8, 1.0, 0), 4); // ceil(1/0.25)=4
        assert_eq!(cap_workers_by_time(8, 0.1, 0), 1); // ceil(0.4)=1
        assert_eq!(cap_workers_by_time(8, 0.0, 0), 1); // never below 1

        // 50 trivial tests: 10ms of overhead, 1 worker.
        assert_eq!(cap_workers_by_time(8, 0.0, 50), 1);
        // 10,000 trivial tests record 0.0s each but cost ~2s of framework
        // overhead: they need workers, not 1 (-n auto ran 3x slower than -n 8).
        assert_eq!(cap_workers_by_time(14, 0.0, 10_000), 8);
    }

    #[test]
    fn auto_selection_counts_timed_tests_not_files() {
        use super::workers::{Selection, Timed};
        let root = std::env::temp_dir().join(format!("rstest-auto-sel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tests")).unwrap();
        for f in ["test_a.py", "test_b.py"] {
            std::fs::write(root.join("tests").join(f), "").unwrap();
        }
        let a = root.join("tests/test_a.py");
        let b = root.join("tests/test_b.py");
        let cache: std::collections::HashMap<String, f64> = (0..20)
            .map(|i| (format!("tests/test_a.py::test_io[{i}]"), 0.25))
            .collect();

        // One timed file: its 20 tests are the units, not 1 file.
        let sel = Selection::from_args(&root, &sv(&["tests/test_a.py"]));
        let t = sel.timed(std::slice::from_ref(&a), &cache, &root);
        assert_eq!((t.tests, t.cold, t.units()), (20, 0, 20));
        assert!((t.secs - 5.0).abs() < 1e-9);

        // An untimed file is one cold unit.
        let t = sel.timed(&[a.clone(), b.clone()], &cache, &root);
        assert_eq!((t.tests, t.cold), (20, 1));

        // A nodeid matches itself and its parametrizations only.
        let sel = Selection::from_args(&root, &sv(&["-k", "x", "tests/test_a.py::test_io"]));
        assert!(sel.paths.is_empty() && sel.nodeids.len() == 1);
        assert_eq!(sel.timed(&[], &cache, &root).units(), 20);
        let sel = Selection::from_args(&root, &sv(&["tests/test_a.py::test_io[3]"]));
        assert_eq!(sel.timed(&[], &cache, &root).units(), 1);
        let sel = Selection::from_args(&root, &sv(&["tests/test_b.py::test_x"]));
        assert_eq!(
            sel.timed(&[], &cache, &root),
            Timed {
                tests: 0,
                secs: 0.0,
                cold: 1
            }
        );
        let _ = std::fs::remove_dir_all(&root);
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
            subtest: false,
            crash: None,
            cause: None,
            xfail_reason: None,
            skip_location: None,
            network: Vec::new(),
            marker_timeout: None,
            no_rerun: false,
            subtest_desc: None,
            subtest_shown: false,
        }
    }

    #[test]
    fn mark_session_flaky_marks_only_tests_that_recovered() {
        let mut run = report::Run::default();
        run.record(None, report("t.py::recovered", "passed"));
        run.record(None, report("t.py::exhausted", "failed"));
        mark_session_flaky(
            &mut run,
            vec![
                "t.py::recovered".into(),
                "t.py::exhausted".into(),
                "t.py::recovered".into(),
            ],
        );
        assert_eq!(run.flaky, vec![("t.py::recovered".to_string(), 2)]);
        let counts = run.counts();
        assert_eq!(counts["flaky"], 1);
        assert_eq!(counts["failed"], 1);
        assert_eq!(counts.get("passed").copied().unwrap_or(0), 0);
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
                    constant: false,
                    repeated: false,
                    redundant: 0.0,
                    fingerprint: None,
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
                    location: String::new(),
                    text: String::new(),
                }]
            }),
            None
        );
        // A scheduling-only event is a no-op in a single session.
        assert_eq!(
            fold(proto::Event::ItemStart {
                index: 0,
                timeout: None
            }),
            None
        );
        // Done terminates with the exit status.
        assert_eq!(
            fold(proto::Event::Done {
                exitstatus: 1,
                peak_rss: None,
                imported: vec![],
                sys_path: vec![],
                coverage: false,
                rewrite: vec![],
                env: vec![],
            }),
            Some(1)
        );

        assert_eq!(run.collect_skips, 1);
        assert_eq!(warnings.len(), 1);
        assert_eq!(fixtures.len(), 1);
    }

    #[test]
    fn fold_run_event_collection_count_drives_the_percentage() {
        // The single session reports its collected count, so -v lines carry
        // pytest's `[ NN%]` column just like `pytest -v`.
        let mut run = report::Run::default();
        let mut prog = progress::Progress::default();
        prog.set_mode(progress::Mode::Verbose);
        let mut fixtures = Vec::new();
        let mut warnings = Vec::new();
        let (mut sink, cap) = Sink::captured();
        let collected: proto::Event = serde_json::from_value(serde_json::json!({
            "kind": "collection_done",
            "payload": {"count": 2, "hash": ""},
        }))
        .unwrap();
        for ev in [
            collected,
            proto::Event::Report(report("t.py::a", "passed")),
            proto::Event::Report(report("t.py::b", "passed")),
        ] {
            fold_run_event(
                ev,
                false,
                &mut run,
                &mut prog,
                &mut fixtures,
                &mut warnings,
                &mut sink,
            );
        }
        let out = cap.out();
        assert!(out.contains("t.py::a PASSED [ 50%]"), "{out}");
        assert!(out.contains("t.py::b PASSED [100%]"), "{out}");
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
        // A path under a regular file can't be created => warn, no panic.
        let blocker =
            std::env::temp_dir().join(format!("rstest-streamjson-file-{}", std::process::id()));
        std::fs::write(&blocker, b"").unwrap();
        let path = blocker.join("out.ndjson");

        let (mut sink, captured) = Sink::captured();
        attach_stream_json(&mut sink, &path);

        let err = captured.err();
        assert!(err.contains("rstest: --stream-json"), "got: {err}");
        assert!(err.contains(&path.display().to_string()), "got: {err}");
        // Nothing was attached, so emitting is a no-op (no panic writing to a
        // closed/absent stream).
        sink.emit_event(serde_json::json!({"event": "sessionfinish"}));
    }

    #[test]
    fn banner_names_single_worker_mode() {
        let (mut sink, cap) = Sink::captured();
        print_run_banner(progress::Mode::Dots, false, false, 0, &mut sink);
        let line = cap.out();
        assert!(line.contains("— single-worker mode"), "got {line}");
        assert!(!line.contains("pytest-exact"), "got {line}");
    }
}
