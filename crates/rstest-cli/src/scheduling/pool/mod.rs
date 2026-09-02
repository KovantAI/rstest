//! Multi-worker execution: in-worker item dispatch (the xdist model).
//!
//! Every worker runs an IDENTICAL session (same args), preserving pytest's
//! config/conftest/skip semantics. Workers verify by count+hash against the
//! first collection; ids ride the wire from the designated worker only.
//!
//! Seeding is barrier-free except the dispatch queue, which waits for the
//! designated worker's id list. Items flow in contiguous chunks (or whole-file
//! groups under --dist loadfile); cached long-poles go out first, individually.
//!
//! Gotcha: the worker holds its last pending item until it learns the
//! successor (nextitem lookahead), so dispatch thresholds must never reach 0
//! and no_more_items must be sent as soon as a worker's queue truly ends.
//!
//! SERIAL PHASE: @pytest.mark.serial items run on the designated worker
//! (lowest alive index) exclusively, after every other worker's session has
//! finished (Done = fixtures torn down, ports/DBs released).
//!
//! CRASH HANDLING: workers announce item_start before each test, so a dead
//! worker's in-flight item is reported failed and NOT retried (segfault-loop
//! guard); its outstanding items requeue; a capped-budget respawn reuses gwN.

mod dispatch;
mod io;
mod state;

pub(crate) use dispatch::chunk_size;

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::mpsc;

use anyhow::{bail, Result};

use crate::reporting::progress::Progress;
use crate::reporting::report::Run;
use crate::scheduling::proto::{self, Event};

use dispatch::{build_dispatch, Dispatch};
use io::{dispatch_to, spawn_into};
use state::WorkerState;

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
    /// Every worker runs the FULL suite (xdist --dist=each). No dispatch
    /// queue; a crash replacement gets only the dead worker's remaining
    /// items. Outcomes keyed "nodeid [gwN]" (every test runs N times).
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

/// The clean nodeid for a dispatched index, from the designate's id list.
fn nodeid_at(ids_store: &Option<Vec<String>>, index: u64) -> Option<&str> {
    ids_store
        .as_ref()
        .and_then(|v| v.get(index as usize))
        .map(String::as_str)
}

/// Whether `index` is rerun-eligible under `--reruns-only-known-flaky`: no gate
/// set (feature off) always passes; otherwise an explicit `@mark.flaky` (present
/// in `flaky_budget`) or a prior-flaky nodeid in the set qualifies. Matches on
/// the clean nodeid from `ids_store` (not a report id, which may carry a `[gwN]`
/// suffix in Dist::Each) so the ItemDone and crash gates stay consistent.
fn known_flaky_ok(
    known_flaky: Option<&std::collections::HashSet<String>>,
    flaky_budget: &std::collections::HashMap<u64, u32>,
    ids_store: &Option<Vec<String>>,
    index: u64,
) -> bool {
    known_flaky.is_none_or(|set| {
        flaky_budget.contains_key(&index)
            || nodeid_at(ids_store, index).is_some_and(|id| set.contains(id))
    })
}

/// Tell every still-listening worker the queue is closed (maxfail trip): each
/// finishes its in-flight work and ends. Bounded overshoot, the trade xdist makes.
fn stop_all(states: &mut [WorkerState]) {
    for s in states.iter_mut().filter(|s| !s.dead && !s.finishing) {
        s.finishing = true;
        let _ = s.worker.send(&proto::Command::NoMoreItems);
    }
}

#[allow(clippy::too_many_arguments)] // orchestration entry point; a config struct adds noise for one caller
pub fn run_pool(
    python: &Path,
    n: usize,
    args: &[String],
    mode: crate::reporting::progress::Mode,
    track_durations: bool,
    palette: crate::reporting::color::Palette,
    dist: Dist,
    maxfail: Option<u64>,
    reruns: u32,
    only_rerun: &[regex::Regex],
    worker_timeout: Option<std::time::Duration>,
    shuffle: Option<u64>,
    shard: Option<(usize, usize)>,
    // Some(set) => --reruns-only-known-flaky: only tests in this set (prior
    // flaky history) or explicitly @mark.flaky-marked are rerun-eligible.
    known_flaky: Option<&std::collections::HashSet<String>>,
) -> Result<PoolOutcome> {
    let (tx, rx) = mpsc::channel::<(usize, Result<Event>)>();

    let mut states = Vec::new();
    for idx in 0..n {
        let worker = spawn_into(python, idx, n, args, &tx)?;
        states.push(WorkerState::fresh(worker));
    }
    // NOTE: `tx` stays alive for respawns; the event loop exits via the
    // explicit done_workers break, not channel disconnect.

    let duration_cache = crate::scheduling::durations::load();
    let mut run = Run::default();
    run.track_phase_durations = track_durations;
    let mut prog = Progress::default();
    prog.set_palette(palette);
    // Json mode keeps stdout pure NDJSON: the footer's ANSI repaint would
    // corrupt the stream on a TTY, so skip it.
    if mode != crate::reporting::progress::Mode::Json {
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
    // worker is told no_more_items (it finishes in-flight work and ends;
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
                if maxfail.is_some_and(|limit| fail_count >= limit) && !stopping {
                    stopping = true;
                    stop_all(&mut states);
                }
            }
            Ok(Event::CollectError { path, longrepr }) => run.collect_error(path, longrepr),
            Ok(Event::DoctorFixtures { fixtures: fx }) => fixtures.extend(fx),
            Ok(Event::Warnings { entries }) => {
                // Per-test warnings are disjoint across workers; config and
                // collection warnings repeat in every session, so count those
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
                            // plugins etc.), so refuse rather than misassign.
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
                        // --shard: keep only bucket K's node-ids, deselecting
                        // the rest. Under an affinity dist mode we partition at
                        // group granularity so a group is never split (its contract).
                        let keep = shard.map(|(k, total)| {
                            let idx = match dist {
                                Dist::Loadfile | Dist::Loadscope => {
                                    let keys: Vec<Option<String>> = ids
                                        .iter()
                                        .map(|id| {
                                            Some(match dist {
                                                Dist::Loadfile => {
                                                    id.split("::").next().unwrap_or(id).to_string()
                                                }
                                                _ => id
                                                    .rsplit_once("::")
                                                    .map(|(head, _)| head)
                                                    .unwrap_or(id)
                                                    .to_string(),
                                            })
                                        })
                                        .collect();
                                    crate::scheduling::shard::shard_groups(
                                        &ids,
                                        &keys,
                                        &duration_cache,
                                        k,
                                        total,
                                    )
                                }
                                Dist::Loadgroup => {
                                    let g = groups.as_ref();
                                    let keys: Vec<Option<String>> = (0..ids.len())
                                        .map(|i| g.and_then(|m| m.get(&i.to_string()).cloned()))
                                        .collect();
                                    crate::scheduling::shard::shard_groups(
                                        &ids,
                                        &keys,
                                        &duration_cache,
                                        k,
                                        total,
                                    )
                                }
                                // Load has no affinity: per-test split is fine.
                                _ => crate::scheduling::shard::shard_indices(
                                    &ids,
                                    &duration_cache,
                                    k,
                                    total,
                                ),
                            };
                            eprintln!(
                                "rstest: shard {k}/{total} -> {} of {} test(s)",
                                idx.len(),
                                ids.len()
                            );
                            idx.into_iter().collect::<HashSet<u64>>()
                        });
                        dispatch = Some(build_dispatch(
                            &ids,
                            serial.unwrap_or_default(),
                            groups.unwrap_or_default(),
                            &duration_cache,
                            dist,
                            shuffle,
                            keep.as_ref(),
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
                let nodeid = nodeid_at(&ids_store, index)
                    .map(str::to_string)
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
                    // --reruns-only-known-flaky: gate on prior flaky history.
                    // An explicit @mark.flaky (present in flaky_budget) is an
                    // author declaration and always bypasses the gate. Match on
                    // the CLEAN nodeid from ids_store (keyed by index), not
                    // s.attempt's report nodeid: in Dist::Each the report id
                    // carries a ` [gwN]` suffix (added on receipt) and would
                    // never match the un-suffixed history set. Same lookup the
                    // crash path uses, keeping the two gates consistent.
                    let flaky_ok = known_flaky_ok(known_flaky, &flaky_budget, &ids_store, index);
                    let used = rerun_used.entry(index).or_insert(0);
                    if s.attempt_failed && *used < item_budget && rerun_allowed && flaky_ok {
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
                            stop_all(&mut states);
                        }
                    }
                }
                let s = &mut states[idx];
                // Refill when half-drained. Threshold must never be 0: the
                // worker HOLDS its last pending item (nextitem lookahead), so
                // outstanding floors at 1 until refilled; a 0 threshold deadlocks.
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
                    // Crashed item retries on the replacement worker when it
                    // has budget, bounded by BOTH rerun and restart budgets
                    // (segfault-loop guard); else reported failed, not retried.

                    // Gotcha: it appears in BOTH `running` and `outstanding`;
                    // the orphan loop must skip it by ORIGINAL identity even
                    // when the rerun branch clears `crashed`, else it runs twice.
                    let crashed_orig = crashed;
                    let mut crashed = crashed;
                    {
                        if let Some(i) = crashed {
                            // Same known-flaky gate as the ItemDone path: a
                            // crash of a non-known-flaky, unmarked test is not
                            // retried when --reruns-only-known-flaky is on.
                            let known_ok =
                                known_flaky_ok(known_flaky, &flaky_budget, &ids_store, i);
                            let used = rerun_used.entry(i).or_insert(0);
                            if *used < budget_of(&flaky_budget, i) && known_ok {
                                *used += 1;
                                if let Some(d) = dispatch.as_mut() {
                                    d.requeued.push_back(i);
                                }
                                crashed = None;
                            }
                        }
                    }
                    if let Some(i) = crashed {
                        let mut nodeid = nodeid_at(&ids_store, i)
                            .map(str::to_string)
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
                        // Serial phase needs a host; promote the lowest alive
                        // worker. A finishing worker can't host (session already
                        // draining); if none remain, serial items are lost (below).
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

        // Crash cleanup: hand each dead worker's workerinput to the lowest
        // surviving worker so its conftest's pytest_testnodedown runs with the
        // right idents (best-effort; a second crash mid-pipe loses it).
        while let Some((winput, error)) = pending_downs.pop_front() {
            match states.iter_mut().find(|s| !s.dead && !s.ended) {
                Some(s) => {
                    // Best-effort: the chosen worker may itself be dying (crash
                    // event not yet processed). Re-queue and retry next event;
                    // its death marks it dead and routing moves on.
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
        // unknown in that case, so warn.
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

        // Each-mode: a verified worker is seeded with the FULL suite (or a
        // crash replacement's remainder) and released immediately. No shared
        // queue and no reruns, so it drains then EndSessions independently.
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

        // Barrier-free seeding: any verified-collected worker starts once the
        // dispatch queue exists. Two dispatches each: a worker only RUNS an
        // item once it knows the successor, so a lone pending item never starts.
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

        // Session lifecycle: workers stay alive after draining so failed items
        // can rerun anywhere. EndSession goes out only when every outcome is
        // final (or the parallel portion is, for the serial phase wind-down).
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
    // one-by-one serializes N interpreter teardowns: the visible pause
    // between the last test and the summary on small suites.
    let mut workers: Vec<_> = states.into_iter().map(|s| s.worker).collect();
    for w in &mut workers {
        let _ = w.send(&proto::Command::Shutdown);
    }
    for w in workers {
        let _ = w.wait();
    }
    // Recorded outcomes win over session exit codes both ways: a fabricated
    // crash failure never hits a session (codes read 0), and a flaky test's
    // first attempt fails inside a session (code 1) though it finally passed.
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
}
