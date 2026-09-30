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

/// Indices per `RunItems` message when feeding a backlog. Small enough that a
/// message (at most ~5 bytes per index) fits any pipe buffer (4 KiB on
/// Windows), so a write into an empty pipe never blocks.
pub(super) const FEED_CHUNK: usize = 512;

/// Assign `indices` to a worker whose whole list is known up front
/// (`--dist each`, replay) and release it once they are all sent. The list
/// goes out through [`feed`], not in one burst: the worker reads commands only
/// between tests, so a burst larger than the pipe buffer would block the
/// event loop (other workers' seeding, event handling, the watchdog) until
/// this worker ran most of its list.
pub(super) fn seed_list(s: &mut WorkerState, indices: Vec<u64>) {
    s.outstanding.extend(indices.iter().copied());
    s.backlog.extend(indices);
    s.release_after_backlog = true;
    feed(s, false);
}

/// Top up a worker from its backlog: keep at most two chunks sent but not
/// done. A worker runs a chunk's last item only after reading the next
/// message, so once no more than one chunk is in flight the pipe is empty
/// and the next write cannot block. Called on seeding and on each ItemDone.
/// Under a global stop the unsent backlog is dropped (never ran, never will),
/// leaving `stop_all`'s NoMoreItems to release the worker.
pub(super) fn feed(s: &mut WorkerState, stopping: bool) {
    if s.dead {
        return;
    }
    if stopping {
        let unsent = s.backlog.len();
        s.backlog.clear();
        s.outstanding.truncate(s.outstanding.len() - unsent);
        s.release_after_backlog = false;
        return;
    }
    while !s.backlog.is_empty() && s.outstanding.len() - s.backlog.len() <= FEED_CHUNK {
        let take = FEED_CHUNK.min(s.backlog.len());
        let indices: Vec<u64> = s.backlog.drain(..take).collect();
        // Best-effort (see dispatch_to): a dying worker's crash event
        // reclaims `outstanding`, which still holds these and the backlog.
        if s.worker
            .send(&proto::Command::RunItems { indices })
            .is_err()
        {
            return;
        }
    }
    if s.backlog.is_empty() && s.release_after_backlog {
        // No shared queue and no reruns: release the held last item.
        s.release_after_backlog = false;
        s.finishing = true;
        let _ = s.worker.send(&proto::Command::NoMoreItems);
    }
}

pub(super) fn dispatch_to(
    s: &mut WorkerState,
    d: &mut Dispatch,
    chunk: usize,
    is_designate: bool,
) -> Result<()> {
    // A stopped worker left its run loop: anything sent now would never run.
    if s.dead || s.ended || s.stopped {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::scheduling::worker::WorkerEnv;

    /// A worker process that never reads its command pipe (from the
    /// orchestrator's side, a worker busy running a long list). An unbounded
    /// write to it blocks once the pipe buffer fills.
    fn silent_worker(tag: &str) -> (WorkerState, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let script = std::env::temp_dir().join(format!(
            "rstest-silent-worker-{}-{tag}.sh",
            std::process::id()
        ));
        std::fs::write(&script, "#!/bin/sh\nexec sleep 60\n").expect("write stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stand-in");
        let env = WorkerEnv {
            run_uid: "uid-feed".into(),
            doctor: false,
            timeout: None,
            leakcheck: false,
            send_ids: false,
            debug_port: None,
            stream_output: false,
            junitxml: None,
            quarantine: None,
        };
        let worker = Worker::spawn(&script, Some((0, 1)), &env).expect("spawn stand-in");
        (WorkerState::fresh(worker), script)
    }

    #[test]
    fn seed_list_feeds_two_chunks_then_tops_up_per_item_done() {
        // A 60k-item seed (--dist each at scale) written in one go fills the
        // pipe and blocks here until the worker reads it; fed, it returns at
        // once with only two chunks sent.
        let (mut s, script) = silent_worker("topup");
        let n = 60_000usize;
        seed_list(&mut s, (0..n as u64).collect());
        assert_eq!(s.outstanding.len(), n);
        assert_eq!(s.backlog.len(), n - 2 * FEED_CHUNK);
        assert!(!s.finishing, "released before the backlog was fed");
        // ItemDones through the first chunk: exactly one more chunk goes out,
        // once the worker must have read everything sent before it.
        for _ in 0..FEED_CHUNK - 1 {
            s.outstanding.pop_front();
            feed(&mut s, false);
        }
        assert_eq!(s.backlog.len(), n - 2 * FEED_CHUNK);
        s.outstanding.pop_front();
        feed(&mut s, false);
        assert_eq!(s.backlog.len(), n - 3 * FEED_CHUNK);
        // A global stop drops the unsent tail from `outstanding` (it never
        // ran and never will) so the session can end.
        feed(&mut s, true);
        assert!(s.backlog.is_empty());
        assert_eq!(s.outstanding.len(), 2 * FEED_CHUNK);
        assert!(!s.release_after_backlog);
        s.worker.reap();
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn a_stopped_worker_is_never_dispatched_to_and_gives_its_work_back() {
        // A worker whose session stopped (Stopped) left its run loop: a refill
        // sent to it would be lost, and whatever it still held must go back
        // to the queue for the survivors.
        let (mut s, script) = silent_worker("stopped");
        let names: Vec<String> = (0..4).map(|i| format!("t.py::t{i}")).collect();
        let mut d = super::super::dispatch::build_dispatch(
            &names,
            vec![],
            Default::default(),
            &Default::default(),
            &Default::default(),
            super::super::Dist::Load,
            super::super::Order::Throughput,
            None,
            None,
        )
        .unwrap();
        dispatch_to(&mut s, &mut d, 2, false).unwrap();
        assert_eq!(s.outstanding, [0, 1]);
        s.stopped = true;
        dispatch_to(&mut s, &mut d, 2, false).unwrap();
        assert_eq!(s.outstanding, [0, 1], "refilled a stopped worker");
        assert_eq!(d.cursor, 2, "took items off the queue for a stopped worker");
        assert_eq!(super::super::reclaim(&mut s), vec![0, 1]);
        assert!(s.outstanding.is_empty());
        s.worker.reap();
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn reclaim_drops_the_unsent_backlog_with_the_rest() {
        let (mut s, script) = silent_worker("reclaim");
        seed_list(&mut s, (0..(3 * FEED_CHUNK) as u64).collect());
        assert!(!s.backlog.is_empty());
        let back = super::super::reclaim(&mut s);
        assert_eq!(
            back.len(),
            3 * FEED_CHUNK,
            "every assigned item comes back once"
        );
        assert!(s.backlog.is_empty() && s.outstanding.is_empty());
        assert!(!s.release_after_backlog);
        s.worker.reap();
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn seed_list_releases_once_the_backlog_is_sent() {
        let (mut s, script) = silent_worker("release");
        seed_list(&mut s, (0..10).collect());
        assert!(s.backlog.is_empty());
        assert_eq!(s.outstanding.len(), 10);
        assert!(s.finishing, "a fully sent list releases the held item");
        s.worker.reap();
        let _ = std::fs::remove_file(script);
    }
}
