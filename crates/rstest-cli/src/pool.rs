//! Multi-worker execution: in-worker item dispatch (the xdist model).
//!
//! Every worker runs an IDENTICAL session (same args) — pytest's
//! config/conftest/skip semantics are preserved exactly. Workers collect in
//! parallel and verify by count+hash against the first collection seen;
//! full ids (+ serial-mark indices) ride the wire from the designated
//! worker only (D5).
//!
//! Seeding is BARRIER-FREE for everyone except the dispatch queue itself,
//! which waits for the designated worker's id list (duration ordering and
//! serial-mark safety both need it). Items flow in contiguous chunks
//! (module-fixture locality) — or whole-file groups under --dist loadfile —
//! except cached long-poles, which go out first and individually. The
//! worker holds its last pending item until it learns the successor
//! (nextitem lookahead), so dispatch thresholds must never reach 0 and
//! no_more_items must be sent as soon as a worker's queue truly ends.
//!
//! SERIAL PHASE: @pytest.mark.serial items are excluded from the parallel
//! queue. The designated worker (lowest alive index) is held open; once
//! every other worker's session has FINISHED (Done received — fixtures torn
//! down, ports/DBs released), the serial items run there exclusively.
//!
//! CRASH HANDLING: workers announce item_start before each test, so a dead
//! worker's in-flight item is known exactly. The crashed item is reported
//! failed and NOT retried (segfault-loop guard); its remaining outstanding
//! items requeue; a replacement respawns under the same gwN identity with a
//! capped total restart budget.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::mpsc;

use anyhow::{bail, Result};

use crate::progress::Progress;
use crate::proto::{self, Event};
use crate::report::Run;
use crate::worker::Worker;

/// Items per dispatch message (load mode). Contiguous ranges keep module
/// locality and cut protocol round-trips; sized so each worker sees ~16
/// refills.
pub(crate) fn chunk_size(total: usize, workers: usize) -> usize {
    (total / (workers * 16)).clamp(1, 64)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Dist {
    /// Dynamic chunks, duration-aware (default).
    Load,
    /// Whole-file groups: file affinity + in-file order preserved.
    /// The remedy for order-dependent suites (xdist --dist=loadfile).
    Loadfile,
    /// Fixture-scope affinity: a class's tests stay together, module
    /// functions stay with their module (xdist --dist=loadscope).
    Loadscope,
    /// @pytest.mark.xdist_group affinity across files; unmarked tests
    /// distribute individually (xdist --dist=loadgroup).
    Loadgroup,
    /// Every worker runs the FULL suite (xdist --dist=each:
    /// multi-environment validation). No item dispatch queue — each
    /// verified worker is seeded with every index; a crash replacement
    /// gets only the dead worker's remaining items (xdist semantics).
    /// Outcomes are keyed "nodeid [gwN]" (the run legitimately contains
    /// every test N times).
    Each,
}

pub struct PoolOutcome {
    pub run: Run,
    pub prog: Progress,
    pub fixtures: Vec<proto::FixtureStat>,
    pub warnings: Vec<proto::WarningEntry>,
    pub cache_dir: Option<String>,
    pub exitstatus: i32,
}

struct WorkerState {
    worker: Worker,
    collected: bool,
    seeded: bool,
    /// Told "queue exhausted for now" (NoMoreItems). Still listening!
    finishing: bool,
    /// Told EndSession (no resend).
    ended: bool,
    dead: bool,
    /// Indices dispatched but not yet item_done'd, in dispatch order.
    outstanding: VecDeque<u64>,
    /// The item the worker announced via item_start and hasn't finished.
    running: Option<u64>,
    /// When the in-flight item started (hang watchdog).
    running_since: Option<std::time::Instant>,
    /// Set when the watchdog killed this worker (better crash message).
    timeout_killed: bool,
    /// Reports of the in-flight attempt (only used when reruns are on).
    attempt: Vec<proto::Report>,
    attempt_failed: bool,
    /// workerinput snapshot (NodeInput) for crash-time testnodedown.
    node_input: Option<serde_json::Value>,
}

impl WorkerState {
    fn fresh(worker: Worker) -> Self {
        Self {
            worker,
            collected: false,
            seeded: false,
            finishing: false,
            ended: false,
            dead: false,
            outstanding: VecDeque::new(),
            running: None,
            running_since: None,
            timeout_killed: false,
            attempt: Vec::new(),
            attempt_failed: false,
            node_input: None,
        }
    }
}

/// Dispatch queue, built from the designated worker's id list.
struct Dispatch {
    /// Parallel-phase indices in dispatch order: cached long-poles first.
    order: Vec<u64>,
    /// Length of the long-pole prefix (dispatched one at a time).
    slow_count: usize,
    cursor: usize,
    /// Group end-positions in `order` (loadfile mode): a dispatch never
    /// splits a group.
    group_ends: Option<Vec<usize>>,
    /// Items reclaimed from crashed workers; served before `order`.
    requeued: VecDeque<u64>,
    /// @pytest.mark.serial items: run on the designate, exclusively,
    /// after all other workers are Done.
    serial: VecDeque<u64>,
    serial_active: bool,
}

enum Take {
    Items(Vec<u64>),
    Exhausted,
}

impl Dispatch {
    fn take(&mut self, want: usize, is_designate: bool) -> Take {
        let mut indices: Vec<u64> = Vec::new();
        while indices.len() < want {
            if let Some(i) = self.requeued.pop_front() {
                indices.push(i);
                continue;
            }
            if self.cursor < self.order.len() {
                match &self.group_ends {
                    None => {
                        indices.push(self.order[self.cursor]);
                        self.cursor += 1;
                    }
                    Some(ends) => {
                        // Whole-file group: take it all, regardless of `want`.
                        if !indices.is_empty() {
                            break; // one group per dispatch
                        }
                        // First boundary STRICTLY past cursor (a boundary
                        // can equal cursor when a group just ended).
                        let end = match ends.binary_search(&self.cursor) {
                            Ok(pos) => ends[pos + 1],
                            Err(pos) => ends[pos],
                        };
                        indices.extend_from_slice(&self.order[self.cursor..end]);
                        self.cursor = end;
                    }
                }
                continue;
            }
            break;
        }
        if !indices.is_empty() {
            return Take::Items(indices);
        }
        if !self.serial.is_empty() && self.serial_active && is_designate {
            let n = want.max(1).min(self.serial.len());
            return Take::Items(self.serial.drain(..n).collect());
        }
        // Serial items not yet runnable (or not ours): exhausted FOR NOW.
        // Workers keep listening after their queue release, so the serial
        // phase reaches the designate later as ordinary run_items.
        Take::Exhausted
    }
}

#[allow(clippy::too_many_arguments)] // orchestration entry point; a config struct adds noise for one caller
pub fn run_pool(
    python: &Path,
    n: usize,
    args: &[String],
    mode: crate::progress::Mode,
    track_durations: bool,
    palette: crate::color::Palette,
    dist: Dist,
    maxfail: Option<u64>,
    reruns: u32,
    only_rerun: &[regex::Regex],
    worker_timeout: Option<std::time::Duration>,
    shuffle: Option<u64>,
) -> Result<PoolOutcome> {
    let (tx, rx) = mpsc::channel::<(usize, Result<Event>)>();

    let mut states = Vec::new();
    for idx in 0..n {
        let worker = spawn_into(python, idx, n, args, &tx)?;
        states.push(WorkerState::fresh(worker));
    }
    // NOTE: `tx` stays alive for respawns; the event loop exits via the
    // explicit done_workers break, not channel disconnect.

    let duration_cache = crate::durations::load();
    let mut run = Run::default();
    run.track_phase_durations = track_durations;
    let mut prog = Progress::default();
    prog.set_palette(palette);
    // Json mode keeps stdout pure NDJSON — the footer's ANSI repaint would
    // corrupt the stream on a TTY, so skip it.
    if mode != crate::progress::Mode::Json {
        prog.enable_footer(n);
    }
    prog.set_mode(mode);
    let mut fixtures: Vec<proto::FixtureStat> = Vec::new();
    let mut warnings: Vec<proto::WarningEntry> = Vec::new();
    let mut statuses = Vec::new();
    // Reference collection: (count, hash) from whichever worker reports first.
    let mut reference: Option<(u64, String)> = None;
    // Designate's id list, retained for crash-report nodeids.
    let mut ids_store: Option<Vec<String>> = None;
    let mut total_items = 0usize;
    let mut dispatch: Option<Dispatch> = None;
    let mut done_workers = 0usize;
    let mut restarts_left = n.max(4);
    // Each-mode: a crash replacement runs only the dead worker's
    // REMAINING items (xdist semantics), stashed here per slot.
    let mut each_remnant: Vec<Option<Vec<u64>>> = vec![None; n];
    // Serial-phase designate: lowest alive index.
    let mut designate = 0usize;
    // testnodedown payloads of CRASHED workers, awaiting a surviving
    // worker to run the cleanup hooks (xdist's master does this natively;
    // rstest routes it to a sibling).
    let mut pending_downs: VecDeque<(serde_json::Value, String)> = VecDeque::new();
    let mut cache_dir: Option<String> = None;
    let mut rerun_used: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    // @pytest.mark.flaky(reruns=N) budgets, from the designate's payload.
    let mut flaky_budget: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    let budget_of = |flaky: &std::collections::HashMap<u64, u32>, i: u64| -> u32 {
        flaky.get(&i).copied().unwrap_or(reruns)
    };
    let mut fail_count = 0u64;
    // Global -x/--maxfail: once tripped, dispatch halts and every alive
    // worker is told no_more_items (it finishes in-flight work and ends —
    // bounded overshoot, same trade xdist makes).
    let mut stopping = false;

    loop {
        let (idx, event) = match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(pair) => pair,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                prog.tick();
                // Watchdog tick: kill workers stuck on one item too long.
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
            Ok(Event::Report(mut r)) => {
                if dist == Dist::Each {
                    // The run legitimately holds every test once PER
                    // WORKER; the keyed Run needs distinct ids.
                    r.nodeid.push_str(&format!(" [gw{idx}]"));
                }
                // With reruns on, reports of an in-flight item are buffered:
                // a failed attempt may be discarded and retried, and only
                // the FINAL attempt reaches the user/output/exit code.
                if let Some(i) = states[idx].running {
                    if budget_of(&flaky_budget, i) > 0 {
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
            Ok(Event::CollectError { path, longrepr }) => run.collect_error(path, longrepr),
            Ok(Event::DoctorFixtures { fixtures: fx }) => fixtures.extend(fx),
            Ok(Event::Warnings { entries }) => {
                // Per-test warnings are disjoint across workers; config and
                // collection warnings repeat in every session — count those
                // from worker 0 only.
                warnings.extend(
                    entries
                        .into_iter()
                        .filter(|e| idx == 0 || e.when == "runtest"),
                );
            }
            Ok(Event::CollectSkip { .. }) => {
                // Counted once per skipped collector; workers all see the
                // same skip (identical sessions) so attribute from worker 0
                // only to avoid N-fold counting.
                if idx == 0 {
                    run.collect_skips += 1;
                }
            }
            Ok(Event::CollectionDone {
                count,
                hash,
                ids,
                serial,
                cache_dir: cd,
                flaky,
                groups,
                locations: _,
                marks: _,
            }) => {
                if let Some(cd) = cd {
                    cache_dir.get_or_insert(cd);
                }
                if dist != Dist::Each {
                    if let Some(f) = flaky {
                        for (k, v) in f {
                            if let Ok(i) = k.parse::<u64>() {
                                flaky_budget.insert(i, v);
                            }
                        }
                    }
                }
                match &reference {
                    None => {
                        reference = Some((count, hash));
                        total_items = count as usize;
                        prog.set_total(if dist == Dist::Each {
                            total_items * states.len()
                        } else {
                            total_items
                        });
                    }
                    Some((ref_count, ref_hash)) => {
                        if *ref_count != count || *ref_hash != hash {
                            // Same args should collect identically; divergence
                            // means nondeterministic collection (random-order
                            // plugins etc.) — refuse rather than misassign.
                            for s in &mut states {
                                let _ = s.worker.send(&proto::Command::Shutdown);
                            }
                            bail!(
                                "workers collected different test sets \
                                 ({ref_count} vs {count} items); cannot dispatch safely. \
                                 Common causes: pytest-randomly without a fixed seed, or \
                                 parametrize IDs derived from time/randomness. \
                                 Workarounds: -p no:randomly, stable parametrize ids, or -n 0"
                            );
                        }
                    }
                }
                if let Some(ids) = ids {
                    if dispatch.is_none() && dist != Dist::Each {
                        dispatch = Some(build_dispatch(
                            &ids,
                            serial.unwrap_or_default(),
                            groups.unwrap_or_default(),
                            &duration_cache,
                            dist,
                            shuffle,
                        ));
                    }
                    ids_store.get_or_insert(ids);
                }
                states[idx].collected = true;
            }
            Ok(Event::NodeInput { workerinput }) => {
                states[idx].node_input = Some(workerinput);
            }
            Ok(Event::ItemStart { index }) => {
                states[idx].running = Some(index);
                states[idx].running_since = Some(std::time::Instant::now());
                let nodeid = ids_store
                    .as_ref()
                    .and_then(|v| v.get(index as usize))
                    .cloned()
                    .unwrap_or_else(|| format!("<item #{index}>"));
                prog.item_started(idx, nodeid);
            }
            Ok(Event::Stopped { unrun }) => {
                // Session-local -x tripped: those items never ran there.
                let s = &mut states[idx];
                s.finishing = true;
                for i in &unrun {
                    if let Some(pos) = s.outstanding.iter().position(|x| x == i) {
                        s.outstanding.remove(pos);
                    }
                }
                if !stopping {
                    // Not a global stop: redistribute to other workers.
                    if let Some(d) = dispatch.as_mut() {
                        d.requeued.extend(unrun);
                    }
                }
            }
            Ok(Event::ItemDone { index }) => {
                prog.item_finished(idx);
                let chunk = chunk_size(total_items, states.len());
                let s = &mut states[idx];
                s.running = None;
                s.running_since = None;
                if let Some(pos) = s.outstanding.iter().position(|&x| x == index) {
                    s.outstanding.remove(pos);
                }
                let item_budget = budget_of(&flaky_budget, index);
                if item_budget > 0 {
                    // --only-rerun: failures must match a pattern to retry.
                    let rerun_allowed = only_rerun.is_empty()
                        || s.attempt.iter().any(|r| {
                            r.outcome == "failed"
                                && r.longrepr
                                    .as_deref()
                                    .is_some_and(|t| only_rerun.iter().any(|re| re.is_match(t)))
                        });
                    let used = rerun_used.entry(index).or_insert(0);
                    if s.attempt_failed && *used < item_budget && rerun_allowed {
                        // Failed attempt with budget left: discard its
                        // reports and requeue the item.
                        *used += 1;
                        s.attempt.clear();
                        s.attempt_failed = false;
                        if let Some(d) = dispatch.as_mut() {
                            d.requeued.push_back(index);
                        }
                    } else {
                        let attempts = *used;
                        let failed_now = s.attempt_failed;
                        let nodeid = s.attempt.first().map(|r| r.nodeid.clone());
                        for r in s.attempt.drain(..) {
                            if r.outcome == "failed" {
                                fail_count += 1;
                            }
                            prog.on_report(Some(idx), &r);
                            run.record(Some(idx), r);
                        }
                        s.attempt_failed = false;
                        if !failed_now && attempts > 0 {
                            if let Some(nodeid) = nodeid {
                                run.mark_flaky(nodeid, attempts);
                            }
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
                let s = &mut states[idx];
                // Refill when half-drained. Threshold must never be 0: the
                // worker HOLDS its last pending item (nextitem lookahead),
                // so outstanding floors at 1 until more items or
                // no_more_items arrive — a 0 threshold deadlocks.
                if !stopping && s.outstanding.len() <= (chunk / 2).max(1) {
                    if let Some(d) = dispatch.as_mut() {
                        dispatch_to(s, d, chunk, idx == designate)?;
                    }
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
            // Lazy-mode events; a full-collection session never emits them.
            Ok(Event::LazyReady { .. })
            | Ok(Event::FileCollected { .. })
            | Ok(Event::ItemStartId { .. })
            | Ok(Event::ItemDoneId { .. })
            | Ok(Event::StoppedIds { .. }) => {}
            Err(e) => {
                if let Some(winput) = states[idx].node_input.take() {
                    pending_downs.push_back((winput, format!("{e:#}")));
                }
                let was_timeout = states[idx].timeout_killed;
                let crashed = states[idx].running.take();
                states[idx].attempt.clear();
                states[idx].attempt_failed = false;
                let orphaned: Vec<u64> = states[idx].outstanding.drain(..).collect();
                let restartable = states[idx].collected && restarts_left > 0;
                if restartable {
                    restarts_left -= 1;
                    // With rerun budget, a crashed item gets another chance
                    // on the replacement worker (still bounded by BOTH the
                    // rerun and restart budgets — the segfault-loop guard
                    // holds). Without budget: reported failed, not retried.
                    // The crashed item appears in BOTH `running` and
                    // `outstanding` (it was dispatched); the orphan loop
                    // below must skip it by its ORIGINAL identity even when
                    // the rerun branch clears `crashed`, or it requeues
                    // twice and runs twice.
                    let crashed_orig = crashed;
                    let mut crashed = crashed;
                    {
                        if let Some(i) = crashed {
                            let used = rerun_used.entry(i).or_insert(0);
                            if *used < budget_of(&flaky_budget, i) {
                                *used += 1;
                                if let Some(d) = dispatch.as_mut() {
                                    d.requeued.push_back(i);
                                }
                                crashed = None;
                            }
                        }
                    }
                    if let Some(i) = crashed {
                        let mut nodeid = ids_store
                            .as_ref()
                            .and_then(|v| v.get(i as usize))
                            .cloned()
                            .unwrap_or_else(|| format!("<collected item #{i}>"));
                        if dist == Dist::Each {
                            nodeid.push_str(&format!(" [gw{idx}]"));
                        }
                        let fab = proto::Report {
                            nodeid,
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
                        let crashed_id = fab.nodeid.clone();
                        prog.on_report(Some(idx), &fab);
                        run.record(Some(idx), fab);
                        run.mark_crashed(&crashed_id);
                    }
                    if dist == Dist::Each {
                        each_remnant[idx] = Some(
                            orphaned
                                .into_iter()
                                .filter(|&i| Some(i) != crashed_orig)
                                .collect(),
                        );
                    } else if let Some(d) = dispatch.as_mut() {
                        for i in orphaned.into_iter().filter(|&i| Some(i) != crashed_orig) {
                            d.requeued.push_back(i);
                        }
                    }
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
                    statuses.push(3); // pytest INTERNAL_ERROR
                    states[idx].dead = true;
                    done_workers += 1;
                    if idx == designate {
                        // Serial phase needs a host; promote the lowest
                        // alive worker (it hasn't been told no_more_items
                        // only if the queue hasn't ended — if it has, the
                        // serial items are lost and reported below).
                        // A finishing worker can't host serial (its
                        // session is already draining).
                        if let Some(next) = states.iter().position(|s| !s.dead && !s.finishing) {
                            designate = next;
                        } else if let Some(d) = &dispatch {
                            if !d.serial.is_empty() {
                                run.collect_error(
                                    "<serial phase>".into(),
                                    format!(
                                        "{} @serial tests lost: no worker left \
                                         to host the serial phase",
                                        d.serial.len()
                                    ),
                                );
                            }
                        }
                    }
                    if done_workers == states.len() {
                        break;
                    }
                }
            }
        }

        // Crash cleanup: hand each dead worker's workerinput to the
        // lowest surviving worker so its conftest's pytest_testnodedown
        // runs with the right idents (best-effort — a second crash while
        // the command is in the pipe loses it, as it can for xdist's
        // master too).
        while let Some((winput, error)) = pending_downs.pop_front() {
            match states.iter_mut().find(|s| !s.dead && !s.ended) {
                Some(s) => {
                    // Best-effort: the chosen worker may itself be dying
                    // (its crash event not yet processed). Re-queue and
                    // retry on the next event — its death marks it dead
                    // and routing moves on.
                    if let Err(_e) = s.worker.send(&proto::Command::NodeDown {
                        workerinput: winput.clone(),
                        error: error.clone(),
                    }) {
                        pending_downs.push_front((winput, error));
                        break;
                    }
                }
                None => {
                    eprintln!(
                        "rstest: no surviving worker to run pytest_testnodedown                          for a crashed worker; per-worker resources may leak"
                    );
                    break;
                }
            }
        }

        // If the designated id-carrier died before delivering ids, fall
        // back to identity order rather than stalling. Serial marks are
        // unknown in that case — warn.
        if dist != Dist::Each && dispatch.is_none() && reference.is_some() && states[0].dead {
            eprintln!(
                "rstest: id-carrier worker died before reporting; \
                 falling back to collection order (serial marks unknown)"
            );
            dispatch = Some(Dispatch {
                order: (0..total_items as u64).collect(),
                slow_count: 0,
                cursor: 0,
                group_ends: None,
                requeued: VecDeque::new(),
                serial: VecDeque::new(),
                serial_active: false,
            });
        }

        // Activate the serial phase once every non-designate worker has
        // fully finished its session (Done = fixtures torn down).
        if let Some(d) = dispatch.as_mut() {
            if !stopping
                && !d.serial_active
                && !d.serial.is_empty()
                && states
                    .iter()
                    .enumerate()
                    .all(|(i, s)| i == designate || s.dead)
                && !states[designate].dead
            {
                d.serial_active = true;
                let chunk = chunk_size(total_items, states.len());
                // Kick the (idle, held-open) designate: two dispatches so
                // the first serial item has a known successor.
                let s = &mut states[designate];
                dispatch_to(s, d, chunk, true)?;
                dispatch_to(s, d, chunk, true)?;
            }
        }

        // Each-mode: a verified worker is seeded with the FULL suite
        // (or, for a crash replacement, the dead worker's remainder) and
        // released immediately — there is no shared queue and no reruns,
        // so its lifecycle is independent: drain, then EndSession once
        // its outstanding work is gone.
        if dist == Dist::Each {
            if reference.is_some() {
                for (i, s) in states
                    .iter_mut()
                    .enumerate()
                    .filter(|(_, s)| s.collected && !s.seeded && !s.dead)
                {
                    s.seeded = true;
                    if stopping {
                        let _ = s.worker.send(&proto::Command::EndSession);
                        s.ended = true;
                        continue;
                    }
                    let indices: Vec<u64> = match each_remnant[i].take() {
                        Some(rem) => rem,
                        None => (0..total_items as u64).collect(),
                    };
                    s.outstanding.extend(indices.iter().copied());
                    // Best-effort sends (see dispatch_to): a dying
                    // worker's crash event re-seeds the remnant.
                    // Chunked sends keep messages bounded at pandas scale.
                    for chunk in indices.chunks(4096) {
                        if s.worker
                            .send(&proto::Command::RunItems {
                                indices: chunk.to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    // No shared queue and no reruns: release the held
                    // last item right away.
                    s.finishing = true;
                    let _ = s.worker.send(&proto::Command::NoMoreItems);
                }
            }
            for s in states
                .iter_mut()
                .filter(|s| s.seeded && !s.dead && !s.ended && s.outstanding.is_empty())
            {
                let _ = s.worker.send(&proto::Command::EndSession);
                s.ended = true;
            }
            continue;
        }

        // Barrier-free seeding: any verified-collected worker starts the
        // moment the dispatch queue exists. Two dispatches each — the
        // worker only RUNS an item once it knows the successor, so a
        // single pending item never starts.
        if let Some(d) = dispatch.as_mut() {
            let chunk = chunk_size(total_items, states.len());
            for (i, s) in states
                .iter_mut()
                .enumerate()
                .filter(|(_, s)| s.collected && !s.seeded && !s.dead)
            {
                s.seeded = true;
                if stopping {
                    // Late collector during a global stop: end it cleanly.
                    let _ = s.worker.send(&proto::Command::EndSession);
                    s.ended = true;
                    continue;
                }
                dispatch_to(s, d, chunk, i == designate)?;
                dispatch_to(s, d, chunk, i == designate)?;
            }
        }

        // Session lifecycle: workers stay alive after draining so failed
        // items can rerun anywhere. EndSession goes out only when every
        // outcome is final (or, for the serial phase, when the parallel
        // portion is final and the non-designates must wind down).
        if let Some(d) = dispatch.as_ref() {
            let in_flight = states.iter().any(|s| !s.dead && !s.outstanding.is_empty());
            let parallel_resolved = if stopping {
                !in_flight
            } else {
                d.cursor == d.order.len() && d.requeued.is_empty() && !in_flight
            };
            if parallel_resolved {
                let serial_pending = !stopping && !d.serial.is_empty();
                for (i, s) in states.iter_mut().enumerate() {
                    if s.dead || s.ended || !s.seeded {
                        continue;
                    }
                    if serial_pending && i == designate {
                        continue; // held open to host the serial phase
                    }
                    let _ = s.worker.send(&proto::Command::EndSession);
                    s.ended = true;
                }
            }
        }
    }

    // Parallel wind-down: send every shutdown first, THEN wait. Waiting
    // one-by-one serializes N interpreter teardowns — the visible pause
    // between the last test and the summary on small suites.
    let mut workers: Vec<_> = states.into_iter().map(|s| s.worker).collect();
    for w in &mut workers {
        let _ = w.send(&proto::Command::Shutdown);
    }
    for w in workers {
        let _ = w.wait();
    }
    // Fabricated crash failures never pass through any worker session, so
    // session exit codes alone can read 0 — the recorded outcomes win.
    // The reverse under --reruns: a flaky test's first attempt fails
    // INSIDE a session (exitstatus 1) but the recorded outcome is a pass —
    // there too the recorded outcomes win.
    let mut exitstatus = merge_statuses(&statuses);
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

/// SplitMix64: tiny, deterministic, platform-stable RNG for --shuffle.
/// The seed is printed for reproduction, so the permutation must be a
/// pure function of it — no std RandomState / platform variance.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Seeded Fisher-Yates.
fn shuffle_slice<T>(v: &mut [T], seed: u64) {
    let mut rng = SplitMix64(seed);
    for i in (1..v.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

fn build_dispatch(
    ids: &[String],
    serial: Vec<u64>,
    groups: std::collections::HashMap<String, String>,
    cache: &std::collections::HashMap<String, f64>,
    dist: Dist,
    shuffle: Option<u64>,
) -> Dispatch {
    let serial_set: HashSet<u64> = serial.iter().copied().collect();
    let parallel = || (0..ids.len() as u64).filter(|i| !serial_set.contains(i));

    let (order, slow_count, group_ends) = match dist {
        // Each mode never builds a dispatch queue (each worker is seeded
        // with the full suite); run_pool guards the call.
        Dist::Each => unreachable!("--dist each has no dispatch queue"),
        Dist::Load => {
            let full = crate::durations::dispatch_order(ids, cache);
            let order: Vec<u64> = full
                .into_iter()
                .filter(|i| !serial_set.contains(i))
                .collect();
            let slow_count = order
                .iter()
                .take_while(|&&i| {
                    cache
                        .get(&ids[i as usize])
                        .is_some_and(|&d| d >= crate::durations::SLOW_THRESHOLD_SECS)
                })
                .count();
            (order, slow_count, None)
        }
        // Affinity modes: collection order, grouped by a key; a dispatch
        // never splits a group. Duration reordering is off — affinity is
        // the point.
        Dist::Loadfile | Dist::Loadscope => {
            let key = |i: u64| -> &str {
                let id = ids[i as usize].as_str();
                match dist {
                    // whole file
                    Dist::Loadfile => id.split("::").next().unwrap_or(id),
                    // fixture scope: drop the last segment (test name) —
                    // class methods key on file::Class, module functions
                    // on the file.
                    _ => id.rsplit_once("::").map(|(head, _)| head).unwrap_or(id),
                }
            };
            let order: Vec<u64> = parallel().collect();
            let mut ends = Vec::new();
            for w in 1..order.len() {
                if key(order[w]) != key(order[w - 1]) {
                    ends.push(w);
                }
            }
            ends.push(order.len());
            (order, 0, Some(ends))
        }
        Dist::Loadgroup => {
            // Consolidate marked groups (possibly spanning files) into
            // contiguous units at the first member's position; unmarked
            // tests are singleton units (≈ load behavior).
            let mut units: Vec<Vec<u64>> = Vec::new();
            let mut unit_of: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for i in parallel() {
                match groups.get(&i.to_string()) {
                    Some(name) => match unit_of.get(name.as_str()) {
                        Some(&u) => units[u].push(i),
                        None => {
                            unit_of.insert(name.as_str(), units.len());
                            units.push(vec![i]);
                        }
                    },
                    None => units.push(vec![i]),
                }
            }
            let mut order = Vec::new();
            let mut ends = Vec::new();
            for unit in units {
                order.extend(unit);
                ends.push(order.len());
            }
            (order, 0, Some(ends))
        }
    };
    let (mut order, mut slow_count, mut group_ends, mut serial) =
        (order, slow_count, group_ends, serial);
    if let Some(seed) = shuffle {
        match group_ends.take() {
            // Affinity modes: shuffle GROUP order, keep each group's
            // internal order intact — in-group order is the affinity
            // contract (loadfile is the order-dependent-suite remedy).
            Some(ends) => {
                let mut units: Vec<&[u64]> = Vec::new();
                let mut start = 0;
                for &end in &ends {
                    units.push(&order[start..end]);
                    start = end;
                }
                shuffle_slice(&mut units, seed);
                let mut new_order = Vec::with_capacity(order.len());
                let mut new_ends = Vec::with_capacity(units.len());
                for unit in units {
                    new_order.extend_from_slice(unit);
                    new_ends.push(new_order.len());
                }
                order = new_order;
                group_ends = Some(new_ends);
            }
            // Load mode: the shuffle IS the order — duration-aware
            // long-pole-first sequencing is deliberately defeated.
            None => {
                shuffle_slice(&mut order, seed);
                slow_count = 0;
            }
        }
        // A different stream position for the serial phase, so it isn't
        // the same permutation pattern as the parallel one.
        shuffle_slice(&mut serial, seed.wrapping_add(1));
    }
    Dispatch {
        order,
        slow_count,
        cursor: 0,
        group_ends,
        requeued: VecDeque::new(),
        serial: serial.into(),
        serial_active: false,
    }
}

/// Spawn a worker into slot `idx` and start its reader thread.
fn spawn_into(
    python: &Path,
    idx: usize,
    n: usize,
    args: &[String],
    tx: &mpsc::Sender<(usize, Result<Event>)>,
) -> Result<Worker> {
    let mut worker = Worker::spawn(python, Some((idx, n)))?;
    worker.send(&proto::Command::RunItemsSession {
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

fn dispatch_to(
    s: &mut WorkerState,
    d: &mut Dispatch,
    chunk: usize,
    is_designate: bool,
) -> Result<()> {
    if s.dead || s.ended {
        return Ok(());
    }
    // Long-pole zone at the head of `order`: hand out ONE slow item per
    // dispatch so they spread across workers instead of stacking.
    let want = if d.cursor < d.slow_count && d.requeued.is_empty() {
        1
    } else {
        chunk
    };
    match d.take(want, is_designate) {
        Take::Items(indices) => {
            s.outstanding.extend(indices.iter().copied());
            // Best-effort: a failed send means the worker is dying — its
            // crash event will orphan-requeue `outstanding` (which already
            // includes these items). Bailing the whole pool on the race
            // killed crash-loop runs.
            if s.worker.send(&proto::Command::RunItems { indices }).is_ok() {
                // New items after a NoMoreItems: the held-item concern
                // returns, so the next exhaustion must re-release.
                s.finishing = false;
            }
        }
        Take::Exhausted => {
            // Queue exhausted FOR NOW. Release the worker's held last item
            // (nextitem lookahead) — it keeps listening for reruns until
            // EndSession says every outcome is final.
            if !s.finishing {
                s.finishing = true;
                let _ = s.worker.send(&proto::Command::NoMoreItems);
            }
        }
    }
    Ok(())
}

/// pytest exit codes: 0 ok, 1 tests failed, 2 interrupted, 3 internal error,
/// 4 usage error, 5 no tests collected. "No tests" in one worker is only
/// meaningful if EVERY worker says so.
pub(crate) fn merge_statuses(statuses: &[i32]) -> i32 {
    let severe = statuses.iter().filter(|&&s| matches!(s, 2..=4)).max();
    if let Some(&s) = severe {
        return s;
    }
    if statuses.contains(&1) {
        return 1;
    }
    if !statuses.is_empty() && statuses.iter().all(|&s| s == 5) {
        return 5;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn drain(d: &mut Dispatch, want: usize, designate: bool) -> Vec<Vec<u64>> {
        let mut batches = Vec::new();
        loop {
            match d.take(want, designate) {
                Take::Items(items) => batches.push(items),
                Take::Exhausted => return batches,
            }
        }
    }

    #[test]
    fn chunk_sizes() {
        // Small suites floor at 1; never exceed 64.
        assert_eq!(chunk_size(4, 8), 1);
        assert_eq!(chunk_size(1600, 2), 50);
        assert_eq!(chunk_size(1_000_000, 4), 64);
    }

    #[test]
    fn merge_status_rules() {
        assert_eq!(merge_statuses(&[]), 0);
        assert_eq!(merge_statuses(&[0, 0]), 0);
        assert_eq!(merge_statuses(&[0, 1]), 1);
        // "no tests" only when EVERY worker says so
        assert_eq!(merge_statuses(&[5, 5]), 5);
        assert_eq!(merge_statuses(&[5, 0]), 0);
        // severe codes dominate
        assert_eq!(merge_statuses(&[1, 2, 0]), 2);
        assert_eq!(merge_statuses(&[4, 1]), 4);
        assert_eq!(merge_statuses(&[3, 5]), 3);
    }

    #[test]
    fn load_orders_slow_first_and_excludes_serial() {
        let names = ids(&["t/a.py::t1", "t/a.py::t2", "t/b.py::t3", "t/b.py::t4"]);
        let mut cache = HashMap::new();
        cache.insert("t/b.py::t3".to_string(), 5.0); // long pole
        let d = build_dispatch(&names, vec![1], HashMap::new(), &cache, Dist::Load, None);
        // slow item first, serial index 1 absent, rest in collection order
        assert_eq!(d.order, vec![2, 0, 3]);
        assert_eq!(d.slow_count, 1);
        assert_eq!(d.serial, VecDeque::from(vec![1]));
    }

    #[test]
    fn shuffle_is_deterministic_and_defeats_duration_order() {
        let names = ids(&["t/a.py::t1", "t/a.py::t2", "t/b.py::t3", "t/b.py::t4"]);
        let mut cache = HashMap::new();
        cache.insert("t/b.py::t3".to_string(), 5.0);
        let a = build_dispatch(&names, vec![], HashMap::new(), &cache, Dist::Load, Some(7));
        let b = build_dispatch(&names, vec![], HashMap::new(), &cache, Dist::Load, Some(7));
        assert_eq!(a.order, b.order); // same seed, same order
        assert_eq!(a.slow_count, 0); // shuffle defeats long-pole-first
        let mut seen = a.order.clone();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]); // a permutation, nothing lost
                                            // Some seed must produce a different order than seed 7.
        assert!((0..20u64).any(|s| {
            build_dispatch(&names, vec![], HashMap::new(), &cache, Dist::Load, Some(s)).order
                != a.order
        }));
    }

    #[test]
    fn shuffle_keeps_groups_contiguous() {
        let names = ids(&[
            "t/a.py::t1",
            "t/a.py::t2",
            "t/a.py::t3",
            "t/b.py::t4",
            "t/b.py::t5",
        ]);
        for seed in 0..10u64 {
            let mut d = build_dispatch(
                &names,
                vec![],
                HashMap::new(),
                &HashMap::new(),
                Dist::Loadfile,
                Some(seed),
            );
            let batches = drain(&mut d, 1, false);
            // Whole files, in-file order intact — only group ORDER varies.
            assert!(
                batches == vec![vec![0, 1, 2], vec![3, 4]]
                    || batches == vec![vec![3, 4], vec![0, 1, 2]],
                "seed {seed}: {batches:?}"
            );
        }
    }

    #[test]
    fn loadfile_never_splits_a_file() {
        let names = ids(&[
            "t/a.py::t1",
            "t/a.py::t2",
            "t/a.py::t3",
            "t/b.py::t4",
            "t/b.py::t5",
        ]);
        let mut d = build_dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Loadfile,
            None,
        );
        // want=1 but whole groups must come out regardless
        let batches = drain(&mut d, 1, false);
        assert_eq!(batches, vec![vec![0, 1, 2], vec![3, 4]]);
    }

    #[test]
    fn loadscope_groups_by_class() {
        let names = ids(&[
            "t/a.py::TestX::t1",
            "t/a.py::TestX::t2",
            "t/a.py::TestY::t3",
            "t/a.py::t4",
        ]);
        let mut d = build_dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Loadscope,
            None,
        );
        let batches = drain(&mut d, 1, false);
        // TestX together; TestY alone; module-level function keys on the file
        assert_eq!(batches, vec![vec![0, 1], vec![2], vec![3]]);
    }

    #[test]
    fn loadgroup_consolidates_marks_across_files() {
        let names = ids(&[
            "t/a.py::t1", // group g
            "t/a.py::t2",
            "t/b.py::t3", // group g (different file)
            "t/b.py::t4",
        ]);
        let mut groups = HashMap::new();
        groups.insert("0".to_string(), "g".to_string());
        groups.insert("2".to_string(), "g".to_string());
        let mut d = build_dispatch(
            &names,
            vec![],
            groups,
            &HashMap::new(),
            Dist::Loadgroup,
            None,
        );
        let batches = drain(&mut d, 1, false);
        // marked group lands at its first member's position, cross-file;
        // unmarked tests are singleton units
        assert_eq!(batches, vec![vec![0, 2], vec![1], vec![3]]);
    }

    #[test]
    fn take_serves_requeued_before_queue() {
        let names = ids(&["a.py::t1", "a.py::t2", "a.py::t3"]);
        let mut d = build_dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Load,
            None,
        );
        d.requeued.push_back(2);
        match d.take(2, false) {
            Take::Items(items) => assert_eq!(items, vec![2, 0]),
            Take::Exhausted => panic!("expected items"),
        }
    }

    #[test]
    fn serial_only_for_active_designate() {
        let names = ids(&["a.py::t1", "a.py::t2"]);
        let mut d = build_dispatch(
            &names,
            vec![0, 1],
            HashMap::new(),
            &HashMap::new(),
            Dist::Load,
            None,
        );
        // parallel queue is empty (all serial); inactive phase = exhausted
        assert!(matches!(d.take(2, true), Take::Exhausted));
        d.serial_active = true;
        // non-designate never receives serial items
        assert!(matches!(d.take(2, false), Take::Exhausted));
        match d.take(2, true) {
            Take::Items(items) => assert_eq!(items, vec![0, 1]),
            Take::Exhausted => panic!("designate should get serial items"),
        }
    }
}
