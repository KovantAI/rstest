//! `rstest replay` in the eager pool: each worker is pinned to its recorded
//! nodeids (resolved to this run's indices) instead of the dispatch queue.

use std::collections::{HashMap, HashSet, VecDeque};

use super::io::{feed, seed_list};
use super::state::WorkerState;
use crate::reporting::sink::Sink;
use crate::scheduling::proto;

/// Replay state for translating the recorded assignment into this run's
/// indices and reporting drift/missing once.
pub(super) struct ReplayState {
    warned_drift: bool,
    missing: usize,
    missing_warned: bool,
    /// Nodeid -> this run's collected indices (ascending), built once when the
    /// id list arrives. A queue per nodeid so a nodeid collected at several
    /// positions (duplicate parametrize ids) maps each recorded occurrence to
    /// its own position instead of running the first one twice.
    index: Option<HashMap<String, VecDeque<u64>>>,
    /// A crashed worker's not-yet-run pinned remainder (this run's indices,
    /// recorded order), handed to its replacement instead of the whole
    /// recorded list. Mirrors `each_remnant`.
    pub(super) remnant: Vec<Option<Vec<u64>>>,
    /// This run's @serial indices, and each slot's pinned serial tail held
    /// back until every other worker has finished. The recording ran them in
    /// the serial phase (alone); replaying them as soon as their worker got
    /// there would run them concurrently with other workers' tests.
    pub(super) serial: HashSet<u64>,
    pub(super) serial_held: Vec<Option<Vec<u64>>>,
}

impl ReplayState {
    pub(super) fn new(n: usize) -> Self {
        Self {
            warned_drift: false,
            missing: 0,
            missing_warned: false,
            index: None,
            remnant: vec![None; n],
            serial: HashSet::new(),
            serial_held: vec![None; n],
        }
    }
}

/// Seed each worker with its recorded nodeids, in the recorded order, release
/// held serial tails, and end the sessions of workers that drained.
pub(super) fn seed_replay(
    pin: &crate::replay::PinnedSchedule,
    replay: &mut ReplayState,
    states: &mut [WorkerState],
    ids_store: &Option<Vec<String>>,
    reference: &Option<(u64, String)>,
    stopping: bool,
    sink: &mut Sink,
) {
    // Seed only once the id list has arrived (it rides on the designate's
    // CollectionDone, which may lag another worker's). Each worker is
    // seeded as it becomes collected (staggered, like the each-branch),
    // so there is no latch that could skip a late collector.
    if let Some(ids) = ids_store.as_ref() {
        if !replay.warned_drift {
            replay.warned_drift = true;
            warn_replay_drift(sink, pin, reference);
        }
        let index_of = replay.index.get_or_insert_with(|| {
            let mut m: HashMap<String, VecDeque<u64>> = HashMap::with_capacity(ids.len());
            for (i, id) in ids.iter().enumerate() {
                m.entry(id.clone()).or_default().push_back(i as u64);
            }
            m
        });
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
            let indices: Vec<u64> = match replay.remnant[i].take() {
                // A crash replacement: only the dead worker's remainder.
                Some(rest) => rest,
                None => pin
                    .assignment
                    .get(i)
                    .into_iter()
                    .flatten()
                    .filter_map(|id| {
                        let at = index_of.get_mut(id.as_str()).and_then(VecDeque::pop_front);
                        if at.is_none() {
                            replay.missing += 1;
                        }
                        at
                    })
                    .collect(),
            };
            let (serial_tail, indices): (Vec<u64>, Vec<u64>) =
                indices.into_iter().partition(|i| replay.serial.contains(i));
            if !serial_tail.is_empty() {
                replay.serial_held[i]
                    .get_or_insert_with(Vec::new)
                    .extend(serial_tail);
            }
            pin_send(s, indices);
        }
        // Serial phase: release one slot's held serial tail once every
        // other worker has finished its session (Done), the rule the
        // normal serial phase uses. A worker that is itself idle and
        // waiting on a serial tail doesn't block; the lowest such slot
        // goes first and the next goes after it ends, so serial tests
        // never overlap anything.
        // A slot that died for good (restart budget spent) is skipped: its tail
        // is lost with it (already reported as a worker failure), and
        // waiting on it would stall every later slot.
        if let Some(i) =
            (0..states.len()).find(|&i| replay.serial_held[i].is_some() && !states[i].dead)
        {
            if stopping {
                replay.serial_held.iter_mut().for_each(|h| *h = None);
            } else {
                let idle = |s: &WorkerState| s.seeded && s.outstanding.is_empty();
                let others_done = states
                    .iter()
                    .enumerate()
                    .all(|(j, o)| j == i || o.dead || (replay.serial_held[j].is_some() && idle(o)));
                let s = &mut states[i];
                if others_done && !s.dead && idle(s) {
                    let tail = replay.serial_held[i].take().unwrap_or_default();
                    pin_send(s, tail);
                }
            }
        }
        // Once every worker is seeded, report any recorded tests that no
        // longer collect (renamed/removed since the journal), just once.
        // Skipped when nothing collected: the drift note already
        // explained that, and "renamed or removed" would mislead.
        if !replay.missing_warned
            && replay.missing > 0
            && !ids.is_empty()
            && states.iter().all(|s| s.seeded || s.dead)
        {
            replay.missing_warned = true;
            sink.warn(&format!(
                "rstest: replay: {} recorded test(s) are no longer collected \
                 (renamed or removed since the journal) and were skipped",
                replay.missing
            ));
        }
    }
    if stopping {
        states.iter_mut().for_each(|s| feed(s, true));
    }
    for (_, s) in states.iter_mut().enumerate().filter(|(i, s)| {
        s.seeded
            && !s.dead
            && !s.ended
            && s.outstanding.is_empty()
            && replay.serial_held[*i].is_none()
    }) {
        let _ = s.worker.send(&proto::Command::EndSession);
        s.ended = true;
    }
}

/// Warn once at replay time if this run's collection differs from the recorded
/// one (tests added/removed/renamed since the journal). Replay proceeds with the
/// tests that still match; the exact interleaving may differ.
fn warn_replay_drift(
    sink: &mut Sink,
    pin: &crate::replay::PinnedSchedule,
    reference: &Option<(u64, String)>,
) {
    let Some((count, hash)) = reference else {
        return;
    };
    let size_drift = pin.collection_size != 0 && pin.collection_size != *count;
    let hash_drift = pin
        .collection_hash
        .as_deref()
        .is_some_and(|h| h != hash.as_str());
    if *count == 0 && pin.collection_size > 0 {
        // Nothing collected is not "the suite changed": it is almost always
        // the wrong directory or a recorded path that doesn't exist here.
        sink.warn(&format!(
            "rstest: replay: collected no tests here (the journal had {}). Run replay from \
             the directory the recording ran in, and check that the recorded test paths \
             exist on this machine.",
            pin.collection_size
        ));
    } else if size_drift || hash_drift {
        sink.warn(&format!(
            "rstest: replay: the suite changed since the journal was recorded \
             (now {count} test(s), journal had {}). Replaying the tests that still \
             match; the exact interleaving may differ.",
            pin.collection_size
        ));
    }
}

/// Replay: hand `indices` to a worker and release its held last item, fed a
/// chunk at a time ([`seed_list`]). Called again for a slot's serial tail; the
/// worker keeps draining, so items sent after a NoMoreItems still run.
fn pin_send(s: &mut WorkerState, indices: Vec<u64>) {
    if !indices.is_empty() {
        seed_list(s, indices);
    }
}
