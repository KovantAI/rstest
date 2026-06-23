//! Orchestrator<->worker protocol: msgpack values over dedicated pipes
//! (never stdio — workers must keep fd 0/1/2 free for capture; see
//! research D4 / xdist execnet fd-steal lesson).
//!
//! Wire shape: every message is a msgpack map `{"kind": ..., "payload": ...}`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum Command {
    /// One self-contained pytest session over `args` (single-worker mode).
    RunTests {
        args: Vec<String>,
    },
    /// Item-dispatch mode: collect per `args`, then await RunItems batches.
    /// Every pool worker receives IDENTICAL args — this preserves pytest's
    /// config/conftest semantics exactly (file-granular args do not; see
    /// pandas pytables importorskip incident).
    RunItemsSession {
        args: Vec<String>,
    },
    /// Append collected-item indices to the worker's pending queue.
    RunItems {
        indices: Vec<u64>,
    },
    /// Lazy-collection mode: session over `args` with NO initial
    /// collection; work arrives as RunFiles/RunIds (D5 single-point
    /// collection — each file is collected by exactly one worker).
    RunLazySession {
        args: Vec<String>,
    },
    /// Lazy mode: collect these files on demand and run their items.
    RunFiles {
        paths: Vec<String>,
    },
    /// Lazy mode: (re-)run items by nodeid — reruns, crash
    /// redistribution, and the serial phase. The worker re-collects a
    /// nodeid it has never seen.
    RunIds {
        ids: Vec<String>,
    },
    /// The queue is exhausted FOR NOW: drain pending (last item runs with
    /// nextitem=None, releasing fixture finalizers), then keep listening —
    /// failed items elsewhere may rerun here (--reruns).
    NoMoreItems,
    /// Run pytest_testnodedown for a CRASHED worker: `workerinput` is
    /// the dead worker's snapshot (shipped via NodeInput while it was
    /// alive), so cleanup hooks see the exact idents it provisioned.
    NodeDown {
        workerinput: serde_json::Value,
        error: String,
    },
    /// Every item's outcome is final: finish the session (Done follows).
    EndSession,
    Shutdown,
}

/// xdist-shaped per-phase test report (subset; grows toward the full
/// `_report_to_json` schema as the vendored core lands).
#[derive(Debug, Deserialize)]
pub struct Report {
    pub nodeid: String,
    pub when: String,
    pub outcome: String,
    pub duration: f64,
    pub longrepr: Option<String>,
    #[serde(default)]
    pub wasxfail: bool,
    #[serde(default)]
    pub skip_reason: Option<String>,
    /// Doctor mode: call-phase CPU time (process_time). wall >> cpu means
    /// the test was waiting, not computing.
    #[serde(default)]
    pub cpu: Option<f64>,
    /// Captured stdout/stderr/log sections, present on failed reports only.
    #[serde(default)]
    pub sections: Vec<(String, String)>,
    /// Source line of the test (0-based, from pytest's report.location);
    /// None when pytest reports no location. Used for editor mapping.
    #[serde(default)]
    pub lineno: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WarningEntry {
    /// pytest phase: "config" / "collect" / "runtest" — config+collect
    /// warnings repeat in every worker session and must be counted once.
    pub when: String,
    pub category: String,
    pub message: String,
    pub filename: String,
    pub lineno: u64,
    pub count: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct FixtureStat {
    pub name: String,
    pub scope: String,
    pub count: u64,
    pub total: f64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum Event {
    Report(Report),
    CollectError {
        path: String,
        longrepr: String,
    },
    /// A collector (module/dir) was skipped wholesale (importorskip,
    /// skip marks at module level). pytest counts these in "skipped".
    CollectSkip {
        #[allow(dead_code)]
        path: String,
    },
    /// Doctor mode: per-fixture setup timing, sent at session finish.
    DoctorFixtures {
        fixtures: Vec<FixtureStat>,
    },
    /// Aggregated warnings, sent at session finish.
    Warnings {
        entries: Vec<WarningEntry>,
    },
    /// Item-dispatch mode: collection finished. Workers verify by
    /// count+hash; `ids` (session order) ride along from one designated
    /// worker only — the orchestrator needs the actual list once, for
    /// duration-cache ordering (D5: no 8x15MB id transfer at pandas scale).
    CollectionDone {
        count: u64,
        hash: String,
        #[serde(default)]
        ids: Option<Vec<String>>,
        /// Source location per item (rootdir-relative file, 0-based lineno),
        /// aligned to `ids`; rides with `ids`. For --collect-only discovery
        /// and editor mapping. lineno is None when pytest reports none.
        #[serde(default)]
        locations: Option<Vec<(String, Option<u64>)>>,
        /// All marker names per item (own + inherited), aligned to `ids`.
        /// For --collect-only discovery; serial/flaky/groups below stay
        /// separate because the scheduler keys on them.
        #[serde(default)]
        marks: Option<Vec<Vec<String>>>,
        /// Indices of @pytest.mark.serial items (rides with `ids`, from the
        /// designated worker only). These run exclusively, after the
        /// parallel phase.
        #[serde(default)]
        serial: Option<Vec<u64>>,
        /// pytest's cache directory (rides with `ids`): the orchestrator
        /// writes the merged lastfailed cache there after the run.
        #[serde(default)]
        cache_dir: Option<String>,
        /// @pytest.mark.flaky(reruns=N) per-item budgets (index -> N),
        /// keys stringified for msgpack-map friendliness.
        #[serde(default)]
        flaky: Option<std::collections::HashMap<String, u32>>,
        /// @pytest.mark.xdist_group names (index -> group), for
        /// --dist loadgroup affinity.
        #[serde(default)]
        groups: Option<std::collections::HashMap<String, String>>,
    },
    /// Lazy mode: session configured, ready for RunFiles. `cache_dir`
    /// rides from every worker; the orchestrator keeps the first.
    LazyReady {
        #[serde(default)]
        cache_dir: Option<String>,
    },
    /// Lazy mode: one file collected (by exactly one worker). `ids` in
    /// collection order; serial/flaky ride along, keyed by nodeid.
    FileCollected {
        #[allow(dead_code)]
        path: String,
        ids: Vec<String>,
        #[serde(default)]
        serial: Vec<String>,
        #[serde(default)]
        flaky: std::collections::HashMap<String, u32>,
    },
    /// Lazy-mode twins of ItemStart/ItemDone, keyed by nodeid (lazy
    /// workers share no index space).
    ItemStartId {
        id: String,
    },
    ItemDoneId {
        id: String,
    },
    /// Lazy-mode twin of Stopped: unrun nodeids.
    StoppedIds {
        unrun: Vec<String>,
    },
    /// Snapshot of the worker's workerinput after configure_node hooks
    /// ran (msgpack-serializable subset). Held by the orchestrator so a
    /// crash can still fire pytest_testnodedown with the right idents.
    NodeInput {
        workerinput: serde_json::Value,
    },
    /// The worker is about to run item `index`. If the worker process dies
    /// before the matching ItemDone, this is the item that killed it.
    ItemStart {
        index: u64,
    },
    /// Item at `index` finished its full runtest protocol (scheduling
    /// signal — distinct from its phase Reports, per xdist lesson).
    ItemDone {
        index: u64,
    },
    /// Session-local -x/--maxfail tripped: the worker stopped early.
    /// `unrun` = pending indices it never ran.
    Stopped {
        unrun: Vec<u64>,
    },
    Done {
        exitstatus: i32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Commands are sent with to_vec_named (worker.rs); the worker matches
    /// on the literal "kind" string. These names ARE the wire protocol —
    /// renaming a variant breaks every worker silently.
    fn kind_of(cmd: &Command) -> String {
        let bytes = rmp_serde::encode::to_vec_named(cmd).unwrap();
        let value: serde_json::Value = rmp_serde::from_slice(&bytes).unwrap();
        value["kind"].as_str().unwrap().to_string()
    }

    #[test]
    fn command_kind_strings_are_stable() {
        assert_eq!(kind_of(&Command::RunTests { args: vec![] }), "run_tests");
        assert_eq!(
            kind_of(&Command::RunItemsSession { args: vec![] }),
            "run_items_session"
        );
        assert_eq!(
            kind_of(&Command::RunItems { indices: vec![1] }),
            "run_items"
        );
        assert_eq!(kind_of(&Command::NoMoreItems), "no_more_items");
        assert_eq!(kind_of(&Command::EndSession), "end_session");
        assert_eq!(kind_of(&Command::Shutdown), "shutdown");
    }

    /// Events arrive as Python msgpack maps {"kind": ..., "payload": ...}.
    /// Build byte-identical frames and decode them like the reader does.
    fn from_python(value: serde_json::Value) -> Event {
        let bytes = rmp_serde::encode::to_vec_named(&value).unwrap();
        rmp_serde::from_slice(&bytes).unwrap()
    }

    #[test]
    fn event_report_decodes_with_defaults() {
        let e = from_python(serde_json::json!({
            "kind": "report",
            "payload": {
                "nodeid": "a.py::t",
                "when": "call",
                "outcome": "passed",
                "duration": 0.5,
                "longrepr": null,
            }
        }));
        match e {
            Event::Report(r) => {
                assert_eq!(r.nodeid, "a.py::t");
                assert!(!r.wasxfail); // optional fields default
                assert!(r.sections.is_empty());
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn event_collection_done_minimal_and_full() {
        // Non-designate workers send only count+hash.
        let e = from_python(serde_json::json!({
            "kind": "collection_done",
            "payload": {"count": 3, "hash": "abc"}
        }));
        match e {
            Event::CollectionDone {
                count,
                ids,
                serial,
                flaky,
                groups,
                ..
            } => {
                assert_eq!(count, 3);
                assert!(ids.is_none() && serial.is_none());
                assert!(flaky.is_none() && groups.is_none());
            }
            other => panic!("wrong event: {other:?}"),
        }
        // The designate ships the full payload.
        let e = from_python(serde_json::json!({
            "kind": "collection_done",
            "payload": {
                "count": 2, "hash": "h", "ids": ["a", "b"], "serial": [1],
                "cache_dir": "/tmp/c", "flaky": {"0": 2}, "groups": {"1": "g"},
            }
        }));
        match e {
            Event::CollectionDone {
                ids, serial, flaky, ..
            } => {
                assert_eq!(ids.unwrap().len(), 2);
                assert_eq!(serial.unwrap(), vec![1]);
                assert_eq!(flaky.unwrap()["0"], 2);
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn event_lifecycle_kinds() {
        assert!(matches!(
            from_python(serde_json::json!({"kind": "item_start", "payload": {"index": 7}})),
            Event::ItemStart { index: 7 }
        ));
        assert!(matches!(
            from_python(serde_json::json!({"kind": "item_done", "payload": {"index": 7}})),
            Event::ItemDone { index: 7 }
        ));
        assert!(matches!(
            from_python(serde_json::json!({"kind": "done", "payload": {"exitstatus": 5}})),
            Event::Done { exitstatus: 5 }
        ));
        assert!(matches!(
            from_python(serde_json::json!({"kind": "stopped", "payload": {"unrun": [1, 2]}})),
            Event::Stopped { .. }
        ));
    }
}
