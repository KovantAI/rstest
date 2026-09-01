//! Per-worker bookkeeping the pool event loop mutates: liveness, the
//! outstanding/running item queues, rerun-attempt buffers, and the
//! crash-time workerinput snapshot.

use std::collections::VecDeque;

use crate::scheduling::proto;
use crate::scheduling::worker::Worker;

pub(super) struct WorkerState {
    pub(super) worker: Worker,
    pub(super) collected: bool,
    pub(super) seeded: bool,
    /// Told "queue exhausted for now" (NoMoreItems). Still listening!
    pub(super) finishing: bool,
    /// Told EndSession (no resend).
    pub(super) ended: bool,
    pub(super) dead: bool,
    /// Indices dispatched but not yet item_done'd, in dispatch order.
    pub(super) outstanding: VecDeque<u64>,
    /// The item the worker announced via item_start and hasn't finished.
    pub(super) running: Option<u64>,
    /// When the in-flight item started (hang watchdog).
    pub(super) running_since: Option<std::time::Instant>,
    /// Set when the watchdog killed this worker (better crash message).
    pub(super) timeout_killed: bool,
    /// Reports of the in-flight attempt (only used when reruns are on).
    pub(super) attempt: Vec<proto::Report>,
    pub(super) attempt_failed: bool,
    /// workerinput snapshot (NodeInput) for crash-time testnodedown.
    pub(super) node_input: Option<serde_json::Value>,
}

impl WorkerState {
    pub(super) fn fresh(worker: Worker) -> Self {
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
