//! Worker process I/O: spawn a worker into a slot (with its reader thread)
//! and push item batches to it, honoring the dispatch queue's chunking.

use std::path::Path;
use std::sync::mpsc;

use anyhow::Result;

use crate::scheduling::proto::{self, Event};
use crate::scheduling::worker::Worker;

use super::dispatch::{Dispatch, Take};
use super::state::WorkerState;

/// Spawn a worker into slot `idx` and start its reader thread. Used for the
/// crash-respawn path (one fresh, independently spawned worker); the initial
/// pool uses [`Worker::spawn_pool`] + [`start_into`] so it can fork-prewarm.
pub(super) fn spawn_into(
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

/// Send the eager-session command to an already-spawned worker and start its
/// reader thread, mapping events to slot `idx`. Shared by [`spawn_into`] and the
/// fork-prewarmed initial pool.
pub(super) fn start_into(
    mut worker: Worker,
    idx: usize,
    args: &[String],
    tx: &mpsc::Sender<(usize, Result<Event>)>,
) -> Result<Worker> {
    worker.send(&proto::Command::RunItemsSession {
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

pub(super) fn dispatch_to(
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
            // Best-effort: a failed send means the worker is dying; its crash
            // event orphan-requeues `outstanding` (already includes these
            // items). Bailing the whole pool on the race killed crash-loop runs.
            if s.worker.send(&proto::Command::RunItems { indices }).is_ok() {
                // New items after a NoMoreItems: the held-item concern
                // returns, so the next exhaustion must re-release.
                s.finishing = false;
            }
        }
        Take::Exhausted => {
            // Queue exhausted FOR NOW. Release the worker's held last item
            // (nextitem lookahead); it keeps listening for reruns until
            // EndSession says every outcome is final.
            if !s.finishing {
                s.finishing = true;
                let _ = s.worker.send(&proto::Command::NoMoreItems);
            }
        }
    }
    Ok(())
}
