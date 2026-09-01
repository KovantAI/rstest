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

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::Result;

use crate::reporting::progress::Progress;
use crate::reporting::report::Run;
use crate::scheduling::pool::PoolOutcome;
use crate::scheduling::proto::{self, Event};
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
    timeout_killed: bool,
    attempt: Vec<proto::Report>,
    attempt_failed: bool,
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
            timeout_killed: false,
            attempt: Vec::new(),
            attempt_failed: false,
        }
    }
}

/// Order files by cached duration totals, biggest first (long-pole files
/// must start early); files with no cache data follow in path order.
fn order_files(files: Vec<PathBuf>, cache: &HashMap<String, f64>, cwd: &Path) -> Vec<String> {
    // The duration cache keys on nodeids relative to the invocation dir;
    // group totals by the file prefix.
    let mut totals: HashMap<String, f64> = HashMap::new();
    for (id, secs) in cache {
        let file = id.split("::").next().unwrap_or(id);
        *totals.entry(file.to_string()).or_insert(0.0) += secs;
    }
    let mut known: Vec<(String, f64)> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(cwd)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| f.to_string_lossy().into_owned());
        match totals.get(&rel) {
            Some(&t) => known.push((rel, t)),
            None => unknown.push(rel),
        }
    }
    known.sort_by(|a, b| b.1.total_cmp(&a.1));
    known.into_iter().map(|(f, _)| f).chain(unknown).collect()
}

#[allow(clippy::too_many_arguments)] // orchestration entry point, same shape as run_pool
pub fn run_lazy_pool(
    python: &Path,
    n: usize,
    args: &[String],
    files: Vec<PathBuf>,
    mode: crate::reporting::progress::Mode,
    palette: crate::reporting::color::Palette,
    // --dist loadfile => steal=false: strict file affinity, the remedy
    // for order-dependent suites (same contract as the full pool).
    steal: bool,
    maxfail: Option<u64>,
    reruns: u32,
    only_rerun: &[regex::Regex],
    worker_timeout: Option<std::time::Duration>,
    // Some(set) => --reruns-only-known-flaky: gate reruns on prior flaky
    // history (or an explicit @mark.flaky budget). See run_pool.
    known_flaky: Option<&std::collections::HashSet<String>>,
) -> Result<PoolOutcome> {
    let (tx, rx) = mpsc::channel::<(usize, Result<Event>)>();
    let mut states = Vec::new();
    for idx in 0..n {
        let worker = spawn_into(python, idx, n, args, &tx)?;
        states.push(WorkerState::fresh(worker));
    }

    let duration_cache = crate::scheduling::durations::load();
    let cwd = std::env::current_dir()?;
    let mut file_queue: VecDeque<String> = order_files(files, &duration_cache, &cwd).into();

    let continue_on_collect_errors = args.iter().any(|a| a == "--continue-on-collection-errors");

    let mut run = Run::default();
    let mut prog = Progress::default();
    prog.set_palette(palette);
    // Json mode keeps stdout pure NDJSON; the footer would corrupt it.
    if mode != crate::reporting::progress::Mode::Json {
        prog.enable_footer(n);
    }
    prog.set_mode(mode);
    let mut fixtures: Vec<proto::FixtureStat> = Vec::new();
    let mut warnings: Vec<proto::WarningEntry> = Vec::new();
    let mut statuses = Vec::new();
    let mut cache_dir: Option<String> = None;
    let mut total_items = 0usize;
    let mut requeued: VecDeque<String> = VecDeque::new();
    let mut serial: VecDeque<String> = VecDeque::new();
    let mut serial_active = false;
    let mut flaky_budget: HashMap<String, u32> = HashMap::new();
    let mut rerun_used: HashMap<String, u32> = HashMap::new();
    let budget_of = |flaky: &HashMap<String, u32>, id: &str| -> u32 {
        flaky.get(id).copied().unwrap_or(reruns)
    };
    let mut fail_count = 0u64;
    let mut stopping = false;
    let mut collect_aborted = false;
    let mut done_workers = 0usize;
    let mut restarts_left = n.max(4);
    let mut designate = 0usize;

    loop {
        let (idx, event) = match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(pair) => pair,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                prog.tick();
                if let Some(limit) = worker_timeout {
                    for (widx, s) in states.iter_mut().enumerate() {
                        if s.dead || s.timeout_killed {
                            continue;
                        }
                        if let Some(since) = s.running_since {
                            if since.elapsed() > limit {
                                eprintln!(
                                    "rstest: worker gw{widx} exceeded --worker-timeout ({}s) on one test; killing it",
                                    limit.as_secs()
                                );
                                s.timeout_killed = true;
                                s.worker.kill();
                            }
                        }
                    }
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match event {
            Ok(Event::Report(r)) => {
                if let Some(id) = &states[idx].running {
                    if budget_of(&flaky_budget, id) > 0 {
                        states[idx].attempt_failed |= r.outcome == "failed";
                        states[idx].attempt.push(r);
                        continue;
                    }
                }
                if r.outcome == "failed" {
                    fail_count += 1;
                }
                prog.on_report(Some(idx), &r);
                run.record(Some(idx), r);
                if let Some(limit) = maxfail {
                    if !stopping && fail_count >= limit {
                        stopping = true;
                        for s in states.iter_mut().filter(|s| !s.dead && !s.finishing) {
                            s.finishing = true;
                            let _ = s.worker.send(&proto::Command::NoMoreItems);
                        }
                    }
                }
            }
            Ok(Event::CollectError { path, longrepr }) => {
                run.collect_error(path, longrepr);
                if !continue_on_collect_errors && !stopping {
                    // pytest aborts on collection errors; in lazy mode the
                    // error can surface mid-run - stop dispatching and wind
                    // down (already-final outcomes stay reported).
                    collect_aborted = true;
                    stopping = true;
                    for s in states.iter_mut().filter(|s| !s.dead && !s.finishing) {
                        s.finishing = true;
                        let _ = s.worker.send(&proto::Command::NoMoreItems);
                    }
                }
            }
            Ok(Event::DoctorFixtures { fixtures: fx }) => fixtures.extend(fx),
            Ok(Event::Warnings { entries }) => {
                // Files are disjoint across lazy workers, so collect and
                // runtest warnings are each seen once; only config-phase
                // warnings repeat per session - count those from gw0.
                warnings.extend(
                    entries
                        .into_iter()
                        .filter(|e| idx == 0 || e.when != "config"),
                );
            }
            Ok(Event::CollectSkip { .. }) => {
                // Each skipped collector is seen by exactly one worker.
                run.collect_skips += 1;
            }
            Ok(Event::LazyReady { cache_dir: cd }) => {
                if let Some(cd) = cd {
                    cache_dir.get_or_insert(cd);
                }
                states[idx].ready = true;
            }
            Ok(Event::FileCollected {
                path,
                ids,
                serial: ser,
                flaky,
            }) => {
                total_items += ids.len();
                prog.set_total(total_items);
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
                flaky_budget.extend(flaky);
            }
            Ok(Event::ItemStartId { id }) => {
                states[idx].running = Some(id.clone());
                states[idx].running_since = Some(std::time::Instant::now());
                prog.item_started(idx, id);
            }
            Ok(Event::ItemDoneId { id }) => {
                prog.item_finished(idx);
                let s = &mut states[idx];
                s.running = None;
                s.running_since = None;
                if let Some(pos) = s.outstanding.iter().position(|x| *x == id) {
                    s.outstanding.remove(pos);
                }
                let item_budget = budget_of(&flaky_budget, &id);
                if item_budget > 0 {
                    let rerun_allowed = only_rerun.is_empty()
                        || s.attempt.iter().any(|r| {
                            r.outcome == "failed"
                                && r.longrepr
                                    .as_deref()
                                    .is_some_and(|t| only_rerun.iter().any(|re| re.is_match(t)))
                        });
                    // --reruns-only-known-flaky: id IS the nodeid in lazy mode.
                    let known_flaky_ok = known_flaky
                        .is_none_or(|set| flaky_budget.contains_key(&id) || set.contains(&id));
                    let used = rerun_used.entry(id.clone()).or_insert(0);
                    if s.attempt_failed && *used < item_budget && rerun_allowed && known_flaky_ok {
                        *used += 1;
                        s.attempt.clear();
                        s.attempt_failed = false;
                        requeued.push_back(id);
                    } else {
                        let attempts = *used;
                        let failed_now = s.attempt_failed;
                        for r in s.attempt.drain(..) {
                            if r.outcome == "failed" {
                                fail_count += 1;
                            }
                            prog.on_report(Some(idx), &r);
                            run.record(Some(idx), r);
                        }
                        s.attempt_failed = false;
                        if !failed_now && attempts > 0 {
                            run.mark_flaky(id, attempts);
                        }
                        if maxfail.is_some_and(|limit| fail_count >= limit) && !stopping {
                            stopping = true;
                            for st in states.iter_mut().filter(|st| !st.dead && !st.finishing) {
                                st.finishing = true;
                                let _ = st.worker.send(&proto::Command::NoMoreItems);
                            }
                        }
                    }
                }
            }
            Ok(Event::StoppedIds { unrun }) => {
                let s = &mut states[idx];
                s.finishing = true;
                for id in &unrun {
                    if let Some(pos) = s.outstanding.iter().position(|x| x == id) {
                        s.outstanding.remove(pos);
                    }
                }
                if !stopping {
                    requeued.extend(unrun);
                }
            }
            Ok(Event::Done { exitstatus }) => {
                statuses.push(exitstatus);
                states[idx].dead = true;
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
            | Ok(Event::Stopped { .. }) => {}
            Err(e) => {
                let was_timeout = states[idx].timeout_killed;
                let crashed = states[idx].running.take();
                states[idx].attempt.clear();
                states[idx].attempt_failed = false;
                let mut orphaned: Vec<String> = std::mem::take(&mut states[idx].outstanding);
                // Collected-but-undispatched ids die with their owner's
                // item cache; survivors re-collect the files.
                orphaned.extend(states[idx].own_queue.drain(..));
                // Assigned-but-uncollected files go back to the queue.
                for f in states[idx].uncollected_files.drain(..) {
                    file_queue.push_front(f);
                }
                let restartable = states[idx].ready && restarts_left > 0;
                if restartable {
                    restarts_left -= 1;
                    let crashed_orig = crashed.clone();
                    let mut crashed = crashed;
                    if let Some(id) = &crashed {
                        let known_ok = known_flaky
                            .is_none_or(|set| flaky_budget.contains_key(id) || set.contains(id));
                        let used = rerun_used.entry(id.clone()).or_insert(0);
                        if *used < budget_of(&flaky_budget, id) && known_ok {
                            *used += 1;
                            requeued.push_back(id.clone());
                            crashed = None;
                        }
                    }
                    if let Some(id) = crashed {
                        let fab = proto::Report {
                            nodeid: id,
                            when: "call".into(),
                            outcome: "failed".into(),
                            duration: 0.0,
                            longrepr: Some(if was_timeout {
                                format!(
                                    "test exceeded --worker-timeout ({}s); its worker was killed (reported failed)",
                                    worker_timeout.map(|d| d.as_secs()).unwrap_or(0)
                                )
                            } else {
                                format!(
                                    "worker gw{idx} crashed while running this test \
                                     (reported failed, not retried): {e:#}"
                                )
                            }),
                            wasxfail: false,
                            skip_reason: None,
                            cpu: None,
                            sections: Vec::new(),
                            lineno: None,
                        };
                        prog.on_report(Some(idx), &fab);
                        run.record(Some(idx), fab);
                    }
                    requeued.extend(
                        orphaned
                            .into_iter()
                            .filter(|id| Some(id) != crashed_orig.as_ref()),
                    );
                    eprintln!(
                        "rstest: worker gw{idx} crashed; respawning \
                         ({restarts_left} restarts left)"
                    );
                    let worker = spawn_into(python, idx, states.len(), args, &tx)?;
                    states[idx] = WorkerState::fresh(worker);
                } else {
                    run.collect_error(
                        format!("<worker gw{idx}>"),
                        format!("worker terminated unexpectedly: {e:#}"),
                    );
                    statuses.push(3);
                    states[idx].dead = true;
                    done_workers += 1;
                    if idx == designate {
                        if let Some(next) = states.iter().position(|s| !s.dead && !s.finishing) {
                            designate = next;
                        } else if !serial.is_empty() {
                            run.collect_error(
                                "<serial phase>".into(),
                                format!(
                                    "{} @serial tests lost: no worker left \
                                     to host the serial phase",
                                    serial.len()
                                ),
                            );
                        }
                    }
                    if done_workers == states.len() {
                        break;
                    }
                }
            }
        }

        let chunk = crate::scheduling::pool::chunk_size(total_items.max(1), states.len());

        // File assignment: a worker collects one file at a time, picked
        // up when it has nothing left to collect and little left to run
        // (a busy worker must not hoard files an idle one could collect).
        if !stopping {
            for s in states.iter_mut().filter(|s| s.ready && !s.dead && !s.ended) {
                if s.uncollected_files.is_empty() && s.outstanding.len() <= chunk {
                    if let Some(f) = file_queue.pop_front() {
                        s.uncollected_files.push(f.clone());
                        s.worker
                            .send(&proto::Command::RunFiles { paths: vec![f] })?;
                        s.finishing = false;
                    }
                }
            }
        }

        // Id dispatch: top up any worker below the refill threshold. Sources
        // in order: requeued (reruns/redistribution), the worker's own
        // queue, then STEAL from the longest queue (only when no files left).
        if !stopping {
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
                    s.worker.send(&proto::Command::RunIds { ids })?;
                    s.finishing = false;
                }
            }
        }

        let ids_left = !requeued.is_empty() || states.iter().any(|s| !s.own_queue.is_empty());

        // Serial phase: once every non-designate worker is Done, the held
        // designate runs the serial ids exclusively.
        if !stopping
            && !serial_active
            && !serial.is_empty()
            && file_queue.is_empty()
            && !ids_left
            && states
                .iter()
                .enumerate()
                .all(|(i, s)| i == designate || s.dead)
            && !states[designate].dead
        {
            serial_active = true;
            let ids: Vec<String> = serial.drain(..).collect();
            let s = &mut states[designate];
            s.outstanding.extend(ids.iter().cloned());
            s.worker.send(&proto::Command::RunIds { ids })?;
            s.finishing = false;
        }

        // Release workers whose queue is exhausted FOR NOW. A collection in
        // flight ANYWHERE blocks release: its ids may be stolen by a drained
        // worker whose fixtures were torn down (premature NoMoreItems = re-setup).
        let collecting = states
            .iter()
            .any(|s| !s.dead && !s.uncollected_files.is_empty());
        let queue_empty = file_queue.is_empty() && !ids_left && !collecting;
        if queue_empty || stopping {
            for s in states
                .iter_mut()
                .filter(|s| s.ready && !s.dead && !s.finishing && s.uncollected_files.is_empty())
            {
                s.finishing = true;
                let _ = s.worker.send(&proto::Command::NoMoreItems);
            }
        }
        let in_flight = states.iter().any(|s| !s.dead && !s.outstanding.is_empty());
        let parallel_resolved = if stopping {
            !in_flight
        } else {
            queue_empty
                && !in_flight
                && states
                    .iter()
                    .all(|s| s.dead || s.uncollected_files.is_empty())
        };
        if parallel_resolved {
            let serial_pending = !stopping && (!serial.is_empty() || serial_active && in_flight);
            for (i, s) in states.iter_mut().enumerate() {
                // Gate on `ready` (sent LazyReady), NOT "was assigned a file":
                // a collection error can trip `stopping` before a ready worker
                // gets one, and skipping it hangs the run (done_workers < n).
                if s.dead || s.ended || !s.ready {
                    continue;
                }
                if serial_pending && i == designate {
                    continue;
                }
                let _ = s.worker.send(&proto::Command::EndSession);
                s.ended = true;
            }
        }
    }

    let mut workers: Vec<_> = states.into_iter().map(|s| s.worker).collect();
    for w in &mut workers {
        let _ = w.send(&proto::Command::Shutdown);
    }
    for w in workers {
        let _ = w.wait();
    }
    let mut exitstatus = crate::scheduling::pool::merge_statuses(&statuses);
    if collect_aborted {
        exitstatus = exitstatus.max(2);
    }
    if exitstatus == 0 && !run.all_passed() {
        exitstatus = 1;
    }
    if reruns > 0 && exitstatus == 1 && run.all_passed() {
        exitstatus = 0;
    }
    Ok(PoolOutcome {
        run,
        prog,
        fixtures,
        warnings,
        cache_dir,
        exitstatus,
    })
}

fn spawn_into(
    python: &Path,
    idx: usize,
    n: usize,
    args: &[String],
    tx: &mpsc::Sender<(usize, Result<Event>)>,
) -> Result<Worker> {
    let mut worker = Worker::spawn(python, Some((idx, n)))?;
    worker.send(&proto::Command::RunLazySession {
        args: args.to_vec(),
    })?;
    let tx = tx.clone();
    let mut reader = worker.take_reader();
    std::thread::spawn(move || loop {
        let event = reader.recv();
        let done = matches!(event, Ok(Event::Done { .. }) | Err(_));
        if tx.send((idx, event)).is_err() || done {
            break;
        }
    });
    Ok(worker)
}
