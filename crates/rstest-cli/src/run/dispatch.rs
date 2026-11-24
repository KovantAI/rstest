//! Dispatch of one resolved run to its execution path (pool, lazy pool or a
//! single session), with the order, shuffle and shard resolution it needs.

use anyhow::Result;

use super::lazy::{lazy_roots, lazy_should_steal, DurationCache};
use super::RunConfig;
use crate::cli::{parse_maxfail, Cli};
use crate::reporting::sink::Sink;
use crate::reporting::{progress, report};
use crate::scheduling::orchestrator::{self, Dedup};
use crate::scheduling::{lazy, pool, proto, shard, worker};
use crate::{collect, config, incremental, select};

/// Dispatch-time selection modifiers, bundled so [`dispatch_run`] stays within
/// the argument budget: the resolved `--shuffle` seed, `--shard` bucket,
/// lazy-collection choice, `--quarantine` matcher (fail-fast keeps quarantined
/// tests out of the front), the run's shared duration cache, and the env
/// fingerprint (fail-fast diffs against the last green run recorded under it).
#[derive(Clone, Copy)]
pub(super) struct DispatchSelection<'a> {
    pub(super) shuffle_seed: Option<u64>,
    pub(super) shard: Option<(usize, usize)>,
    pub(super) budget: Option<&'a pool::Budget>,
    pub(super) lazy: bool,
    pub(super) quarantine: Option<&'a super::gates::Quarantine>,
    pub(super) durations: &'a DurationCache,
    pub(super) env_fp: &'a str,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_run(
    cfg: &RunConfig,
    cli: &Cli,
    settings: &config::RstestSettings,
    args: &[String],
    skip_ids: &std::collections::HashSet<String>,
    selection: DispatchSelection<'_>,
    pinned: Option<&crate::replay::PinnedSchedule>,
    sink: &mut Sink,
) -> Result<pool::PoolOutcome> {
    let DispatchSelection {
        shuffle_seed,
        shard,
        budget,
        lazy,
        quarantine,
        durations: duration_cache,
        env_fp,
    } = selection;
    let RunConfig {
        n,
        reruns,
        worker_timeout,
        passthrough,
        single_worker_reruns,
        mode,
        durations,
        order,
        order_explicit,
        ref dist_name,
        ref known_flaky,
        ref worker_env,
        ref python,
        ..
    } = *cfg;
    let python = python.as_path();
    let worker_env: &worker::WorkerEnv = worker_env;
    let known_flaky = known_flaky.as_ref();
    // Replay pins a recorded schedule, which only the eager pool can honor.
    // The command layer forces `-n <recorded>` + `--collect full` and no
    // passthrough flags, so this is a guard against a stray incompatible arg.
    if pinned.is_some() && (passthrough || n <= 1) {
        anyhow::bail!(
            "replay needs the parallel pool (-n >= 2, no passthrough flags like -s/--pdb); \
             the journal was recorded from a pool run"
        );
    }
    // Only the explicit cap travels here: without one, the pool sizes each
    // test's watchdog from the timeout the worker reports at item_start.
    let watchdog = worker_timeout.map(std::time::Duration::from_secs);
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
        fork_prewarm: cli.fork_pool.is_some(),
        quarantine,
    };
    let path = if passthrough {
        RunPath::Passthrough
    } else if n <= 1 && !single_worker_reruns {
        RunPath::SingleWorker
    } else if lazy {
        RunPath::Lazy
    } else {
        RunPath::Pool
    };
    if let Some(w) =
        order_ignored_warning(order, order_explicit, cli.order.is_some(), dist_name, path)
    {
        sink.warn(&w);
    }
    Ok(
        if matches!(path, RunPath::Passthrough | RunPath::SingleWorker) {
            // Byte-exact output: pytest's own terminal writer prints.
            let io = if passthrough || mode == progress::Mode::Pytest {
                worker::Stdio::Inherit
            } else {
                worker::Stdio::Null
            };
            let mut w = worker::Worker::spawn_with_io(python, None, io, worker_env)?;
            // Byte-exact session: survive SIGINT/SIGTERM so the reports are
            // still written (see `SingleWorkerInterrupts`). Passthrough (`-s`,
            // `--pdb`) keeps the default action: there the child owns the
            // console and a debugger uses Ctrl-C itself.
            let interrupts =
                (path == RunPath::SingleWorker).then(|| SingleWorkerInterrupts::install(w.pid()));
            w.send(&proto::Command::RunTests {
                args: args.to_vec(),
            })?;
            let mut run = report::Run::default();
            run.track_phase_durations = durations.is_some();
            let mut prog = progress::Progress::default();
            prog.set_mode(mode);
            let mut fixtures: Vec<proto::FixtureStat> = Vec::new();
            let mut warnings: Vec<proto::WarningEntry> = Vec::new();
            let mut session_died = false;
            // Failed attempts of flaky-marked tests the session retried
            // (outcome `rerun`), per nodeid: see `mark_session_flaky`.
            let mut reruns_seen: Vec<String> = Vec::new();
            let mut session_peak: Option<u64> = None;
            let exitstatus = loop {
                let event = match w.recv() {
                    Ok(event) => event,
                    // The session died under the signal (a process-group
                    // SIGTERM reaches the child too, and Python's default
                    // action kills it): keep what it reported and wind down
                    // as INTERRUPTED.
                    Err(e) => match crate::scheduling::interrupt::requested() {
                        Some(sig) if interrupts.is_some() => {
                            sink.warn(&format!(
                                "rstest: interrupted by {}; the pytest session stopped \
                                 before its summary",
                                crate::scheduling::interrupt::name(sig)
                            ));
                            session_died = true;
                            break 2; // pytest INTERRUPTED
                        }
                        _ => return Err(e),
                    },
                };
                if let proto::Event::Done {
                    peak_rss: Some(p), ..
                } = &event
                {
                    session_peak = Some(*p);
                }
                if let (proto::Event::Interrupted {}, Some(i)) = (&event, &interrupts) {
                    i.session_got_sigint();
                }
                if let proto::Event::Report(r) = &event {
                    if r.outcome == "rerun" {
                        // A retried attempt: pytest's own terminal shows it
                        // (`R`, `N rerun`); the run record keeps the final one.
                        reruns_seen.push(r.nodeid.clone());
                        continue;
                    }
                }
                if let Some(code) = fold_run_event(
                    event,
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
            // Stop forwarding before the child is reaped (its pid is then free
            // for reuse), and restore the default handlers.
            drop(interrupts);
            mark_session_flaky(&mut run, reruns_seen);
            // Reaped after the post-run saves (see `PoolOutcome::exiting`).
            let exiting = if session_died {
                w.reap();
                Vec::new()
            } else {
                w.send(&proto::Command::Shutdown)?;
                vec![w]
            };
            pool::PoolOutcome {
                fixture_groups: Vec::new(),
                run,
                prog,
                fixtures,
                warnings,
                cache_dir: None,
                exitstatus,
                // Single-worker path never shards (resolve_shard rejects it).
                collection_hash: None,
                collection_size: 0,
                // A single session reports no collection; the duration cache
                // falls back to its saved rootdir and current file contents.
                sources: Default::default(),
                // No worker pool spawned here (one passthrough worker).
                startup_seconds: 0.0,
                fork_prewarmed: false,
                warm_record: Default::default(),
                warm: None,
                // Recorded only when `-n auto` chose this one session (as when
                // memory capped it at 1): then it is the worker's peak, and a
                // later run on more memory can size up from it. An explicit
                // `-n 0` runs the whole suite in one process, a peak that
                // would undersize the next parallel run.
                peak_rss: session_peak.filter(|_| cfg.numprocesses == "auto"),
                // One session collects once; nothing a lazy pool would save.
                collect_seconds: None,
                lazy: false,
                exiting,
            }
        } else if path == RunPath::Lazy {
            // Auto-picked (neither --collect nor the setting present)? Say so once,
            // here at the single dispatch site, so the choice is observable.
            if cli.collect.is_none() && settings.collect.is_none() {
                sink.warn(&format!(
                    "rstest: auto-selected lazy collection ({} known tests x {n} workers); \
                     pass --collect full to force eager collection",
                    duration_cache.get().len()
                ));
            }
            let cwd = std::env::current_dir()?;
            let project = config::discover(&cwd, sink.err());
            // `collect_lazy` already resolved these (memoized), so Some.
            let paths = lazy_roots(python, args).unwrap_or_default();
            let mut files = collect::collect_test_files(&paths, &project)?;
            if let Some((k, total)) = shard {
                let before = files.len();
                files = shard::shard_files(
                    &files,
                    duration_cache.get(),
                    &cwd,
                    &crate::cache::base_dir(),
                    k,
                    total,
                );
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
                duration_cache.get(),
                durations.is_some(),
                // Steal (split files across workers) only on an EXPLICIT --dist
                // load: lazy defaults to strict file affinity, since stealing
                // exposes cross-file/in-file order dependence affinity doesn't.
                lazy_should_steal(cli.dist.as_deref(), settings.dist.as_deref()),
                // A shard is a partial suite: no journal (same as run_pool).
                shard.is_none().then_some(dist_name.as_str()),
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
            // Only fail-fast load dispatch and --budget read it; the diff
            // costs git calls and a graph walk, so every other run skips it.
            let near = if pinned.is_none()
                && (budget.is_some() || order == pool::Order::FailFast && dist == pool::Dist::Load)
            {
                near_changes(env_fp)
            } else {
                std::collections::HashMap::new()
            };
            pool::run_pool(
                &base_cfg,
                dist,
                order,
                durations.is_some(),
                shuffle_seed,
                shard,
                skip_ids,
                near,
                budget,
                pinned,
                sink,
            )?
        },
    )
}

/// [`select::near_changes`] for the project at the cwd, diffed against the last
/// green commit when one is recorded under `env_fp` (so the whole branch's
/// work counts, not just uncommitted edits), else HEAD. Same rootdir rule as
/// `--changed`: with no config file, the cache's base dir.
pub(super) fn near_changes(env_fp: &str) -> std::collections::HashMap<String, u8> {
    let Ok(cwd) = std::env::current_dir() else {
        return std::collections::HashMap::new();
    };
    // discover()'s config warnings were already shown by this run.
    let mut project = config::discover(&cwd, &mut std::io::sink());
    if project.inifile.is_none() {
        project.rootdir = crate::cache::base_dir();
    }
    let base = incremental::baseline(&cwd, env_fp);
    select::near_changes(&project.rootdir, &project, base.as_deref())
}

/// SIGINT/SIGTERM during the byte-exact single-worker session. With the
/// default action the orchestrator died on Ctrl-C while pytest, in the same
/// process group, was still printing its KeyboardInterrupt summary: exit by
/// signal, output cut off, no `--report-json`/`--junitxml`.
///
/// While installed, the signal is only recorded (see
/// [`crate::scheduling::interrupt`]); the orchestrator keeps reading the
/// session, which ends with pytest's own INTERRUPTED (2), then writes the
/// reports as usual. SIGINT is forwarded only when the session does not say
/// it got one within [`SIGINT_GRACE`]: Ctrl-C and a CI runner's cancel signal
/// the whole process group, so pytest already has it (and reports
/// `Interrupted` from its KeyboardInterrupt hook), and a second SIGINT would
/// abort pytest's teardown and summary. Silence means it went to our pid alone
/// (`kill -INT PID`), even while a test hangs: pass it on. SIGTERM is
/// forwarded to the session as SIGINT: `kill PID` and job timeouts often
/// target only our pid, and Python has no clean SIGTERM unwinding. When the group got SIGTERM
/// too, the child is already gone and the caller winds down from what it
/// reported. A second signal still exits at once.
pub(super) struct SingleWorkerInterrupts {
    pub(super) _guard: crate::scheduling::interrupt::Guard,
    pub(super) stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The session reported a SIGINT of its own (see
    /// [`Self::session_got_sigint`]).
    pub(super) got_sigint: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(super) forwarder: Option<std::thread::JoinHandle<()>>,
}

/// How long after our SIGINT the session has to report one of its own (a
/// group-wide signal reaches it at the same time) before it is passed on.
#[cfg(unix)]
pub(super) const SIGINT_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

impl SingleWorkerInterrupts {
    pub(super) fn install(child: Option<u32>) -> Self {
        use std::sync::atomic::AtomicBool;
        let guard = crate::scheduling::interrupt::Guard::install();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let got_sigint = std::sync::Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        let forwarder = child.map(|pid| {
            let stop = std::sync::Arc::clone(&stop);
            let got_sigint = std::sync::Arc::clone(&got_sigint);
            std::thread::spawn(move || {
                use std::sync::atomic::Ordering::SeqCst;
                // When our SIGINT was seen; None until one arrives.
                let mut sigint: Option<std::time::Instant> = None;
                while !stop.load(SeqCst) {
                    let forward = match crate::scheduling::interrupt::requested() {
                        Some(libc::SIGTERM) => true,
                        Some(libc::SIGINT) => {
                            let at = sigint.get_or_insert_with(std::time::Instant::now);
                            at.elapsed() >= SIGINT_GRACE && !got_sigint.load(SeqCst)
                        }
                        _ => false,
                    };
                    if forward {
                        // SAFETY: sending a signal touches no memory. The pid
                        // is our unreaped child (the caller drops this guard,
                        // joining the thread, before reaping it), so it cannot
                        // have been recycled.
                        unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
                        return;
                    }
                    // Parked, not slept: Drop unparks so its join returns at once
                    // instead of waiting out the tick after the run.
                    std::thread::park_timeout(std::time::Duration::from_millis(50));
                }
            })
        });
        #[cfg(not(unix))]
        let forwarder = {
            let _ = child;
            None
        };
        Self {
            _guard: guard,
            stop,
            got_sigint,
            forwarder,
        }
    }

    /// The session caught a SIGINT itself: never send it another.
    pub(super) fn session_got_sigint(&self) {
        self.got_sigint
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for SingleWorkerInterrupts {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.forwarder.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// Resolve dispatch ordering (CLI > [tool.rstest] > auto). Auto picks
/// fail-fast under `--watch` (surface a red as fast as possible on each save)
/// and `--budget` (what it picks for risk runs first), throughput otherwise. Where the order has no effect is reported once, at
/// dispatch, by [`order_ignored_warning`].
pub(super) fn resolve_order(cli: &Cli, settings: &config::RstestSettings) -> Result<pool::Order> {
    Ok(match cli.order.as_deref().or(settings.order.as_deref()) {
        Some(s) => s.parse::<pool::Order>().map_err(|e| anyhow::anyhow!(e))?,
        None if cli.watch || cli.budget.is_some() => pool::Order::FailFast,
        None => pool::Order::Throughput,
    })
}

/// Which run path [`dispatch_run`] takes, as far as `--order` cares.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum RunPath {
    /// `-s`/`--pdb`/`--co`/`--debug`: one worker, inherited stdio.
    Passthrough,
    /// `-n <= 1` (explicit, or `auto` capped down on a small suite).
    SingleWorker,
    /// Parallel pool with `--collect lazy`.
    Lazy,
    /// Parallel pool with full collection: the only path that reorders.
    Pool,
}

/// Heads-up when an explicit `--order fail-fast` lands on a run path that
/// never builds a `--dist load` dispatch queue. At most ONE warning, and it
/// names the actual cause, so following it enables the reordering. The
/// `--watch` auto-pick stays quiet: it was never asked for. The single-worker
/// paths warn only for an `--order` typed on the command line (`from_cli`): a
/// `[tool.rstest] order` means "when it applies", and `-n auto` legitimately
/// drops to one worker on small suites, so warning there would fire every run.
pub(super) fn order_ignored_warning(
    order: pool::Order,
    explicit: bool,
    from_cli: bool,
    dist_name: &str,
    path: RunPath,
) -> Option<String> {
    if !explicit || order != pool::Order::FailFast {
        return None;
    }
    let msg = if path == RunPath::Passthrough {
        if !from_cli {
            return None;
        }
        "rstest: --order fail-fast has no effect under -s/--pdb/--co/--debug \
         (one worker runs the session in its own order)"
            .to_string()
    } else if path == RunPath::SingleWorker {
        if !from_cli {
            return None;
        }
        "rstest: --order fail-fast needs the parallel pool (-n >= 2); \
         single-worker mode runs in session order"
            .to_string()
    } else if dist_name == "each" {
        // Pool path only: collect_lazy rejects --dist each, so Lazy never
        // reaches here with it.
        "rstest: --order fail-fast has no effect with --dist each (every worker \
         runs the full suite; there is no dispatch queue)"
            .to_string()
    } else if path == RunPath::Lazy {
        let dist_hint = if dist_name == "load" {
            ""
        } else {
            " --dist load"
        };
        format!(
            "rstest: --order fail-fast is ignored under --collect lazy (lazy \
             dispatches whole files); use --collect full{dist_hint}"
        )
    } else if dist_name != "load" {
        format!(
            "rstest: --order fail-fast only reorders --dist load; --dist {dist_name} \
             keeps its affinity order"
        )
    } else {
        return None;
    };
    Some(msg)
}

/// An explicit fail-fast order and `--shuffle` both claim the dispatch queue;
/// refuse rather than let the shuffle silently discard the requested order.
/// The `--watch` auto-pick yields to `--shuffle` without complaint.
pub(super) fn check_order_shuffle(
    order: pool::Order,
    explicit: bool,
    shuffled: bool,
) -> Result<()> {
    if shuffled && explicit && order == pool::Order::FailFast {
        anyhow::bail!(
            "--shuffle cannot be combined with --order fail-fast (the shuffle \
             replaces the dispatch order); drop one of them"
        );
    }
    Ok(())
}

/// Resolve `--shuffle` into an optional dispatch seed. `--shuffle` needs the
/// parallel pool (`-n >= 2`, not passthrough), can't ride the file-affine lazy
/// collector, and is meaningless under `--dist each`; a `random` value stamps a
/// time+pid seed, otherwise the value must parse as `u64`. Prints the resolved
/// seed to `w` so a shuffled run is reproducible.
pub(super) fn resolve_shuffle_seed(
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
pub(super) fn resolve_shard(
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

/// Mark the tests a single session retried flaky when their final attempt
/// passed, as the pool does. The session retries `@pytest.mark.flaky` tests
/// itself (rstest's own reruns, or pytest-rerunfailures when installed) and
/// reports each retried attempt with outcome `rerun`; `reruns` holds one
/// nodeid per such attempt.
pub(super) fn mark_session_flaky(run: &mut report::Run, reruns: Vec<String>) {
    let mut counts: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for id in reruns {
        *counts.entry(id).or_default() += 1;
    }
    for (id, attempts) in counts {
        if run.tests().get(&id).is_some_and(|e| !e.any_phase_failed()) {
            run.mark_flaky(id, attempts);
        }
    }
}

/// Fold one worker event into the single-session accumulators (the byte-exact /
/// passthrough / one-worker-rerun path). Returns `Some(exitstatus)` on `Done`.
/// Reports drive progress (suppressed under passthrough, whose IO is inherited)
/// and the run record; collect errors/skips, doctor fixtures, and warnings
/// accumulate. `CollectionDone` sets the progress total. Other scheduling /
/// lazy events are no-ops in a single session,
/// enumerated (not `_`) so a new event type forces a decision here.
pub(super) fn fold_run_event(
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
        e @ (proto::Event::DoctorFixtures { .. }
        | proto::Event::ScopeCosts { .. }
        | proto::Event::FixtureLeaks { .. }
        | proto::Event::JunitCase { .. }
        | proto::Event::JunitSuite { .. }
        | proto::Event::Warnings { .. }
        | proto::Event::CollectSkip { .. }
        | proto::Event::LfRecollected { .. }) => {
            orchestrator::fold_bookkeeping(e, Dedup::Single, run, fixtures, warnings);
            None
        }
        proto::Event::CollectionDone {
            count, deselected, ..
        } => {
            // The single session reports its collected count so the dots and
            // -v renderers print pytest's `[ NN%]` column.
            prog.set_total(count as usize);
            run.cap_total = Some(count as usize);
            run.deselected = deselected;
            None
        }
        proto::Event::NodeInput { .. }
        | proto::Event::ItemStart { .. }
        | proto::Event::ItemDone { .. }
        | proto::Event::Stopped { .. }
        // Pool-only: a single session runs pytest's own loop, which handles
        // pytest.exit() itself.
        | proto::Event::SessionExit { .. }
        | proto::Event::LazyReady { .. }
        | proto::Event::FileCollected { .. }
        | proto::Event::ItemStartId { .. }
        | proto::Event::ItemDoneId { .. }
        | proto::Event::StoppedIds { .. }
        // Read by the dispatch loop (`SingleWorkerInterrupts`).
        | proto::Event::Interrupted {} => None,
        proto::Event::Done { exitstatus, .. } => Some(exitstatus),
    }
}
