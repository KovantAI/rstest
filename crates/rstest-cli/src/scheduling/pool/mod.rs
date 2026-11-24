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
pub use dispatch::FixtureGroup;
mod io;
mod replay;
mod state;

pub(crate) use dispatch::chunk_size;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::mpsc;

use anyhow::{bail, Result};

use crate::reporting::progress::Progress;
use crate::reporting::report::Run;
use crate::reporting::sink::Sink;
use crate::scheduling::orchestrator::{self, Dedup};
use crate::scheduling::proto::{self, Event};

use crate::scheduling::orchestrator::PoolCtl;
use crate::scheduling::reruns::Reruns;
use dispatch::{build_dispatch, Dispatch};
use io::{dispatch_to, feed, release_lookahead, seed_each, spawn_into, start_into};
use replay::{seed_replay, ReplayState};
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

impl std::str::FromStr for Dist {
    type Err = String;

    /// The single source of truth for the `--dist` name set: parsed once up
    /// front to validate, and again to map to the enum. `Err` carries the
    /// user-facing message so both call sites report identically.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "load" => Dist::Load,
            "loadfile" => Dist::Loadfile,
            "loadscope" => Dist::Loadscope,
            "loadgroup" => Dist::Loadgroup,
            "each" => Dist::Each,
            other => {
                return Err(format!(
                    "unknown --dist mode: {other} (use load|loadfile|loadscope|loadgroup|each)"
                ))
            }
        })
    }
}

/// Dispatch ordering under `--dist load`. Ignored by the affinity/each modes
/// (their order is the affinity contract, not a tunable).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Order {
    /// Slow tests first, to pack workers (best wall-clock throughput).
    #[default]
    Throughput,
    /// Recently failed, then near changed code, then flaky tests first, then
    /// the throughput order for the rest: earliest red signal, for `--watch` and PR CI (compose with
    /// `--maxfail`).
    FailFast,
}

impl std::str::FromStr for Order {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "throughput" => Order::Throughput,
            // Accept the hyphen spelling (the flag/doc form) and the
            // underscore for convenience.
            "fail-fast" | "failfast" | "fail_fast" => Order::FailFast,
            other => {
                return Err(format!(
                    "unknown --order mode: {other} (use throughput|fail-fast)"
                ))
            }
        })
    }
}

/// The `-v` line saying what fail-fast pulled to the front, so a run's order
/// is explainable.
fn failfast_note(l: crate::scheduling::durations::FailFastLead) -> String {
    if l.len() == 0 {
        return "rstest: fail-fast: no recent failures, changes or flakes; throughput order"
            .to_string();
    }
    format!(
        "rstest: fail-fast: leading with {} recent failures, {} near changes, {} flaky",
        l.failed, l.near, l.flaky
    )
}

/// Drop quarantined ids from the flake history fail-fast ranks on, so they
/// fall into the normal (clean) tail instead of leading the queue.
fn without_quarantined(
    mut history: std::collections::HashMap<String, crate::reporting::flakes::FlakeStats>,
    quarantine: Option<&regex::RegexSet>,
) -> std::collections::HashMap<String, crate::reporting::flakes::FlakeStats> {
    if let Some(q) = quarantine {
        history.retain(|id, _| !q.is_match(id));
    }
    history
}

/// `--budget`: the wall time to fill, and each test's median setup seconds
/// from the run history (its call seconds come from the duration cache).
pub struct Budget {
    pub secs: f64,
    /// As typed (`90s`), for the summary line.
    pub label: String,
    pub setup: std::collections::HashMap<String, f64>,
}

/// [`without_quarantined`] for co-failure partners: a quarantined test
/// neither leads nor gets pulled forward by another's failure.
fn without_quarantined_partners(
    mut cofail: crate::reporting::history::CoFail,
    quarantine: Option<&regex::RegexSet>,
) -> crate::reporting::history::CoFail {
    if let Some(q) = quarantine {
        cofail.retain(|id, _| !q.is_match(id));
        for partners in cofail.values_mut() {
            partners.retain(|(b, _)| !q.is_match(b));
        }
    }
    cofail
}

/// A counted failure of queue item `failed`: pull its co-failure partners
/// forward (a no-op unless fail-fast found some).
fn promote_partners(dispatch: &mut Option<Dispatch>, failed: Option<u64>) {
    if let (Some(d), Some(i)) = (dispatch.as_mut(), failed) {
        d.promote(i);
    }
}

/// Worker-pool parameters common to the eager (`run_pool`) and lazy
/// (`run_lazy_pool`) orchestrators. Bundled so each entry point takes a handful
/// of mode-specific args on top rather than one flat ~17-arg list.
pub struct PoolConfig<'a> {
    pub python: &'a Path,
    pub n: usize,
    pub args: &'a [String],
    pub mode: crate::reporting::progress::Mode,
    pub maxfail: Option<u64>,
    pub reruns: u32,
    pub only_rerun: &'a [regex::Regex],
    pub worker_timeout: Option<std::time::Duration>,
    /// Some(set) => --reruns-only-known-flaky: only tests in this set (prior
    /// flaky history) or explicitly @mark.flaky-marked are rerun-eligible.
    pub known_flaky: Option<&'a std::collections::HashSet<String>>,
    pub worker_env: &'a crate::scheduling::worker::WorkerEnv,
    /// `--fork-pool`: fork-prewarm the initial pool off one warm zygote (Unix
    /// only; ignored elsewhere / on the crash-respawn path). Pays the vendored
    /// pytest import once per run instead of once per worker.
    pub fork_prewarm: bool,
    /// The run's quarantine. Fail-fast ordering drops its ids from the suspect
    /// set: a quarantined test fails every run by design, so leading with it
    /// would trip `-x`/`--maxfail` and then be forgiven post-run. The `-x` /
    /// `--maxfail` count skips its failures (see [`orchestrator::MaxfailCount`]).
    pub quarantine: Option<&'a orchestrator::Quarantine>,
}

/// Everything the orchestrator loop produces from one pool run, handed back to
/// `run.rs` for post-run reporting and gates.
pub struct PoolOutcome {
    /// Merged per-test results across all workers.
    pub run: Run,
    /// The live progress renderer, carried out so the caller can print the
    /// closing summary in the same style.
    pub prog: Progress,
    /// Per-fixture setup timings (doctor mode), aggregated across workers.
    pub fixtures: Vec<proto::FixtureStat>,
    /// Modules/classes `--dist load` kept on one worker for their fixture
    /// cost (empty on paths that never group).
    pub fixture_groups: Vec<dispatch::FixtureGroup>,
    /// Deduplicated session/collect/runtest warnings.
    pub warnings: Vec<proto::WarningEntry>,
    /// pytest's cache dir (from the designate worker), where the merged
    /// lastfailed cache is written after the run.
    pub cache_dir: Option<String>,
    /// Reconciled process exit status for the run.
    pub exitstatus: i32,
    /// Full-collection identity for `--shard` verification: sha256 of the
    /// ordered nodeid list, as agreed by every worker. `None` on paths with no
    /// worker collection hash (single-worker / lazy).
    pub collection_hash: Option<String>,
    /// Number of collected tests (the reference count all workers agreed on);
    /// 0 when unknown.
    pub collection_size: u64,
    /// Wall time to spawn the initial worker pool (fork-prewarm zygote or plain
    /// per-worker spawns), for `--doctor` startup reporting. 0.0 on paths that
    /// don't spawn a pool (single-worker).
    pub startup_seconds: f64,
    /// The largest peak RSS any worker reported at its session end (bytes);
    /// `None` on the single-session path and where workers can't measure it.
    pub peak_rss: Option<u64>,
    /// Whether the initial pool was actually fork-prewarmed off a zygote (not
    /// merely requested: `--fork-pool` is a no-op off Unix and on paths that
    /// don't spawn a pool). Feeds the `--doctor` report.
    pub fork_prewarmed: bool,
    /// `--fork-pool=app`: the modules the workers imported, for the manifest.
    pub warm_record: crate::scheduling::warm::Record,
    /// `--fork-pool=app`: what the template imported (set after the run).
    pub warm: Option<crate::scheduling::warm::Stats>,
    /// pytest's rootdir and the collected test files' fingerprints, which the
    /// duration cache tags this run's timings with.
    pub sources: crate::scheduling::durations::Collected,
    /// Seconds one worker spends collecting the whole suite, for auto-lazy's
    /// long-pole budget (see `durations::COLLECT_FILE`). `None` when no worker
    /// reported it (single session, lazy pool).
    pub collect_seconds: Option<f64>,
    /// The run used lazy collection (its wall is recorded per mode, see
    /// `durations::MODE_WALL_FILE`).
    pub lazy: bool,
    /// Workers already sent `Shutdown` but not yet reaped, so their teardown
    /// (atexit handlers, coverage data files) overlaps the post-run saves.
    /// Reap with [`PoolOutcome::reap_workers`] before reading anything they
    /// write on exit, and before the run returns.
    pub exiting: Vec<crate::scheduling::worker::Worker>,
}

impl PoolOutcome {
    /// Wait for every [`PoolOutcome::exiting`] worker to exit.
    pub fn reap_workers(&mut self) {
        for w in self.exiting.drain(..) {
            let _ = w.wait();
        }
    }
}

/// The `--dist` name for a mode, for the replay journal (inverse of the
/// `FromStr` above; kept beside it so the two never drift).
fn dist_name(dist: Dist) -> &'static str {
    match dist {
        Dist::Load => "load",
        Dist::Loadfile => "loadfile",
        Dist::Loadscope => "loadscope",
        Dist::Loadgroup => "loadgroup",
        Dist::Each => "each",
    }
}

/// The clean nodeid for a dispatched index, from the designate's id list.
fn nodeid_at(ids_store: &Option<Vec<String>>, index: u64) -> Option<&str> {
    ids_store
        .as_ref()
        .and_then(|v| v.get(index as usize))
        .map(String::as_str)
}

/// The report nodeid for a dispatched index: the clean nodeid, with the
/// ` [gwN]` suffix under `--dist each` (every worker runs its own copy).
fn report_nodeid(ids_store: &Option<Vec<String>>, dist: Dist, worker: usize, index: u64) -> String {
    let mut nodeid = nodeid_at(ids_store, index)
        .map(str::to_string)
        .unwrap_or_else(|| format!("<collected item #{index}>"));
    if dist == Dist::Each {
        nodeid.push_str(&format!(" [gw{worker}]"));
    }
    nodeid
}

// The eager pool's knobs (dist/order/shuffle/shard/skip/pinned) are genuinely
// independent run-shaping inputs; bundling them buys no clarity over the named
// params, so this one call site keeps them flat.
#[allow(clippy::too_many_arguments)]
pub fn run_pool(
    cfg: &PoolConfig,
    dist: Dist,
    order: Order,
    track_durations: bool,
    shuffle: Option<u64>,
    shard: Option<(usize, usize)>,
    // --incremental: nodeids that were green last run and whose covered source
    // is unchanged. Collected but never dispatched; carried forward as cached
    // passes. Empty = feature off.
    skip_ids: &std::collections::HashSet<String>,
    // `--order fail-fast` and `--budget`: tests near changed code (see
    // `select::near_changes`). Empty under throughput or with no signal.
    near_changes: std::collections::HashMap<String, u8>,
    // `--budget`: run only what fits, picked after the shard and incremental
    // filters (see `durations::budget_select`).
    budget: Option<&Budget>,
    // `rstest replay`: a recorded per-worker schedule to re-pin. When Some, the
    // dynamic dispatch queue is bypassed entirely — each worker runs exactly its
    // recorded nodeids in order — and no new journal is written.
    pinned: Option<&crate::replay::PinnedSchedule>,
    sink: &mut Sink,
) -> Result<PoolOutcome> {
    let &PoolConfig {
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
        quarantine,
    } = cfg;
    let (tx, rx) = mpsc::channel::<(usize, Result<Event>)>();

    // Fork-prewarm the initial pool off one warm zygote when asked (Unix);
    // otherwise this is n independent spawns. Each worker then gets its session
    // command + reader thread via start_into. Time the spawn so --doctor can
    // report pool startup cost (the lever --fork-pool moves).
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
    // "Pool ready" = every initial worker has emitted its first event (imported
    // its core + started collecting). This is the startup window --fork-pool
    // shrinks; stamped in the event loop when all n have responded. Timing the
    // spawn call itself would be unfair — the plain path returns before its
    // workers import (that cost is paid async, off-thread), while the zygote
    // blocks on the shared import, so spawn duration understates one and
    // overstates the other.
    let mut ready_workers: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut startup_seconds = 0.0f64;
    // Each worker's collection phase (seconds), for `PoolOutcome::collect_seconds`.
    let mut collect_times: Vec<f64> = Vec::new();
    // NOTE: `tx` stays alive for respawns; the event loop exits via the
    // explicit done_workers break, not channel disconnect.

    let duration_cache = crate::scheduling::durations::load();
    let scope_costs = crate::scheduling::durations::load_scope_costs();
    // Fail-fast ranks on flake records, proximity to changes and co-failure
    // partners; --budget ranks on the first two. All empty (and untouched)
    // under the throughput default so a cold cache costs nothing. The run
    // history behind co-failure can be big (tens of MB), so it loads on a
    // thread while workers spawn and collect, and is joined only when the
    // queue is built.
    let mut signals = crate::scheduling::durations::FailFastSignals::default();
    let mut cofail_job = None;
    if order == Order::FailFast || budget.is_some() {
        let q = quarantine.map(|q| q.all.clone());
        signals.flakes = without_quarantined(crate::reporting::flakes::load(), q.as_ref());
        signals.near = near_changes;
        if order == Order::FailFast {
            cofail_job = Some(std::thread::spawn(move || {
                without_quarantined_partners(crate::reporting::history::load().cofail(), q.as_ref())
            }));
        }
    }
    let mut run = Run::default();
    run.track_phase_durations = track_durations;
    let mut prog = Progress::default();
    // Json mode keeps stdout pure NDJSON: the footer's ANSI repaint would
    // corrupt the stream on a TTY, so skip it.
    if mode != crate::reporting::progress::Mode::Json {
        prog.enable_footer(n, sink.palette().live());
    }
    prog.set_mode(mode);
    let mut fixtures: Vec<proto::FixtureStat> = Vec::new();
    let mut warnings: Vec<proto::WarningEntry> = Vec::new();
    let mut statuses = Vec::new();
    let mut peak_rss: Option<u64> = None;
    let mut warm_record = crate::scheduling::warm::Record::default();
    // Reference collection: (count, hash) from whichever worker reports first.
    let mut reference: Option<(u64, String)> = None;
    // Designate's id list, retained for crash-report nodeids.
    let mut ids_store: Option<Vec<String>> = None;
    let mut dispatch: Option<Dispatch> = None;
    let mut done_workers = 0usize;
    let mut restarts_left = n.max(4);
    // Each-mode: a crash replacement runs only the dead worker's
    // REMAINING items (xdist semantics), stashed here per slot.
    let mut each_remnant: Vec<Option<Vec<u64>>> = vec![None; n];
    // testnodedown payloads of CRASHED workers, awaiting a surviving
    // worker to run the cleanup hooks (xdist's master does this natively;
    // rstest routes it to a sibling).
    let mut pending_downs: VecDeque<(serde_json::Value, String)> = VecDeque::new();
    let mut cache_dir: Option<String> = None;
    let mut sources = crate::scheduling::durations::Collected::default();
    let mut retry = Reruns::new(reruns, only_rerun, known_flaky);
    // --incremental: nodeids collected but skipped (unchanged since last green),
    // injected as cached passes once the run finishes.
    let mut cached_ids: Vec<String> = Vec::new();
    let mut ctl = PoolCtl::new(maxfail, quarantine);
    // Initial seeding round: a worker whose second (lookahead) dispatch would
    // take work a not-yet-seeded worker needs (a long pole, an affinity group,
    // or the last items of a tiny suite) gets ONE dispatch as it collects; its
    // second goes out once every worker has its first, or after
    // SEED_ROUND_WAIT so a slow or respawning collector can't stall the rest.
    // Otherwise, and after the round closes, both go out at once.
    let mut seed_round_open = true;
    let mut seed_round_deadline: Option<std::time::Instant> = None;
    // A worker died with the restart budget spent: its items moved to
    // survivors, and anything left unrun at the end is reported as not run.
    let mut lost_worker = false;

    // Replay journaling: record each worker's ordered item_starts so the
    // schedule can be re-pinned later. Off during a replay (don't re-journal),
    // for --dist each (every worker runs everything — nothing to replay), for a
    // shard (partial suite), or when opted out. Nodeid, not index, so the
    // journal survives a machine hop (CI -> local).
    // Also off for a one-worker pool (`-n 1 --reruns N` runs through here): it
    // has no parallel schedule to reproduce, replay rejects it, and writing it
    // would clobber the last parallel run's latest.json.
    let journaling = pinned.is_none()
        && n >= 2
        && dist != Dist::Each
        && shard.is_none()
        && crate::replay::journaling_enabled();
    let mut recorder: Vec<Vec<String>> = if journaling {
        vec![Vec::new(); n]
    } else {
        Vec::new()
    };
    // Indices already journaled: a --reruns retry (same or another worker)
    // re-announces item_start, but the journal keeps only the FIRST attempt,
    // the one whose schedule produced the failure. Replay runs with reruns off,
    // so a duplicate entry would just run the test twice.
    let mut journaled: HashSet<u64> = HashSet::new();
    let mut replay = ReplayState::new(n);

    // SIGINT/SIGTERM: stop the workers and fall through to the wind-down
    // below, so the journal and reports are still written.
    let _interrupt = crate::scheduling::interrupt::Guard::install();
    loop {
        let tick = std::time::Duration::from_millis(500);
        let wait = match seed_round_deadline.filter(|_| seed_round_open) {
            Some(t) => t
                .saturating_duration_since(std::time::Instant::now())
                .min(tick),
            None => tick,
        };
        let received = rx.recv_timeout(wait);
        // Checked before the event is handled: the signal usually reached the
        // workers too, and their dying must not read as crashes to respawn.
        if let Some(sig) = crate::scheduling::interrupt::requested() {
            orchestrator::interrupt_all(
                sink,
                &mut run,
                &mut prog,
                &mut states,
                |w, s| {
                    let i = s.running?;
                    let mut id = nodeid_at(&ids_store, i)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("<collected item #{i}>"));
                    if dist == Dist::Each {
                        id.push_str(&format!(" [gw{w}]"));
                    }
                    Some(id)
                },
                sig,
            );
            statuses.push(2); // pytest INTERRUPTED
            break;
        }
        let (idx, event) = match received {
            Ok(pair) => pair,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if seed_round_open
                    && seed_round_due(
                        states.iter().map(|s| s.seeded || s.dead),
                        seed_round_deadline,
                        std::time::Instant::now(),
                    )
                {
                    if let Some(d) = dispatch.as_mut() {
                        seed_round_open = false;
                        let chunk = chunk_size(ctl.total_items, states.len());
                        release_lookahead(&mut states, d, chunk, ctl.designate, ctl.stopping)?;
                    }
                }
                prog.tick(sink);
                // Watchdog tick: kill workers stuck on one item too long.
                orchestrator::watchdog_tick(sink, &mut states);
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // First response from an initial worker (Ok event or a startup crash):
        // when all n have responded, the pool is warm — record the window.
        if ready_workers.len() < n && ready_workers.insert(idx) && ready_workers.len() == n {
            startup_seconds = spawn_start.elapsed().as_secs_f64();
        }
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
                    if retry.budget(&i) > 0 {
                        states[idx].attempt_failed |= r.outcome == "failed";
                        states[idx].attempt.push(r);
                        continue;
                    }
                }
                let counted = ctl.fail_count.n;
                ctl.fail_count.record(&r);
                if ctl.fail_count.n > counted {
                    promote_partners(&mut dispatch, states[idx].running);
                }
                prog.on_report(sink, Some(idx), &r);
                sink.emit_report(Some(idx), &r);
                run.record(Some(idx), r);
                ctl.trip_maxfail(&mut run, &mut states);
            }
            Ok(Event::CollectError { path, longrepr }) => {
                // Every worker collects the whole suite: show each error once.
                if run.collect_error(path.clone(), longrepr.clone()) {
                    prog.on_collect_error(sink, &path, &longrepr);
                    sink.emit_collect_error(&path, &longrepr);
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
                Dedup::Eager { idx },
                &mut run,
                &mut fixtures,
                &mut warnings,
            ),
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
                rootdir,
                args_source: _,
                root_args: _,
                inifile: _,
                order_flags: _,
                confcutdir: _,
                maxfail: reported_maxfail,
                deselected,
                collect_seconds,
            }) => {
                collect_times.extend(collect_seconds);
                run.deselected = run.deselected.max(deselected);
                // pytest's own resolution (argv + addopts, last wins) is
                // authoritative; it arrives before any item is dispatched.
                if reported_maxfail.is_some() {
                    ctl.maxfail = reported_maxfail;
                }
                if let Some(cd) = cd {
                    cache_dir.get_or_insert(cd);
                }
                if let Some(rd) = &rootdir {
                    sources.set_rootdir(rd);
                }
                if let Some(ids) = &ids {
                    sources.record(ids);
                    run.junit.set_collection_order(ids);
                }
                // Replay runs with reruns off, @mark.flaky included: a retried
                // attempt would be requeued onto a dispatch queue replay never
                // builds, silently dropping the failure being reproduced.
                if dist != Dist::Each && pinned.is_none() {
                    if let Some(f) = flaky {
                        for (k, v) in f {
                            if let Ok(i) = k.parse::<u64>() {
                                retry.marked.insert(i, v);
                            }
                        }
                    }
                }
                match &reference {
                    None => {
                        reference = Some((count, hash));
                        ctl.total_items = count as usize;
                        prog.set_total(if dist == Dist::Each {
                            ctl.total_items * states.len()
                        } else {
                            ctl.total_items
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
                                 Workers must collect the same ids in the same order. \
                                 Common causes: pytest-randomly without a fixed seed, \
                                 parametrize IDs derived from time/randomness, or \
                                 parametrize over a set (set/dict iteration order of \
                                 strings changes with PYTHONHASHSEED, random per process). \
                                 Workarounds: -p no:randomly, stable parametrize ids, \
                                 a list or sorted(...) instead of a set, a fixed \
                                 PYTHONHASHSEED for the run, or -n 0. \
                                 `rstest migrate-check` names the unstable sites"
                            );
                        }
                    }
                }
                if let Some(ids) = ids {
                    // The auto-quarantine cap is a share of the tests this run
                    // reports: the collected suite, or a shard's bucket below.
                    ctl.fail_count.set_total(ids.len());
                    if pinned.is_some() {
                        replay.serial.extend(serial.iter().flatten().copied());
                    }
                    // Replay pins the exact per-worker lists, so the dynamic
                    // dispatch queue is never built; seeding happens below.
                    if dispatch.is_none() && dist != Dist::Each && pinned.is_none() {
                        // --shard: keep only bucket K's node-ids, deselecting
                        // the rest. Under an affinity dist mode we partition at
                        // group granularity so a group is never split (its contract).
                        let keep = shard.map(|(k, total)| {
                            let idx = shard_partition(&ids, groups.as_ref(), dist, &duration_cache, k, total);
                            sink.warn(&format!(
                                "rstest: shard {k}/{total} -> {} of {} test(s)",
                                idx.len(),
                                ids.len()
                            ));
                            ctl.fail_count.set_total(idx.len());
                            idx.into_iter().collect::<HashSet<u64>>()
                        });
                        // --incremental: fold the skip set into `keep` — an index
                        // whose nodeid is green+unchanged is deselected. Any such
                        // index that would otherwise have run becomes a cached
                        // pass. (run.rs guards this off for shard/shuffle, so
                        // `keep` here is None unless sharding, which it isn't.)
                        let keep = if skip_ids.is_empty() {
                            keep
                        } else {
                            let (run_idx, mut cached, skipped_positions) =
                                partition_skip(&ids, keep.as_ref(), skip_ids);
                            if !cached.is_empty() {
                                // Count skipped POSITIONS, not deduped nodeids: a
                                // nodeid at K collected positions removes K test
                                // runs. `cached` (deduped) drives carry-forward;
                                // `skipped_positions` keeps the message and the
                                // progress total honest against `ids.len()`.
                                sink.warn(&format!(
                                    "rstest: --incremental: {} of {} test(s) unchanged since \
                                     last green -> skipped (cached)",
                                    skipped_positions,
                                    ids.len()
                                ));
                                // The progress total tracks only tests that run.
                                prog.set_total(ctl.total_items.saturating_sub(skipped_positions));
                            }
                            cached_ids.append(&mut cached);
                            Some(run_idx)
                        };
                        // --budget: what fits, from what the shard and the
                        // skip set left. Wall time already spent (pool start,
                        // collection) comes off the budget.
                        let keep = match budget {
                            None => keep,
                            Some(b) => {
                                let cost = budget_costs(&ids, &duration_cache, b);
                                let spent = spawn_start.elapsed().as_secs_f64();
                                let in_keep = |i: u64| keep.as_ref().is_none_or(|k| k.contains(&i));
                                let pick = crate::scheduling::durations::budget_select(
                                    &ids,
                                    &cost,
                                    &scope_costs,
                                    &signals.flakes,
                                    &signals.near,
                                    b.secs - spent,
                                    states.len(),
                                    in_keep,
                                );
                                sink.warn(&budget_summary(b, &pick, spent, states.len()));
                                run.budget_skipped = (0..ids.len() as u64)
                                    .filter(|&i| in_keep(i) && !pick.keep.contains(&i))
                                    .map(|i| ids[i as usize].clone())
                                    .collect();
                                ctl.fail_count.set_total(pick.keep.len());
                                prog.set_total(pick.keep.len());
                                Some(pick.keep)
                            }
                        };
                        if let Some(job) = cofail_job.take() {
                            signals.cofail = job.join().unwrap_or_default();
                        }
                        dispatch = build_dispatch(
                            &ids,
                            serial.unwrap_or_default(),
                            groups.unwrap_or_default(),
                            &duration_cache,
                            &signals,
                            &scope_costs,
                            states.len(),
                            dist,
                            order,
                            shuffle,
                            keep.as_ref(),
                        );
                        if mode == crate::reporting::progress::Mode::Verbose {
                            if let Some(l) = dispatch.as_ref().and_then(|d| d.failfast) {
                                sink.warn(&failfast_note(l));
                            }
                        }
                    }
                    ids_store.get_or_insert(ids);
                }
                states[idx].collected = true;
            }
            Ok(Event::NodeInput { workerinput }) => {
                states[idx].node_input = Some(workerinput);
            }
            Ok(Event::ItemStart { index, timeout }) => {
                states[idx].running = Some(index);
                states[idx].running_since = Some(std::time::Instant::now());
                states[idx].running_watchdog = orchestrator::watchdog_for(worker_timeout, timeout);
                let resolved = nodeid_at(&ids_store, index).map(str::to_string);
                // Journal the emergent schedule: worker `idx` started this
                // nodeid, in order. Recorded per worker so replay can re-pin it.
                if journaling && journaled.insert(index) {
                    if let Some(id) = &resolved {
                        recorder[idx].push(id.clone());
                    }
                }
                let nodeid = resolved.unwrap_or_else(|| format!("<item #{index}>"));
                prog.item_started(sink, idx, nodeid);
            }
            Ok(Event::Stopped { unrun }) => {
                // Either the reply to stop_run (the run is stopping), or the
                // worker's own session stopped (`session.shouldstop`, e.g.
                // --sw): those items never ran there.
                let s = &mut states[idx];
                s.finishing = true;
                s.stopped = true;
                for i in &unrun {
                    if let Some(pos) = s.outstanding.iter().position(|x| x == i) {
                        s.outstanding.remove(pos);
                    }
                }
                if !ctl.stopping {
                    // Not a global stop: the worker left its run loop, so it
                    // takes no more items (anything already sent comes back
                    // at its Done). Redistribute to the other workers.
                    s.ended = true;
                    if let Some(d) = dispatch.as_mut() {
                        d.requeued.extend(unrun);
                    }
                }
            }
            Ok(Event::ItemDone { index }) => {
                on_item_done(
                    idx,
                    index,
                    &mut states,
                    &mut dispatch,
                    &mut ctl,
                    &mut retry,
                    &ids_store,
                    &mut run,
                    &mut prog,
                    sink,
                )?;
            }
            Ok(Event::SessionExit { reason, returncode }) => {
                // pytest.exit() in a test ends the whole run, as under pytest:
                // every worker finishes its test in flight and starts nothing
                // more. The calling test never finished, so (like pytest) it
                // has no outcome. The worker's Done follows.
                let s = &mut states[idx];
                s.attempt.clear();
                s.attempt_failed = false;
                s.running_since = None;
                s.running_watchdog = None;
                if let Some(i) = s.running.take() {
                    if let Some(id) = nodeid_at(&ids_store, i) {
                        let id = if dist == Dist::Each {
                            format!("{id} [gw{idx}]")
                        } else {
                            id.to_string()
                        };
                        run.forget_unfinished(&id);
                    }
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
                // A session that ended on its own (see Stopped) never ran what
                // reached it after it left its loop: a refill or a rerun.
                // Requeue it for the live workers, unless the run is stopping.
                let left: Vec<u64> = s.outstanding.drain(..).collect();
                if !ctl.stopping {
                    if let Some(d) = dispatch.as_mut() {
                        d.requeued.extend(left);
                    }
                }
                done_workers += 1;
                if done_workers == states.len() {
                    break;
                }
            }
            // Lazy-mode events; a full-collection pool never emits them.
            Ok(Event::LazyReady { .. })
            | Ok(Event::FileCollected { .. })
            | Ok(Event::ItemStartId { .. })
            | Ok(Event::ItemDoneId { .. })
            | Ok(Event::StoppedIds { .. })
            // Single-session only.
            | Ok(Event::Interrupted {}) => {}
            Err(e) => {
                // Name the cause (exit status, stderr tail) before anything below
                // reaps the worker, which kills first and would hide it.
                let e = states[idx].worker.explain_failure(e);
                if let Some(winput) = states[idx].node_input.take() {
                    pending_downs.push_back((winput, format!("{e:#}")));
                }
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
                // Gotcha: the crashed item appears in BOTH `running` and
                // `outstanding`; `rest` excludes it by identity so it never
                // runs twice (whether retried below or reported failed).
                let rest: Vec<u64> = states[idx]
                    .outstanding
                    .drain(..)
                    .filter(|&i| Some(i) != crashed)
                    .collect();
                let restartable = states[idx].collected && restarts_left > 0;
                let report_crash =
                    retry_crashed(crashed, restartable, &mut retry, &ids_store, &mut dispatch);
                if let Some(i) = report_crash {
                    let fab = orchestrator::fabricate_crash_report(
                        report_nodeid(&ids_store, dist, idx, i),
                        killed_by,
                        idx,
                        &e,
                    );
                    orchestrator::record_fabricated(sink, &mut run, &mut prog, Some(idx), fab);
                }
                if restartable {
                    restarts_left -= 1;
                    if dist == Dist::Each {
                        each_remnant[idx] = Some(rest);
                    } else if pinned.is_some() && states[idx].seeded {
                        // Replay: the replacement runs only what this worker
                        // had left, in order. Without this it would be seeded
                        // with the full recorded list and re-run tests that
                        // already finished (including the crasher, in a loop
                        // until the restart budget ran out). A worker that died
                        // before it was seeded ran nothing, so its replacement
                        // takes the full recorded list (no remnant).
                        replay.remnant[idx] = Some(rest);
                    } else if let Some(d) = dispatch.as_mut() {
                        d.requeued.extend(rest);
                    }
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
                    // No replacement. Under --dist each and replay the items
                    // are bound to this slot (each worker runs its own copy;
                    // replay pins the recorded worker), so they cannot move:
                    // report them as not run. Otherwise survivors take them
                    // over (the idle-survivor kick below), and whatever no
                    // survivor is left to run is swept up after the loop.
                    if dist == Dist::Each || pinned.is_some() {
                        let held = replay.serial_held[idx].take().unwrap_or_default();
                        report_slot_lost(
                            idx,
                            rest.into_iter().chain(held),
                            dist,
                            &ids_store,
                            &mut run,
                            &mut prog,
                            sink,
                        );
                    } else if let Some(d) = dispatch.as_mut() {
                        d.requeued.extend(rest);
                    }
                    run.collect_error(
                        format!("<worker gw{idx}>"),
                        format!("worker terminated unexpectedly: {e:#}"),
                    );
                    statuses.push(3); // pytest INTERNAL_ERROR
                    states[idx].dead = true;
                    // Reap now (a decode error can leave the child alive) rather
                    // than letting it linger as a zombie until the end-of-run
                    // wait(). The slot stays in the vec, so reap in place.
                    states[idx].worker.reap();
                    done_workers += 1;
                    if idx == ctl.designate {
                        // Serial phase needs a host; promote the lowest worker
                        // still listening (a finishing one keeps listening, as
                        // for reruns). With none left the serial items are
                        // swept up as not run after the loop.
                        if let Some(next) = states
                            .iter()
                            .position(|s| !s.dead && !s.ended && !s.stopped)
                        {
                            ctl.designate = next;
                        }
                    }
                    if done_workers == states.len() {
                        break;
                    }
                }
            }
        }

        hand_off_node_downs(&mut pending_downs, &mut states, sink);

        // If the designated id-carrier died before delivering ids, fall
        // back to identity order rather than stalling. Serial marks are
        // unknown in that case, so warn. Not under replay: its `dispatch` is
        // always None by design (the pinned lists replace the queue), so a
        // gw0 that simply finished would otherwise trip this and hand the
        // whole suite to whichever workers are still open.
        if dist != Dist::Each
            && pinned.is_none()
            && dispatch.is_none()
            && reference.is_some()
            && states[0].dead
        {
            sink.warn(
                "rstest: id-carrier worker died before reporting; \
                 falling back to collection order (serial marks unknown)",
            );
            dispatch = Some(Dispatch {
                order: (0..ctl.total_items as u64).collect(),
                slow_count: 0,
                lead_ends: Vec::new(),
                fixture_groups: Vec::new(),
                failfast: None,
                partners: Default::default(),
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
            if !ctl.stopping
                && !d.serial_active
                && !d.serial.is_empty()
                && states
                    .iter()
                    .enumerate()
                    .all(|(i, s)| i == ctl.designate || s.dead)
                && !states[ctl.designate].dead
            {
                d.serial_active = true;
                let chunk = chunk_size(ctl.total_items, states.len());
                // Kick the (idle, held-open) designate: two dispatches so
                // the first serial item has a known successor.
                let s = &mut states[ctl.designate];
                dispatch_to(s, d, chunk, true)?;
                dispatch_to(s, d, chunk, true)?;
            }
        }

        // Replay: seed each worker with its recorded nodeids (resolved to this
        // run's indices), in the recorded order. Bypasses the dynamic queue
        // entirely, so like --dist each it drains and EndSessions on its own.
        if let Some(pin) = pinned {
            seed_replay(
                pin,
                &mut replay,
                &mut states,
                &ids_store,
                &reference,
                ctl.stopping,
                sink,
            );
            continue;
        }

        // Each-mode: a verified worker is seeded with the FULL suite (or a
        // crash replacement's remainder) and released immediately. No shared
        // queue and no reruns, so it drains then EndSessions independently.
        if dist == Dist::Each {
            // Wait for the designate's id list (unless it died without one):
            // a crash report needs it to name the test, and the id list can
            // lag another worker's CollectionDone.
            let ids_ready = reference.is_some() && (ids_store.is_some() || states[0].dead);
            seed_each(
                &mut states,
                &mut each_remnant,
                ctl.total_items,
                ids_ready,
                ctl.stopping,
            );
            continue;
        }

        if let Some(d) = dispatch.as_mut() {
            seed_new_workers(
                d,
                &mut states,
                ctl.total_items,
                ctl.designate,
                ctl.stopping,
                &mut seed_round_open,
                &mut seed_round_deadline,
            )?;
        }

        if let Some(d) = dispatch.as_mut().filter(|_| !ctl.stopping) {
            kick_idle_for_requeued(d, &mut states, ctl.total_items, ctl.designate)?;
        }

        if let Some(d) = dispatch.as_ref() {
            end_drained_sessions(d, &mut states, ctl.designate, ctl.stopping);
        }
    }

    // Parallel wind-down: send every shutdown now; the caller reaps them
    // after the post-run saves, so N interpreter teardowns overlap each other
    // and that work instead of pausing between the last test and the summary.
    let mut exiting: Vec<_> = states.into_iter().map(|s| s.worker).collect();
    for w in &mut exiting {
        let _ = w.send(&proto::Command::Shutdown);
    }
    // Every worker able to run the rest died for good: report what never ran
    // (reclaimed, never dispatched, or serial) instead of dropping it from the
    // artifacts. Not after a -x/--maxfail stop, where unrun tests are expected.
    if lost_worker && !ctl.stopping {
        if let Some(d) = dispatch.as_mut() {
            let unrun: Vec<u64> = d
                .requeued
                .drain(..)
                .chain(d.order.drain(d.cursor..))
                .chain(d.serial.drain(..))
                .collect();
            for i in unrun {
                let fab = orchestrator::fabricate_lost_report(
                    report_nodeid(&ids_store, dist, 0, i),
                    orchestrator::LOST_NO_WORKER,
                );
                orchestrator::record_fabricated(sink, &mut run, &mut prog, None, fab);
            }
        }
    }
    // --incremental: carry forward the skipped tests as cached passes so every
    // artifact reflects the whole suite. They passed last run and their source
    // is unchanged, so they never affect the exit status.
    for id in &cached_ids {
        run.record_cached(id.clone());
    }
    run.cap_total = ctl.fail_count.total;
    let retried = reruns > 0 || !run.flaky.is_empty();
    let exitstatus = orchestrator::finalize_exit(
        &statuses,
        run.all_passed(),
        retried,
        false,
        run.stopped_after.is_some(),
    );
    // pytest.exit(): its returncode is the run's exit code, as under pytest.
    let exitstatus = run
        .session_exit
        .as_ref()
        .map_or(exitstatus, |x| x.returncode);
    let (collection_size, collection_hash) = match reference {
        Some((count, hash)) => (count, Some(hash)),
        None => (0, None),
    };
    // Persist the schedule for `rstest replay`. Best-effort; a write failure
    // never affects the run's outcome.
    if journaling {
        crate::replay::write(&crate::replay::Journal::record(
            worker_env.run_uid.clone(),
            n,
            dist_name(dist).to_string(),
            shuffle,
            args.to_vec(),
            collection_hash.clone(),
            collection_size,
            std::mem::take(&mut recorder),
        ));
    }
    let fixture_groups = dispatch
        .as_mut()
        .map(|d| std::mem::take(&mut d.fixture_groups))
        .unwrap_or_default();
    Ok(PoolOutcome {
        run,
        prog,
        fixtures,
        fixture_groups,
        warnings,
        cache_dir,
        exitstatus,
        collection_hash,
        collection_size,
        startup_seconds,
        fork_prewarmed,
        warm_record,
        warm: None,
        sources,
        peak_rss,
        collect_seconds: {
            collect_times.sort_by(f64::total_cmp);
            crate::explain::median(&collect_times)
        },
        lazy: false,
        exiting,
    })
}

/// The crashed item retries only when BOTH the rerun and the restart budgets
/// allow (segfault-loop guard); otherwise it is reported failed, not retried.
/// Returns the item to report as crashed, if any.
fn retry_crashed(
    crashed: Option<u64>,
    restartable: bool,
    retry: &mut Reruns<u64>,
    ids_store: &Option<Vec<String>>,
    dispatch: &mut Option<Dispatch>,
) -> Option<u64> {
    let mut report_crash = crashed;
    if let (true, Some(i)) = (restartable, crashed) {
        // Same known-flaky gate as the ItemDone path: a crash of a
        // non-known-flaky, unmarked test is not retried when
        // --reruns-only-known-flaky is on.
        let known_ok = retry.known_flaky_ok(&i, nodeid_at(ids_store, i));
        let budget = retry.budget(&i);
        let used = retry.used.entry(i).or_insert(0);
        if *used < budget && known_ok {
            *used += 1;
            if let Some(d) = dispatch.as_mut() {
                d.requeued.push_back(i);
            }
            report_crash = None;
        }
    }
    report_crash
}

/// A worker died for good under `--dist each` or replay: its items are bound
/// to its slot (each worker runs its own copy; replay pins the recorded
/// worker), so they cannot move to a survivor. Report them as not run.
fn report_slot_lost(
    idx: usize,
    items: impl Iterator<Item = u64>,
    dist: Dist,
    ids_store: &Option<Vec<String>>,
    run: &mut Run,
    prog: &mut Progress,
    sink: &mut Sink,
) {
    let reason = format!(
        "worker gw{idx} crashed and the restart budget was spent; \
         under {} its tests cannot move to another worker",
        if dist == Dist::Each {
            "--dist each"
        } else {
            "replay"
        }
    );
    for i in items {
        let fab =
            orchestrator::fabricate_lost_report(report_nodeid(ids_store, dist, idx, i), &reason);
        orchestrator::record_fabricated(sink, run, prog, Some(idx), fab);
    }
}

/// A worker finished item `index`: retry it if its attempt failed with budget
/// left (and the `--only-rerun` / known-flaky gates allow), else record the
/// final attempt; then refill the worker once it is half-drained.
#[allow(clippy::too_many_arguments)]
fn on_item_done(
    idx: usize,
    index: u64,
    states: &mut [WorkerState],
    dispatch: &mut Option<Dispatch>,
    ctl: &mut PoolCtl,
    retry: &mut Reruns<u64>,
    ids_store: &Option<Vec<String>>,
    run: &mut Run,
    prog: &mut Progress,
    sink: &mut Sink,
) -> Result<()> {
    prog.item_finished(sink, idx);
    let chunk = chunk_size(ctl.total_items, states.len());
    let s = &mut states[idx];
    s.running = None;
    s.running_since = None;
    s.running_watchdog = None;
    if let Some(pos) = s.outstanding.iter().position(|&x| x == index) {
        s.outstanding.remove(pos);
    }
    // --dist each / replay: top up from the unsent backlog.
    feed(s, ctl.stopping);
    let item_budget = retry.budget(&index);
    if item_budget > 0 {
        // --only-rerun: failures must match a pattern to retry.
        let rerun_allowed = orchestrator::rerun_allowed(retry.only_rerun, &s.attempt);
        // --reruns-only-known-flaky: gate on prior flaky history.
        // An explicit @mark.flaky (present in `retry.marked`) is an
        // author declaration and always bypasses the gate. Match on
        // the CLEAN nodeid from ids_store (keyed by index), not
        // s.attempt's report nodeid: in Dist::Each the report id
        // carries a ` [gwN]` suffix (added on receipt) and would
        // never match the un-suffixed history set. Same lookup the
        // crash path uses, keeping the two gates consistent.
        let flaky_ok = retry.known_flaky_ok(&index, nodeid_at(ids_store, index));
        let used = retry.used.entry(index).or_insert(0);
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
            let flaky_key = s.attempt.first().map(|r| r.nodeid.clone());
            let attempt = std::mem::take(&mut s.attempt);
            s.attempt_failed = false;
            let counted = ctl.fail_count.n;
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
                    flaky_key,
                },
            );
            if ctl.fail_count.n > counted {
                promote_partners(dispatch, Some(index));
            }
            ctl.trip_maxfail(run, states);
        }
    }
    let s = &mut states[idx];
    // Refill when half-drained. Threshold must never be 0: the
    // worker HOLDS its last pending item (nextitem lookahead), so
    // outstanding floors at 1 until refilled; a 0 threshold deadlocks.
    if !ctl.stopping && s.outstanding.len() <= (chunk / 2).max(1) {
        if let Some(d) = dispatch.as_mut() {
            // An idle worker holds a lone new item (a rerun, the
            // serial phase) for its successor: dispatch again, as
            // when seeding, to send one or release it.
            let idle = s.outstanding.is_empty();
            dispatch_to(s, d, chunk, idx == ctl.designate)?;
            if idle {
                dispatch_to(s, d, chunk, idx == ctl.designate)?;
            }
        }
    }
    Ok(())
}

/// Seeding: any verified-collected worker is seeded once the dispatch
/// queue exists. Two dispatches each: a worker only RUNS an item once it
/// knows the successor, so a lone pending item never starts. During the
/// initial round the second may wait until every worker has its first
/// (see `seed_round_open`), so long poles, affinity groups and tiny
/// suites spread one per worker instead of pairing up on the earliest.
fn seed_new_workers(
    d: &mut Dispatch,
    states: &mut [WorkerState],
    total_items: usize,
    designate: usize,
    stopping: bool,
    seed_round_open: &mut bool,
    seed_round_deadline: &mut Option<std::time::Instant>,
) -> Result<()> {
    let chunk = chunk_size(total_items, states.len());
    let mut unseeded = states.iter().filter(|s| !s.seeded && !s.dead).count();
    for (i, s) in states
        .iter_mut()
        .enumerate()
        .filter(|(_, s)| s.collected && !s.seeded && !s.dead)
    {
        s.seeded = true;
        unseeded -= 1;
        if stopping {
            // Late collector during a global stop: end it cleanly.
            let _ = s.worker.send(&proto::Command::EndSession);
            s.ended = true;
            continue;
        }
        dispatch_to(s, d, chunk, i == designate)?;
        if *seed_round_open {
            seed_round_deadline.get_or_insert_with(|| std::time::Instant::now() + SEED_ROUND_WAIT);
        }
        if *seed_round_open && !s.finishing && seed_defers_lookahead(d, chunk, unseeded) {
            s.awaiting_lookahead = true;
        } else {
            dispatch_to(s, d, chunk, i == designate)?;
        }
    }
    if *seed_round_open
        && seed_round_due(
            states.iter().map(|s| s.seeded || s.dead),
            *seed_round_deadline,
            std::time::Instant::now(),
        )
    {
        *seed_round_open = false;
        release_lookahead(states, d, chunk, designate, stopping)?;
    }
    Ok(())
}

/// Requeued items (a rerun, or a stopped worker's leftovers) must also
/// reach idle workers: a worker with nothing outstanding sends no
/// ItemDone to refill on, so the queue would never drain and the run
/// would hang. Two dispatches, as for the serial phase: the first item
/// needs a successor or a NoMoreItems to start. Callers skip this while the run is stopping.
fn kick_idle_for_requeued(
    d: &mut Dispatch,
    states: &mut [WorkerState],
    total_items: usize,
    designate: usize,
) -> Result<()> {
    let chunk = chunk_size(total_items, states.len());
    for (i, s) in states.iter_mut().enumerate() {
        if d.requeued.is_empty() {
            break;
        }
        if s.seeded && !s.dead && !s.ended && !s.awaiting_lookahead && s.outstanding.is_empty() {
            dispatch_to(s, d, chunk, i == designate)?;
            dispatch_to(s, d, chunk, i == designate)?;
        }
    }
    // Items reclaimed from a worker that died for good have no
    // replacement to seed, and an idle survivor sends no ItemDone to
    // trigger a refill: kick it (twice, as when seeding, so a lone
    // item is released). A busy survivor refills on its next ItemDone.
    if !d.requeued.is_empty() {
        for (i, s) in states.iter_mut().enumerate() {
            if d.requeued.is_empty() {
                break;
            }
            if s.seeded && !s.dead && !s.ended && !s.stopped && s.outstanding.is_empty() {
                dispatch_to(s, d, chunk, i == designate)?;
                dispatch_to(s, d, chunk, i == designate)?;
            }
        }
    }
    Ok(())
}

/// `--shard K/N`: this bucket's indices. Under an affinity dist mode the
/// partition is at group granularity, so a group is never split (its
/// contract); `--dist load` splits per test.
fn shard_partition(
    ids: &[String],
    groups: Option<&HashMap<String, String>>,
    dist: Dist,
    duration_cache: &HashMap<String, f64>,
    k: usize,
    total: usize,
) -> Vec<u64> {
    match dist {
        Dist::Loadfile | Dist::Loadscope => {
            let keys: Vec<Option<String>> = ids
                .iter()
                .map(|id| {
                    Some(match dist {
                        Dist::Loadfile => id.split("::").next().unwrap_or(id).to_string(),
                        _ => id
                            .rsplit_once("::")
                            .map(|(head, _)| head)
                            .unwrap_or(id)
                            .to_string(),
                    })
                })
                .collect();
            crate::scheduling::shard::shard_groups(ids, &keys, duration_cache, k, total)
        }
        Dist::Loadgroup => {
            let keys: Vec<Option<String>> = (0..ids.len())
                .map(|i| groups.and_then(|m| m.get(&i.to_string()).cloned()))
                .collect();
            crate::scheduling::shard::shard_groups(ids, &keys, duration_cache, k, total)
        }
        // Load has no affinity: per-test split is fine.
        _ => crate::scheduling::shard::shard_indices(ids, duration_cache, k, total),
    }
}

/// `--budget`: each test's known cost, its cached call time plus its setup
/// time. Tests with neither are left out (unknown cost).
fn budget_costs(
    ids: &[String],
    duration_cache: &HashMap<String, f64>,
    b: &Budget,
) -> HashMap<String, f64> {
    ids.iter()
        .filter_map(|id| {
            let call = duration_cache.get(id).copied();
            let setup = b.setup.get(id).copied();
            (call.is_some() || setup.is_some())
                .then(|| (id.clone(), call.unwrap_or(0.0) + setup.unwrap_or(0.0)))
        })
        .collect()
}

/// Crash cleanup: hand each dead worker's workerinput to the lowest
/// surviving worker so its conftest's pytest_testnodedown runs with the
/// right idents (best-effort; a second crash mid-pipe loses it).
fn hand_off_node_downs(
    pending_downs: &mut VecDeque<(serde_json::Value, String)>,
    states: &mut [WorkerState],
    sink: &mut Sink,
) {
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
                sink.warn(
                    "rstest: no surviving worker to run pytest_testnodedown \
                     for a crashed worker; per-worker resources may leak",
                );
                break;
            }
        }
    }
}

/// Session lifecycle: workers stay alive after draining so failed items
/// can rerun anywhere. EndSession goes out only when every outcome is
/// final (or the parallel portion is, for the serial phase wind-down).
fn end_drained_sessions(
    d: &Dispatch,
    states: &mut [WorkerState],
    designate: usize,
    stopping: bool,
) {
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

/// Split collected indices into (run, cached, skipped_positions) for
/// The `--budget` line: what was picked and why, against the estimate.
fn budget_summary(
    b: &Budget,
    pick: &crate::scheduling::durations::BudgetPick,
    spent: f64,
    workers: usize,
) -> String {
    if pick.cold {
        return format!(
            "rstest: --budget {}: no recorded durations yet, running all {} test(s) \
             (this run records them)",
            b.label, pick.candidates
        );
    }
    let est = spent + pick.est_secs;
    let over = if est > b.secs {
        "; over budget: tests in changed test files always run"
    } else {
        ""
    };
    format!(
        "rstest: --budget {} -> {} of {} test(s), est. {est:.0}s on {workers} worker(s) \
         ({} changed-file, {} failed, {} near-change, {} flaky, {} fill){over}",
        b.label,
        pick.keep.len(),
        pick.candidates,
        pick.changed,
        pick.failed,
        pick.near,
        pick.flaky,
        pick.fill,
    )
}

/// `--incremental`: starting from `keep` (None = every index), deselect any
/// index whose nodeid is in `skip_ids` and move it to the cached set instead.
/// Cached nodeids are DEDUPLICATED — parametrized tests can share one nodeid
/// across positions, and carry-forward records each distinct nodeid once.
/// `skipped_positions` counts the actual removed indices (not deduped), so the
/// "N of M" message and the progress total stay honest against `ids.len()`.
fn partition_skip(
    ids: &[String],
    keep: Option<&HashSet<u64>>,
    skip_ids: &HashSet<String>,
) -> (HashSet<u64>, Vec<String>, usize) {
    let mut run_idx = HashSet::new();
    let mut cached = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut skipped_positions = 0usize;
    for i in 0..ids.len() as u64 {
        if !keep.is_none_or(|k| k.contains(&i)) {
            continue;
        }
        let id = ids[i as usize].as_str();
        if skip_ids.contains(id) {
            skipped_positions += 1;
            if seen.insert(id) {
                cached.push(id.to_string());
            }
        } else {
            run_idx.insert(i);
        }
    }
    (run_idx, cached, skipped_positions)
}

/// Longest the initial seeding round holds early workers' lookahead dispatch
/// waiting for the rest of the pool to finish collecting.
const SEED_ROUND_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether a worker just given its first dispatch must wait for its second
/// (lookahead): when that second take would be a long pole or an affinity
/// group, or would leave fewer items than the `unseeded` workers still need
/// one each. Otherwise it gets the next contiguous chunk at once, as before
/// (module locality, and it can start right away). An empty queue never
/// defers: the second dispatch just releases the held item.
fn seed_defers_lookahead(d: &Dispatch, chunk: usize, unseeded: usize) -> bool {
    d.pending() > 0
        && (d.group_ends.is_some() || d.in_slow_zone() || d.pending() < chunk + unseeded)
}

/// The initial seeding round closes once some worker was seeded (a deadline
/// is set) and either every slot is seeded or dead, or the deadline passed.
fn seed_round_due(
    mut settled: impl Iterator<Item = bool>,
    deadline: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    match deadline {
        None => false,
        Some(t) => now >= t || settled.all(|x| x),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_summary_names_the_pick_and_flags_cold_and_overrun() {
        use crate::scheduling::durations::BudgetPick;
        let b = Budget {
            secs: 60.0,
            label: "1m".into(),
            setup: Default::default(),
        };
        let pick = BudgetPick {
            keep: [0, 1, 2].into_iter().collect(),
            est_secs: 50.0,
            candidates: 10,
            changed: 1,
            failed: 1,
            fill: 1,
            ..Default::default()
        };
        assert_eq!(
            budget_summary(&b, &pick, 4.0, 2),
            "rstest: --budget 1m -> 3 of 10 test(s), est. 54s on 2 worker(s) \
             (1 changed-file, 1 failed, 0 near-change, 0 flaky, 1 fill)"
        );
        // Collection time counts: 50s of tests after 20s spent is over 60s.
        assert!(budget_summary(&b, &pick, 20.0, 2)
            .ends_with("; over budget: tests in changed test files always run"));
        let cold = BudgetPick {
            candidates: 10,
            cold: true,
            ..Default::default()
        };
        assert!(
            budget_summary(&b, &cold, 4.0, 2).contains("no recorded durations yet, running all 10")
        );
    }

    fn queue(len: u64, cursor: usize, slow_count: usize, groups: bool) -> Dispatch {
        Dispatch {
            order: (0..len).collect(),
            slow_count,
            lead_ends: Vec::new(),
            fixture_groups: Vec::new(),
            failfast: None,
            partners: Default::default(),
            cursor,
            group_ends: groups.then(|| vec![2, len as usize]),
            requeued: VecDeque::new(),
            serial: VecDeque::new(),
            serial_active: false,
        }
    }

    #[test]
    fn seed_defers_lookahead_only_when_it_would_starve_a_worker() {
        // Ordinary items with plenty left: the contiguous pair goes out at once.
        assert!(!seed_defers_lookahead(&queue(8, 1, 0, false), 1, 1));
        // Tiny suite: 2 tests at -n 2, the second belongs to the other worker.
        assert!(seed_defers_lookahead(&queue(2, 1, 0, false), 1, 1));
        // Next take is a long pole: spread them one per worker.
        assert!(seed_defers_lookahead(&queue(5, 1, 4, false), 1, 0));
        // Affinity groups: one group per worker first.
        assert!(seed_defers_lookahead(&queue(4, 2, 0, true), 1, 0));
        // Nothing left: the second dispatch just releases the held item.
        assert!(!seed_defers_lookahead(&queue(2, 2, 0, true), 1, 3));
    }

    #[test]
    fn seed_round_waits_for_every_worker_or_deadline() {
        let now = std::time::Instant::now();
        let later = now + SEED_ROUND_WAIT;
        // Nobody seeded yet: no deadline, never due.
        assert!(!seed_round_due([false, false].into_iter(), None, now));
        // One worker still collecting, deadline ahead: hold the lookahead.
        assert!(!seed_round_due([true, false].into_iter(), Some(later), now));
        // Every slot seeded (or dead): release.
        assert!(seed_round_due([true, true].into_iter(), Some(later), now));
        // A slow collector can't stall the pool past the deadline.
        assert!(seed_round_due([true, false].into_iter(), Some(now), later));
    }

    #[test]
    fn without_quarantined_drops_only_matching_ids() {
        use crate::reporting::flakes::FlakeStats;
        let red = FlakeStats {
            failed: 3,
            ..Default::default()
        };
        let h = std::collections::HashMap::from([
            ("q.py::always_red".to_string(), red),
            ("a.py::real_red".to_string(), red),
        ]);
        let q = regex::RegexSet::new(["^q\\.py::.*$"]).unwrap();
        let kept = without_quarantined(h.clone(), Some(&q));
        assert!(!kept.contains_key("q.py::always_red"));
        assert!(kept.contains_key("a.py::real_red"));
        // No --quarantine: history untouched.
        assert_eq!(without_quarantined(h, None).len(), 2);
    }

    #[test]
    fn order_from_str_parses_and_rejects() {
        assert_eq!("throughput".parse::<Order>().unwrap(), Order::Throughput);
        assert_eq!("fail-fast".parse::<Order>().unwrap(), Order::FailFast);
        assert_eq!("failfast".parse::<Order>().unwrap(), Order::FailFast);
        assert_eq!("fail_fast".parse::<Order>().unwrap(), Order::FailFast);
        assert_eq!(Order::default(), Order::Throughput);
        assert!("sideways".parse::<Order>().is_err());
    }

    #[test]
    fn partition_skip_dedups_and_splits() {
        // nodeid "a" appears at two positions; both are skippable and must
        // collapse to a single cached entry, while "b" runs.
        let ids = vec![
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
        ];
        let skip: HashSet<String> = ["a".to_string(), "c".to_string()].into_iter().collect();
        let (run, mut cached, skipped_positions) = partition_skip(&ids, None, &skip);
        assert_eq!(run, [1u64].into_iter().collect::<HashSet<u64>>());
        cached.sort();
        assert_eq!(cached, vec!["a".to_string(), "c".to_string()]);
        // "a" occupies two positions + "c" one: three runs skipped, two cached.
        assert_eq!(skipped_positions, 3);
    }

    #[test]
    fn partition_skip_respects_keep() {
        // Under a shard `keep` of {0,1}, index 2 ("c") is out of scope entirely;
        // "a" is skippable (cached), leaving only "b" to run.
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let skip: HashSet<String> = ["a".to_string()].into_iter().collect();
        let keep: HashSet<u64> = [0u64, 1].into_iter().collect();
        let (run, cached, skipped_positions) = partition_skip(&ids, Some(&keep), &skip);
        assert_eq!(run, [1u64].into_iter().collect::<HashSet<u64>>());
        assert_eq!(cached, vec!["a".to_string()]);
        // Only position 0 ("a") is both in scope and skipped.
        assert_eq!(skipped_positions, 1);
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
}
