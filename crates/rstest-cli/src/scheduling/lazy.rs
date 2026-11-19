//! Lazy-collection pool (D5 single-point collection, `--collect lazy`).
//!
//! Workers run sessions with NO initial collection pass; the orchestrator
//! orders FILES by cached duration and assigns them, each collected on
//! demand by EXACTLY ONE process (no mismatch class, no per-worker collect).
//!
//! Item identity on the wire is the NODEID: lazy workers share no index
//! space. Reruns, crash redistribution, and the serial phase travel as
//! RunIds; a worker re-collects the relevant FILE for an unseen nodeid.
//!
//! DISPATCH: chunks go back via RunIds, normally to the owner (items cached
//! there). Once files run out an idle worker STEALS the longest queue (one
//! re-collection); else giant parametrize-heavy files pin single workers.
//!
//! --dist loadscope/loadgroup need cross-file consolidation over a global
//! id list, which lazy mode never builds; they are rejected at the CLI.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::Result;

use crate::reporting::progress::Progress;
use crate::reporting::report::Run;
use crate::reporting::sink::Sink;
use crate::scheduling::orchestrator::{self, Dedup, PoolCtl};
use crate::scheduling::pool::PoolOutcome;
use crate::scheduling::proto::{self, Event};
use crate::scheduling::reruns::Reruns;
use crate::scheduling::worker::Worker;

struct WorkerState {
    worker: Worker,
    ready: bool,
    finishing: bool,
    ended: bool,
    dead: bool,
    /// File paths assigned and not yet collected (reclaimed on crash).
    uncollected_files: Vec<String>,
    /// Ids collected by this worker, not yet dispatched anywhere
    /// (dispatch prefers the owner - items are cached there).
    own_queue: VecDeque<String>,
    /// Nodeids dispatched here (RunIds) and not done.
    outstanding: Vec<String>,
    running: Option<String>,
    running_since: Option<std::time::Instant>,
    running_watchdog: Option<orchestrator::Watchdog>,
    timeout_killed: bool,
    attempt: Vec<proto::Report>,
    attempt_failed: bool,
}

impl orchestrator::Slot for WorkerState {
    fn dead(&self) -> bool {
        self.dead
    }
    fn set_finishing(&mut self, v: bool) {
        self.finishing = v;
    }
    fn timeout_killed(&self) -> bool {
        self.timeout_killed
    }
    fn set_timeout_killed(&mut self) {
        self.timeout_killed = true;
    }
    fn running_since(&self) -> Option<std::time::Instant> {
        self.running_since
    }
    fn running_watchdog(&self) -> Option<orchestrator::Watchdog> {
        self.running_watchdog
    }
    fn kill_worker(&mut self) {
        self.worker.kill();
    }
    fn send_stop_run(&mut self) {
        let _ = self.worker.send(&proto::Command::StopRun);
    }
    fn reap_dead(&mut self) {
        self.worker.reap();
        self.dead = true;
    }
}

impl WorkerState {
    fn fresh(worker: Worker) -> Self {
        Self {
            worker,
            ready: false,
            finishing: false,
            ended: false,
            dead: false,
            uncollected_files: Vec::new(),
            own_queue: VecDeque::new(),
            outstanding: Vec::new(),
            running: None,
            running_since: None,
            running_watchdog: None,
            timeout_killed: false,
            attempt: Vec::new(),
            attempt_failed: false,
        }
    }
}

/// A file at least `1 / POLE_SHARE` of a worker's share of the cached time is
/// a long pole and goes first. Measured 2026-11-17, warm cache, against the
/// all-by-weight order: 16 kept pandas (`-n 8`) at 35.5s against 36.2s and cut
/// agent-platform-backend (14 workers) from 65.1s to 52.7s; 4 cut that suite
/// to 49.3s but cost pandas 2.6s (only 2 of its files counted as poles, so its
/// many medium files started late), 8 sat in between.
const POLE_SHARE: usize = 16;

/// Order files for dispatch: the long poles first, biggest first, so they
/// can't end the run late; every other file in path order (the walk's), so
/// neighbouring files, which tend to share imports and fixtures, reach the
/// same worker. A long pole is a file whose cached total is at least
/// `total / (POLE_SHARE * workers)`. Sorting every file by weight scattered
/// related files over all workers, so each paid their one-time setup.
/// Returns cwd-relative paths (what workers are sent) and the known totals by
/// path.
fn order_files(
    files: Vec<PathBuf>,
    cache: &HashMap<String, f64>,
    cwd: &Path,
    root: &Path,
    workers: usize,
) -> (Vec<String>, HashMap<String, f64>) {
    // The duration cache keys on nodeids relative to the rootdir; group
    // totals by the file prefix.
    let mut totals: HashMap<String, f64> = HashMap::new();
    for (id, secs) in cache {
        let file = id.split("::").next().unwrap_or(id);
        *totals.entry(file.to_string()).or_insert(0.0) += secs;
    }
    let mut weights: HashMap<String, f64> = HashMap::new();
    let mut walked: Vec<String> = Vec::new();
    for f in files {
        let rel = crate::scheduling::durations::file_key(&f, cwd, cwd);
        if let Some(&t) = totals.get(&crate::scheduling::durations::file_key(&f, cwd, root)) {
            weights.insert(rel.clone(), t);
        }
        walked.push(rel);
    }
    let cut = weights.values().sum::<f64>() / (POLE_SHARE * workers.max(1)) as f64;
    let is_pole = |f: &String| weights.get(f).is_some_and(|&w| w > 0.0 && w >= cut);
    let mut poles: Vec<String> = walked.iter().filter(|f| is_pole(f)).cloned().collect();
    poles.sort_by(|a, b| weights[b].total_cmp(&weights[a]));
    let rest = walked.into_iter().filter(|f| !is_pole(f));
    let order = poles.into_iter().chain(rest).collect();
    (order, weights)
}

/// Files per RunFiles: a quarter of a fair share of what's left, by cached
/// seconds where known, else by count (the cap either way). By count alone,
/// heaviest-first order gave the first worker to ask the heaviest files in
/// one batch (pandas: 28 files, 40% of the tests). The worker collects a
/// batch in one `perform_collect`, which lists every directory on the way,
/// so one file per call is quadratic in files per directory; the shrinking
/// batches keep the tail balanced.
fn file_batch(queue: &VecDeque<String>, weights: &HashMap<String, f64>, workers: usize) -> usize {
    let parts = workers.max(1) * 4;
    let share = queue.iter().filter_map(|f| weights.get(f)).sum::<f64>() / parts as f64;
    let mut take = 0;
    let mut acc = 0.0;
    for f in queue.iter().take(queue.len().div_ceil(parts)) {
        if take > 0 && share > 0.0 && acc >= share {
            break;
        }
        acc += weights.get(f).copied().unwrap_or(0.0);
        take += 1;
    }
    take
}

/// Whether a worker takes another file batch: nothing left to collect and
/// little left to run, counting its collected-but-undispatched ids. A busy
/// worker must not hoard files an idle one could collect: lazy keeps file
/// affinity by default, so nothing moves them later (pandas: one worker sat
/// on 78k queued ids and kept taking files).
fn wants_files(uncollected: usize, own_queue: usize, outstanding: usize, chunk: usize) -> bool {
    uncollected == 0 && own_queue <= chunk && outstanding <= chunk
}

pub fn run_lazy_pool(
    cfg: &crate::scheduling::pool::PoolConfig,
    files: Vec<PathBuf>,
    // The run's duration cache (orders files, longest first).
    duration_cache: &HashMap<String, f64>,
    // `--durations N` asked for: record per-phase timings for the report.
    track_durations: bool,
    // --dist loadfile => steal=false: strict file affinity, the remedy
    // for order-dependent suites (same contract as the full pool).
    steal: bool,
    // `--dist` name to record in the replay journal, or `None` to write none
    // (a shard: partial suite, same rule as the full pool).
    journal_dist: Option<&str>,
    sink: &mut Sink,
) -> Result<PoolOutcome> {
    let &crate::scheduling::pool::PoolConfig {
        python,
        n,
        args,
        mode,
        maxfail,
        reruns,
        only_rerun,
        worker_timeout,
        known_flaky,
        worker_env,
        fork_prewarm,
        // Lazy never reorders by flake history; quarantine only shapes the
        // -x / --maxfail count.
        quarantine,
    } = cfg;
    let (tx, rx) = mpsc::channel::<(usize, Result<Event>)>();
    // Fork-prewarm the initial pool off one warm zygote when asked (Unix);
    // otherwise n independent spawns. Each worker then gets its lazy-session
    // command + reader via start_into. Time the spawn for --doctor startup.
    let spawn_start = std::time::Instant::now();
    let workers =
        crate::scheduling::worker::Worker::spawn_pool(python, n, worker_env, fork_prewarm)?;
    // What actually happened, not what was asked: spawn_pool falls back to
    // plain spawns when the zygote can't get its fds.
    let fork_prewarmed = workers
        .first()
        .is_some_and(crate::scheduling::worker::Worker::is_forked);
    let mut states = Vec::new();
    for (idx, worker) in workers.into_iter().enumerate() {
        let worker = start_into(worker, idx, args, &tx)?;
        states.push(WorkerState::fresh(worker));
    }
    // "Pool ready" = every initial worker has emitted its first event; stamped
    // in the event loop below. See run_pool for why spawn duration is not a fair
    // startup metric.
    let mut ready_workers: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut startup_seconds = 0.0f64;

    let cwd = std::env::current_dir()?;
    let (file_order, file_weights) =
        order_files(files, duration_cache, &cwd, &crate::cache::base_dir(), n);
    let mut file_queue: VecDeque<String> = file_order.into();

    let continue_on_collect_errors = args.iter().any(|a| a == "--continue-on-collection-errors");

    let mut run = Run::default();
    run.track_phase_durations = track_durations;
    let mut prog = Progress::default();
    // Json mode keeps stdout pure NDJSON; the footer would corrupt it.
    if mode != crate::reporting::progress::Mode::Json {
        prog.enable_footer(n, sink.palette().live());
    }
    prog.set_mode(mode);
    let mut fixtures: Vec<proto::FixtureStat> = Vec::new();
    let mut warnings: Vec<proto::WarningEntry> = Vec::new();
    let mut statuses = Vec::new();
    let mut peak_rss: Option<u64> = None;
    let mut warm_record = crate::scheduling::warm::Record::default();
    let mut cache_dir: Option<String> = None;
    let mut sources = crate::scheduling::durations::Collected::default();
    let mut requeued: VecDeque<String> = VecDeque::new();
    let mut serial: VecDeque<String> = VecDeque::new();
    let mut serial_active = false;
    let mut retry: Reruns<String> = Reruns::new(reruns, only_rerun, known_flaky);
    let mut ctl = PoolCtl::new(maxfail, quarantine);
    let mut collect_aborted = false;
    let mut done_workers = 0usize;
    let mut restarts_left = n.max(4);
    // A worker died with the restart budget spent: its work moved to
    // survivors, and anything left unrun at the end is reported as not run.
    let mut lost_worker = false;
    // Replay journaling, as in run_pool: each worker's ordered item starts,
    // keyed by nodeid, so `rstest replay` can re-pin this schedule on the
    // eager pool. A one-worker run has no parallel schedule to reproduce.
    let journaling = journal_dist.is_some() && n >= 2 && crate::replay::journaling_enabled();
    let mut recorder: Vec<Vec<String>> = if journaling {
        vec![Vec::new(); n]
    } else {
        Vec::new()
    };
    // Keep only the FIRST attempt of a rerun/redistributed id: replay runs
    // with reruns off, so a duplicate entry would run the test twice.
    let mut journaled: HashSet<String> = HashSet::new();
    let mut collect_skipped: HashSet<String> = HashSet::new();

    // SIGINT/SIGTERM: stop the workers and fall through to the wind-down, as
    // in run_pool.
    let _interrupt = crate::scheduling::interrupt::Guard::install();
    loop {
        let received = rx.recv_timeout(std::time::Duration::from_millis(500));
        if let Some(sig) = crate::scheduling::interrupt::requested() {
            orchestrator::interrupt_all(
                sink,
                &mut run,
                &mut prog,
                &mut states,
                |_, s| s.running.clone(),
                sig,
            );
            statuses.push(2); // pytest INTERRUPTED
            break;
        }
        let (idx, event) = match received {
            Ok(pair) => pair,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                prog.tick(sink);
                orchestrator::watchdog_tick(sink, &mut states);
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if ready_workers.len() < n && ready_workers.insert(idx) && ready_workers.len() == n {
            startup_seconds = spawn_start.elapsed().as_secs_f64();
        }
        match event {
            Ok(Event::Report(r)) => {
                if let Some(id) = &states[idx].running {
                    if retry.budget(id) > 0 {
                        states[idx].attempt_failed |= r.outcome == "failed";
                        states[idx].attempt.push(r);
                        continue;
                    }
                }
                ctl.fail_count.record(&r);
                prog.on_report(sink, Some(idx), &r);
                sink.emit_report(Some(idx), &r);
                run.record(Some(idx), r);
                ctl.trip_maxfail(&mut run, &mut states);
            }
            Ok(Event::CollectError { path, longrepr }) => {
                // A file collected again elsewhere (steal / redistribution)
                // reports the same error: show it once.
                if run.collect_error(path.clone(), longrepr.clone()) {
                    prog.on_collect_error(sink, &path, &longrepr);
                    sink.emit_collect_error(&path, &longrepr);
                }
                if !continue_on_collect_errors && !ctl.stopping {
                    // pytest aborts on collection errors; in lazy mode the
                    // error can surface mid-run - stop dispatching and wind
                    // down (already-final outcomes stay reported).
                    collect_aborted = true;
                    ctl.stopping = true;
                    orchestrator::stop_all(&mut states);
                }
            }
            Ok(e @ (Event::DoctorFixtures { .. }
            | Event::ScopeCosts { .. }
            | Event::FixtureLeaks { .. }
            | Event::JunitCase { .. }
            | Event::JunitSuite { .. }
            | Event::Warnings { .. }
            | Event::CollectSkip { .. }
            | Event::LfRecollected { .. })) => orchestrator::fold_bookkeeping(
                e,
                Dedup::Lazy {
                    idx,
                    skipped: &mut collect_skipped,
                },
                &mut run,
                &mut fixtures,
                &mut warnings,
            ),
            Ok(Event::LazyReady {
                cache_dir: cd,
                rootdir,
                maxfail: reported_maxfail,
            }) => {
                // pytest's own `-x`/`--maxfail` resolution (argv + ini
                // `addopts` / PYTEST_ADDOPTS) is authoritative; every worker
                // reports the same value before its first RunFiles.
                if reported_maxfail.is_some() {
                    ctl.maxfail = reported_maxfail;
                }
                if let Some(cd) = cd {
                    cache_dir.get_or_insert(cd);
                }
                if let Some(rd) = &rootdir {
                    sources.set_rootdir(rd);
                }
                states[idx].ready = true;
            }
            Ok(Event::FileCollected {
                path,
                ids,
                serial: ser,
                flaky,
                deselected,
            }) => {
                run.deselected += deselected;
                ctl.total_items += ids.len();
                prog.set_total(ctl.total_items);
                sources.record(&ids);
                let s = &mut states[idx];
                if let Some(pos) = s.uncollected_files.iter().position(|f| *f == path) {
                    s.uncollected_files.remove(pos);
                }
                // Serial items run on the designate, post-parallel; the
                // rest queue for dispatch (owner-preferred).
                let ser_set: std::collections::HashSet<&String> = ser.iter().collect();
                s.own_queue
                    .extend(ids.iter().filter(|i| !ser_set.contains(i)).cloned());
                serial.extend(ser);
                retry.marked.extend(flaky);
                // Every file collected: the run's test count is final, so the
                // auto-quarantine cap can settle the held failures.
                if file_queue.is_empty() && states.iter().all(|s| s.uncollected_files.is_empty()) {
                    ctl.fail_count.set_total(ctl.total_items);
                    ctl.trip_maxfail(&mut run, &mut states);
                }
            }
            Ok(Event::ItemStartId { id, timeout }) => {
                states[idx].running = Some(id.clone());
                states[idx].running_since = Some(std::time::Instant::now());
                states[idx].running_watchdog = orchestrator::watchdog_for(worker_timeout, timeout);
                if journaling && journaled.insert(id.clone()) {
                    recorder[idx].push(id.clone());
                }
                prog.item_started(sink, idx, id);
            }
            Ok(Event::ItemDoneId { id }) => {
                on_item_done_id(
                    idx,
                    id,
                    &mut states,
                    &mut ctl,
                    &mut retry,
                    &mut requeued,
                    &mut run,
                    &mut prog,
                    sink,
                );
            }
            Ok(Event::StoppedIds { unrun }) => {
                let s = &mut states[idx];
                s.finishing = true;
                for id in &unrun {
                    if let Some(pos) = s.outstanding.iter().position(|x| x == id) {
                        s.outstanding.remove(pos);
                    }
                }
                if !ctl.stopping {
                    // The worker's own session stopped (not a global stop):
                    // it left its run loop and takes no more ids or files.
                    // What it still had comes back at its Done.
                    s.ended = true;
                    requeued.extend(unrun);
                }
            }
            Ok(Event::SessionExit { reason, returncode }) => {
                // pytest.exit() in a test ends the whole run (see the full
                // pool): stop every worker; the calling test has no outcome.
                let s = &mut states[idx];
                s.attempt.clear();
                s.attempt_failed = false;
                s.running_since = None;
                s.running_watchdog = None;
                if let Some(id) = s.running.take() {
                    run.forget_unfinished(&id);
                }
                run.session_exit
                    .get_or_insert(crate::reporting::report::SessionExit { reason, returncode });
                if !ctl.stopping {
                    ctl.stopping = true;
                    orchestrator::stop_all(&mut states);
                }
            }
            Ok(Event::Done {
                exitstatus,
                peak_rss: peak,
                imported,
                sys_path,
                coverage,
                rewrite,
                env,
            }) => {
                statuses.push(exitstatus);
                peak_rss = peak_rss.max(peak);
                warm_record.merge(imported, sys_path, coverage);
                warm_record.rewrite.extend(rewrite);
                warm_record.env.extend(env);
                let s = &mut states[idx];
                s.dead = true;
                // A session that ended on its own never ran what reached it
                // after it left its loop. Hand it to the live workers, unless
                // the run is stopping (as the crash path does).
                let left = std::mem::take(&mut s.outstanding);
                let own: Vec<String> = s.own_queue.drain(..).collect();
                let files = std::mem::take(&mut s.uncollected_files);
                if !ctl.stopping {
                    requeued.extend(left);
                    requeued.extend(own);
                    for f in files.into_iter().rev() {
                        file_queue.push_front(f);
                    }
                }
                done_workers += 1;
                if done_workers == states.len() {
                    break;
                }
            }
            // Index-keyed events belong to the full pool mode; a lazy
            // session never emits them.
            Ok(Event::CollectionDone { .. })
            | Ok(Event::NodeInput { .. })
            | Ok(Event::ItemStart { .. })
            | Ok(Event::ItemDone { .. })
            | Ok(Event::Stopped { .. })
            // Single-session only.
            | Ok(Event::Interrupted {}) => {}
            Err(e) => {
                // Name the cause (exit status, stderr tail) before anything below
                // reaps the worker, which kills first and would hide it.
                let e = states[idx].worker.explain_failure(e);
                // The limit that killed it, when the watchdog did (names it in the
                // fabricated failure).
                let killed_by = if states[idx].timeout_killed {
                    states[idx].running_watchdog
                } else {
                    None
                };
                let crashed = states[idx].running.take();
                states[idx].attempt.clear();
                states[idx].attempt_failed = false;
                let mut orphaned: Vec<String> = std::mem::take(&mut states[idx].outstanding);
                // Collected-but-undispatched ids die with their owner's
                // item cache; survivors re-collect the files.
                orphaned.extend(states[idx].own_queue.drain(..));
                // The crashed id is in `outstanding` too; drop it by identity
                // so it never runs twice (retried below or reported failed).
                orphaned.retain(|id| Some(id) != crashed.as_ref());
                // Assigned-but-uncollected files go back to the queue.
                for f in states[idx].uncollected_files.drain(..) {
                    file_queue.push_front(f);
                }
                let restartable = states[idx].ready && restarts_left > 0;
                let report_crash = retry_crashed(crashed, restartable, &mut retry, &mut requeued);
                if let Some(id) = report_crash {
                    let fab = orchestrator::fabricate_crash_report(id, killed_by, idx, &e);
                    orchestrator::record_fabricated(sink, &mut run, &mut prog, Some(idx), fab);
                }
                // With or without a replacement, the rest is redistributed by
                // the dispatch below; if no worker is left to run it, it is
                // swept up as not run after the loop.
                requeued.extend(orphaned);
                if restartable {
                    restarts_left -= 1;
                    sink.warn(&format!(
                        "rstest: worker gw{idx} crashed; respawning \
                         ({restarts_left} restarts left)"
                    ));
                    // Reap the old worker in place BEFORE spawning its
                    // replacement. This arm also fires on a decode error (the
                    // child may still be alive, running tests against a closed
                    // pipe) and on watchdog kills; `Child`'s drop neither kills
                    // nor waits, so an alive child would orphan and an exited one
                    // become a `<defunct>` zombie. Reaping first (not after the
                    // spawn) means a `spawn_into` error `?`-returning can't leave
                    // the old child un-reaped.
                    states[idx].worker.reap();
                    let worker = spawn_into(python, idx, states.len(), args, &tx, worker_env)?;
                    states[idx] = WorkerState::fresh(worker);
                } else {
                    lost_worker = true;
                    run.collect_error(
                        format!("<worker gw{idx}>"),
                        format!("worker terminated unexpectedly: {e:#}"),
                    );
                    statuses.push(3);
                    states[idx].dead = true;
                    // Reap now (a decode error can leave the child alive) rather
                    // than letting it linger as a zombie until the end-of-run
                    // wait(). The slot stays in the vec, so reap in place.
                    states[idx].worker.reap();
                    done_workers += 1;
                    if idx == ctl.designate {
                        // Promote the lowest worker still listening; with none
                        // left the serial ids are swept up after the loop.
                        if let Some(next) = states.iter().position(|s| !s.dead && !s.ended) {
                            ctl.designate = next;
                        }
                    }
                    if done_workers == states.len() {
                        break;
                    }
                }
            }
        }

        let chunk = crate::scheduling::pool::chunk_size(ctl.total_items.max(1), states.len());

        // File assignment: a worker takes a batch of files (see
        // `file_batch`) when `wants_files`.
        if !ctl.stopping {
            assign_files(&mut states, &mut file_queue, &file_weights, chunk);
        }

        // Id dispatch: top up any worker below the refill threshold. Sources
        // in order: requeued (reruns/redistribution), the worker's own
        // queue, then STEAL from the longest queue (only when no files left).
        if !ctl.stopping {
            dispatch_ids(&mut states, &mut requeued, &file_queue, steal, chunk);
        }

        release_or_end(
            &mut states,
            &ctl,
            &mut serial,
            &mut serial_active,
            &requeued,
            &file_queue,
        );
    }

    // Work left after the last worker ended (every worker able to run it died
    // for good, or a session ended on its own with ids still handed to it):
    // report what never ran instead of dropping it from the artifacts. Ids
    // are known for collected files; a file nobody collected can only be
    // named as a collection error. Not after a -x/--maxfail or
    // collection-error stop, where unrun is expected. On a clean finish
    // every queue is empty and this records nothing.
    if !ctl.stopping {
        let reason = if lost_worker {
            orchestrator::LOST_NO_WORKER
        } else {
            orchestrator::LOST_SESSION_ENDED
        };
        let unrun: Vec<String> = requeued
            .drain(..)
            .chain(states.iter_mut().flat_map(|s| s.own_queue.drain(..)))
            .chain(serial.drain(..))
            .collect();
        for id in unrun {
            let fab = orchestrator::fabricate_lost_report(id, reason);
            orchestrator::record_fabricated(sink, &mut run, &mut prog, None, fab);
        }
        let uncollected: Vec<String> = file_queue
            .iter()
            .cloned()
            .chain(
                states
                    .iter()
                    .flat_map(|s| s.uncollected_files.iter().cloned()),
            )
            .collect();
        for f in uncollected {
            run.collect_error(f, format!("not collected: {reason}"));
        }
    }
    // Files left uncollected (-x/--maxfail or a collection error stopped the
    // run) make `total_items` a partial count; record 0 then, which skips
    // replay's size-drift check instead of flagging a suite change.
    let fully_collected =
        file_queue.is_empty() && states.iter().all(|s| s.uncollected_files.is_empty());
    // Reaped by the caller after the post-run saves (see run_pool).
    let mut exiting: Vec<_> = states.into_iter().map(|s| s.worker).collect();
    for w in &mut exiting {
        let _ = w.send(&proto::Command::Shutdown);
    }
    let retried = reruns > 0 || !run.flaky.is_empty();
    let exitstatus = orchestrator::finalize_exit(
        &statuses,
        run.all_passed(),
        retried,
        collect_aborted,
        run.stopped_after.is_some(),
    );
    // -x can stop the run before every file is collected, leaving the count
    // unsettled: the collected files still bound it better than what reported.
    run.cap_total = Some(ctl.fail_count.total.unwrap_or(ctl.total_items));
    // pytest.exit(): its returncode is the run's exit code, as under pytest.
    let exitstatus = run
        .session_exit
        .as_ref()
        .map_or(exitstatus, |x| x.returncode);
    // Persist the schedule for `rstest replay`. Best-effort, like run_pool. No
    // collection hash: lazy never agrees on one ordered nodeid list, so replay's
    // drift check falls back to the collected count (when it is complete).
    if let (true, Some(dist)) = (journaling, journal_dist) {
        crate::replay::write(&crate::replay::Journal::record(
            worker_env.run_uid.clone(),
            n,
            dist.to_string(),
            None,
            args.to_vec(),
            None,
            if fully_collected {
                ctl.total_items as u64
            } else {
                0
            },
            recorder,
        ));
    }
    Ok(PoolOutcome {
        fixture_groups: Vec::new(),
        run,
        prog,
        fixtures,
        warnings,
        cache_dir,
        exitstatus,
        // Lazy collection shards at file granularity with no global nodeid
        // hash, so shard-verify does not cover lazy runs (no shard meta stamped).
        collection_hash: None,
        collection_size: 0,
        startup_seconds,
        fork_prewarmed,
        warm_record,
        warm: None,
        sources,
        peak_rss,
        // Not recorded here: per-file collection timed under every worker's
        // contention sums to far more than one eager worker's collection (2x
        // on pandas at -n 14), and lazy runs refreshing it would ratchet the
        // auto-lazy budget up. Only eager runs measure what lazy saves.
        collect_seconds: None,
        lazy: true,
        exiting,
    })
}

/// File assignment: a worker takes a batch of files (see
/// `file_batch`) when `wants_files`.
fn assign_files(
    states: &mut [WorkerState],
    file_queue: &mut VecDeque<String>,
    file_weights: &HashMap<String, f64>,
    chunk: usize,
) {
    let workers = states.len();
    for s in states.iter_mut().filter(|s| s.ready && !s.dead && !s.ended) {
        if wants_files(
            s.uncollected_files.len(),
            s.own_queue.len(),
            s.outstanding.len(),
            chunk,
        ) {
            let take = file_batch(file_queue, file_weights, workers);
            let paths: Vec<String> = file_queue.drain(..take).collect();
            if !paths.is_empty() {
                s.uncollected_files.extend(paths.iter().cloned());
                // Best-effort, as in the eager pool: a worker that died
                // (EPIPE) is handled by its reader's error, which puts
                // the files back on the queue.
                let _ = s.worker.send(&proto::Command::RunFiles { paths });
                s.finishing = false;
            }
        }
    }
}

/// Id dispatch: top up any worker below the refill threshold. Sources
/// in order: requeued (reruns/redistribution), the worker's own
/// queue, then STEAL from the longest queue (only when no files left).
fn dispatch_ids(
    states: &mut [WorkerState],
    requeued: &mut VecDeque<String>,
    file_queue: &VecDeque<String>,
    steal: bool,
    chunk: usize,
) {
    for i in 0..states.len() {
        if !states[i].ready || states[i].dead || states[i].ended {
            continue;
        }
        // Top up safely above the hold threshold: the worker holds
        // its last item (nextitem lookahead) and no event triggers a
        // refill, so an under-threshold dispatch deadlocks it alone.
        loop {
            if states[i].outstanding.len() > (chunk / 2).max(1) {
                break;
            }
            let mut ids: Vec<String> = Vec::new();
            while ids.len() < chunk {
                if let Some(id) = requeued.pop_front() {
                    ids.push(id);
                    continue;
                }
                if let Some(id) = states[i].own_queue.pop_front() {
                    ids.push(id);
                    continue;
                }
                if !steal || !file_queue.is_empty() {
                    break; // affinity mode, or more files coming
                }
                // Steal HALF the longest queue's remainder: one
                // re-collection buys sustained balance, whereas tiny
                // steals would re-collect the file per chunk.
                let victim = (0..states.len())
                    .filter(|&j| j != i)
                    .max_by_key(|&j| states[j].own_queue.len());
                match victim {
                    Some(j) if !states[j].own_queue.is_empty() => {
                        let take = (states[j].own_queue.len() / 2)
                            .max(chunk.min(states[j].own_queue.len()));
                        let at = states[j].own_queue.len() - take;
                        ids.extend(states[j].own_queue.split_off(at));
                        break;
                    }
                    _ => break,
                }
            }
            if ids.is_empty() {
                break;
            }
            let s = &mut states[i];
            s.outstanding.extend(ids.iter().cloned());
            // Best-effort: a dead worker's `outstanding` is requeued
            // when its reader reports the death.
            let _ = s.worker.send(&proto::Command::RunIds { ids });
            s.finishing = false;
        }
    }
}

/// A worker finished `id`: retry it if its attempt failed with budget left
/// (and the `--only-rerun` / known-flaky gates allow), else record the final
/// attempt.
#[allow(clippy::too_many_arguments)]
fn on_item_done_id(
    idx: usize,
    id: String,
    states: &mut [WorkerState],
    ctl: &mut PoolCtl,
    retry: &mut Reruns<String>,
    requeued: &mut VecDeque<String>,
    run: &mut Run,
    prog: &mut Progress,
    sink: &mut Sink,
) {
    prog.item_finished(sink, idx);
    let s = &mut states[idx];
    s.running = None;
    s.running_since = None;
    s.running_watchdog = None;
    if let Some(pos) = s.outstanding.iter().position(|x| *x == id) {
        s.outstanding.remove(pos);
    }
    let item_budget = retry.budget(&id);
    if item_budget > 0 {
        let rerun_allowed = orchestrator::rerun_allowed(retry.only_rerun, &s.attempt);
        // --reruns-only-known-flaky: id IS the nodeid in lazy mode.
        let known_flaky_ok = retry.known_flaky_ok(&id, Some(&id));
        let used = retry.used.entry(id.clone()).or_insert(0);
        if s.attempt_failed && *used < item_budget && rerun_allowed && known_flaky_ok {
            *used += 1;
            s.attempt.clear();
            s.attempt_failed = false;
            requeued.push_back(id);
        } else {
            let attempts = *used;
            let failed_now = s.attempt_failed;
            let attempt = std::mem::take(&mut s.attempt);
            s.attempt_failed = false;
            orchestrator::finalize_attempt(
                sink,
                run,
                prog,
                &mut ctl.fail_count,
                idx,
                orchestrator::FinishedAttempt {
                    attempts_used: attempts,
                    reports: attempt,
                    failed: failed_now,
                    flaky_key: Some(id),
                },
            );
            ctl.trip_maxfail(run, states);
        }
    }
}

/// The crashed id retries only when BOTH the rerun and the restart budgets
/// allow (segfault-loop guard). Returns the id to report as crashed, if any.
fn retry_crashed(
    crashed: Option<String>,
    restartable: bool,
    retry: &mut Reruns<String>,
    requeued: &mut VecDeque<String>,
) -> Option<String> {
    let mut report_crash = crashed;
    if let (true, Some(id)) = (restartable, &report_crash) {
        let known_ok = retry.known_flaky_ok(id, Some(id));
        let budget = retry.budget(id);
        let used = retry.used.entry(id.clone()).or_insert(0);
        if *used < budget && known_ok {
            *used += 1;
            requeued.push_back(id.clone());
            report_crash = None;
        }
    }
    report_crash
}

/// After each event: start the serial phase once only the designate is left,
/// release workers whose queue is exhausted for now (NoMoreItems), and end
/// every session once the parallel portion is resolved.
fn release_or_end(
    states: &mut [WorkerState],
    ctl: &PoolCtl,
    serial: &mut VecDeque<String>,
    serial_active: &mut bool,
    requeued: &VecDeque<String>,
    file_queue: &VecDeque<String>,
) {
    let ids_left = !requeued.is_empty() || states.iter().any(|s| !s.own_queue.is_empty());

    // Serial phase: once every non-designate worker is Done, the held
    // designate runs the serial ids exclusively.
    if !ctl.stopping
        && !*serial_active
        && !serial.is_empty()
        && file_queue.is_empty()
        && !ids_left
        && states
            .iter()
            .enumerate()
            .all(|(i, s)| i == ctl.designate || s.dead)
        && !states[ctl.designate].dead
    {
        *serial_active = true;
        let ids: Vec<String> = serial.drain(..).collect();
        let s = &mut states[ctl.designate];
        s.outstanding.extend(ids.iter().cloned());
        // Best-effort, as above.
        let _ = s.worker.send(&proto::Command::RunIds { ids });
        s.finishing = false;
    }

    // Release workers whose queue is exhausted FOR NOW. A collection in
    // flight ANYWHERE blocks release: its ids may be stolen by a drained
    // worker whose fixtures were torn down (premature NoMoreItems = re-setup).
    let collecting = states
        .iter()
        .any(|s| !s.dead && !s.uncollected_files.is_empty());
    let queue_empty = file_queue.is_empty() && !ids_left && !collecting;
    if queue_empty || ctl.stopping {
        for s in states
            .iter_mut()
            .filter(|s| s.ready && !s.dead && !s.finishing && s.uncollected_files.is_empty())
        {
            s.finishing = true;
            let _ = s.worker.send(&proto::Command::NoMoreItems);
        }
    }
    let in_flight = states.iter().any(|s| !s.dead && !s.outstanding.is_empty());
    let parallel_resolved = if ctl.stopping {
        !in_flight
    } else {
        queue_empty
            && !in_flight
            && states
                .iter()
                .all(|s| s.dead || s.uncollected_files.is_empty())
    };
    if parallel_resolved {
        let serial_pending = !ctl.stopping && (!serial.is_empty() || *serial_active && in_flight);
        for (i, s) in states.iter_mut().enumerate() {
            // Gate on `ready` (sent LazyReady), NOT "was assigned a file":
            // a collection error can trip `stopping` before a ready worker
            // gets one, and skipping it hangs the run (done_workers < n).
            if s.dead || s.ended || !s.ready {
                continue;
            }
            if serial_pending && i == ctl.designate {
                continue;
            }
            let _ = s.worker.send(&proto::Command::EndSession);
            s.ended = true;
        }
    }
}

/// Respawn path: one fresh, independently spawned lazy worker. The initial pool
/// uses [`Worker::spawn_pool`] + [`start_into`] so it can fork-prewarm.
fn spawn_into(
    python: &Path,
    idx: usize,
    n: usize,
    args: &[String],
    tx: &mpsc::Sender<(usize, Result<Event>)>,
    env: &crate::scheduling::worker::WorkerEnv,
) -> Result<Worker> {
    let worker = Worker::spawn(python, Some((idx, n)), env)?;
    start_into(worker, idx, args, tx)
}

/// Send the lazy-session command to an already-spawned worker and start its
/// reader thread. Shared by [`spawn_into`] and the fork-prewarmed initial pool.
fn start_into(
    mut worker: Worker,
    idx: usize,
    args: &[String],
    tx: &mpsc::Sender<(usize, Result<Event>)>,
) -> Result<Worker> {
    worker.send(&proto::Command::RunLazySession {
        args: args.to_vec(),
    })?;
    let tx = tx.clone();
    let mut reader = worker.take_reader()?;
    std::thread::spawn(move || loop {
        let event = reader.recv();
        let done = matches!(event, Ok(Event::Done { .. }) | Err(_));
        if tx.send((idx, event)).is_err() || done {
            break;
        }
    });
    Ok(worker)
}

#[cfg(test)]
mod tests {
    use super::{file_batch, order_files, wants_files};
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};

    fn queue(n: usize) -> VecDeque<String> {
        (0..n).map(|i| format!("t{i}.py")).collect()
    }

    #[test]
    fn file_batch_without_timings_takes_a_quarter_share_and_at_least_one() {
        let none = HashMap::new();
        assert_eq!(file_batch(&queue(1000), &none, 8), 32);
        assert_eq!(file_batch(&queue(1000), &none, 1), 250);
        assert_eq!(file_batch(&queue(3), &none, 8), 1);
        assert_eq!(file_batch(&queue(0), &none, 8), 0);
    }

    #[test]
    fn file_batch_deals_heavy_files_one_per_batch() {
        // Heaviest first, as `order_files` leaves it: 8 files hold 90% of the
        // seconds. Counting files, the first worker to ask took all 8.
        let q = queue(1000);
        let weights: HashMap<String, f64> = q
            .iter()
            .enumerate()
            .map(|(i, f)| (f.clone(), if i < 8 { 100.0 } else { 0.1 }))
            .collect();
        assert_eq!(file_batch(&q, &weights, 8), 1);
        // The light tail goes back to the count cap.
        let tail: VecDeque<String> = q.into_iter().skip(8).collect();
        assert_eq!(file_batch(&tail, &weights, 8), 31);
    }

    #[test]
    fn wants_files_only_when_idle_enough() {
        assert!(wants_files(0, 0, 0, 64));
        assert!(wants_files(0, 64, 64, 64)); // at the limit
        assert!(!wants_files(1, 0, 0, 64)); // still collecting
        assert!(!wants_files(0, 0, 65, 64)); // enough in flight
                                             // Plenty queued but little in flight: took files before, piling the
                                             // heaviest files on one worker.
        assert!(!wants_files(0, 78_000, 10, 64));
    }

    #[test]
    fn order_files_sorts_known_by_total_then_unknown_in_path_order() {
        let cwd = Path::new("/proj");
        let files = vec![
            PathBuf::from("/proj/test_a.py"),
            PathBuf::from("/proj/test_b.py"),
            PathBuf::from("/proj/test_c.py"),
        ];
        // Nodeid keys group by the file prefix (before `::`); test_a sums to 3.0.
        let cache = HashMap::from([
            ("test_a.py::t1".to_string(), 1.0),
            ("test_a.py::t2".to_string(), 2.0),
            ("test_b.py::t".to_string(), 5.0),
            // test_c.py absent => unknown, ordered last in input order.
        ]);
        let ordered = order_files(files, &cache, cwd, cwd, 1).0;
        assert_eq!(ordered, vec!["test_b.py", "test_a.py", "test_c.py"]);
    }

    #[test]
    fn order_files_matches_backslash_paths_to_slash_cache_keys() {
        // Windows paths carry `\`, cached nodeids `/`: they must still match.
        let cwd = Path::new("/proj");
        let files = vec![
            PathBuf::from("/proj/tests\\test_a.py"),
            PathBuf::from("/proj/tests\\test_b.py"),
        ];
        let cache = HashMap::from([
            ("tests/test_a.py::t".to_string(), 1.0),
            ("tests/test_b.py::t".to_string(), 5.0),
        ]);
        let ordered = order_files(files, &cache, cwd, cwd, 1).0;
        assert_eq!(ordered, vec!["tests/test_b.py", "tests/test_a.py"]);
    }

    #[test]
    fn order_files_matches_cache_keyed_on_a_rootdir_below_cwd() {
        // pandas from the corpus: cwd is the venv's parent, pytest's rootdir
        // the installed package, so cached nodeids read `tests/...` while the
        // walked files read `pkg/tests/...`. Workers still get cwd paths.
        let cwd = Path::new("/proj");
        let files = vec![
            PathBuf::from("pkg/tests/test_a.py"),
            PathBuf::from("pkg/tests/test_b.py"),
        ];
        let cache = HashMap::from([
            ("tests/test_a.py::t".to_string(), 1.0),
            ("tests/test_b.py::t".to_string(), 5.0),
        ]);
        let ordered = order_files(files, &cache, cwd, Path::new("/proj/pkg"), 1).0;
        assert_eq!(ordered, vec!["pkg/tests/test_b.py", "pkg/tests/test_a.py"]);
    }

    #[test]
    fn order_files_puts_only_long_poles_first_and_keeps_path_order() {
        // agent-platform-backend: one flat tests/ dir whose neighbouring files
        // share imports and fixtures. Sorting every file by weight scattered
        // them over the workers (65.1s against 52.7s); only a long pole
        // (total / (POLE_SHARE * workers) or more) jumps the queue.
        let cwd = Path::new("/proj");
        let names = [
            "test_a.py",
            "test_b.py",
            "test_big.py",
            "test_c.py",
            "test_d.py",
        ];
        let files: Vec<PathBuf> = names.iter().map(|n| cwd.join(n)).collect();
        let cache = HashMap::from([
            ("test_a.py::t".to_string(), 0.5),
            ("test_b.py::t".to_string(), 0.1),
            ("test_big.py::t".to_string(), 20.0),
            ("test_c.py::t".to_string(), 0.4),
            ("test_d.py::t".to_string(), 0.2),
        ]);
        // 21.2s over 2 workers: the pole cut is 21.2 / (16 * 2) = 0.66s, so
        // only test_big leads; the light files keep path order, not weight
        // order.
        let ordered = order_files(files, &cache, cwd, cwd, 2).0;
        assert_eq!(
            ordered,
            vec![
                "test_big.py",
                "test_a.py",
                "test_b.py",
                "test_c.py",
                "test_d.py"
            ]
        );
    }

    #[test]
    fn order_files_all_unknown_keeps_input_order() {
        let cwd = Path::new("/proj");
        let files = vec![PathBuf::from("/proj/z.py"), PathBuf::from("/proj/a.py")];
        let ordered = order_files(files, &HashMap::new(), cwd, cwd, 1).0;
        assert_eq!(ordered, vec!["z.py", "a.py"]); // no cache => unchanged
    }

    #[test]
    fn order_files_keeps_full_path_when_outside_cwd() {
        // A file that isn't under cwd can't be stripped; it stays absolute and,
        // unmatched by the cwd-relative cache keys, lands in the unknown tail.
        let cwd = Path::new("/proj");
        let files = vec![PathBuf::from("/other/test_x.py")];
        let ordered = order_files(files, &HashMap::new(), cwd, cwd, 1).0;
        assert_eq!(ordered, vec!["/other/test_x.py"]);
    }

    #[cfg(unix)]
    #[test]
    fn order_files_strips_a_cwd_spelled_differently_from_the_walk() {
        // Windows CI: the walk yields canonical `\\?\C:\...` paths while the
        // cwd is GitHub's 8.3 `RUNNER~1` spelling. Unstripped, workers got
        // absolute paths outside pytest's rootdir, every file's nodeids came
        // back as `::test_x[i]`, and 4 files x 500 tests ran as 500. A
        // symlinked cwd is the same split on unix.
        let base = std::env::temp_dir().join(format!("rstest-lazy-alias-{}", std::process::id()));
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let alias = base.join("alias");
        let _ = std::fs::remove_file(&alias);
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let real = real.canonicalize().unwrap();
        for f in ["test_a.py", "test_b.py"] {
            std::fs::write(real.join(f), "").unwrap();
        }
        let files = vec![real.join("test_a.py"), real.join("test_b.py")];
        let cache = HashMap::from([("test_b.py::t".to_string(), 5.0)]);
        let (ordered, weights) = order_files(files, &cache, &alias, &alias, 1);
        std::fs::remove_dir_all(&base).unwrap();
        assert_eq!(ordered, vec!["test_b.py", "test_a.py"]);
        assert_eq!(weights.get("test_b.py"), Some(&5.0));
    }
}
