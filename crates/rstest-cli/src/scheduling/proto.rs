//! Orchestrator<->worker protocol: msgpack values over dedicated pipes
//! (never stdio - workers must keep fd 0/1/2 free for capture; see
//! research D4 / xdist execnet fd-steal lesson).
//!
//! Wire shape: every message is a msgpack map `{"kind": ..., "payload": ...}`.
//!
//! This enum is the source of truth for the protocol. The Python worker mirrors
//! it as TypedDicts in `python/rstest_worker/_internal/messages.py`; the parity
//! test `python/tests/test_protocol_parity.py` fails if the two `kind` sets
//! diverge, so add/rename a variant on both sides together.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Clone))]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum Command {
    /// One self-contained pytest session over `args` (single-worker mode).
    RunTests {
        args: Vec<String>,
    },
    /// Item-dispatch mode: collect per `args`, then await RunItems batches.
    /// Every pool worker gets IDENTICAL args to preserve pytest config/conftest
    /// semantics (file-granular args do not; see pandas pytables importorskip).
    RunItemsSession {
        args: Vec<String>,
    },
    /// Append collected-item indices to the worker's pending queue.
    RunItems {
        indices: Vec<u64>,
    },
    /// Lazy-collection mode: session over `args` with NO initial
    /// collection; work arrives as RunFiles/RunIds (D5 single-point
    /// collection - each file is collected by exactly one worker).
    RunLazySession {
        args: Vec<String>,
    },
    /// Lazy mode: collect these files on demand and run their items.
    RunFiles {
        paths: Vec<String>,
    },
    /// Lazy mode: (re-)run items by nodeid - reruns, crash
    /// redistribution, and the serial phase. The worker re-collects a
    /// nodeid it has never seen.
    RunIds {
        ids: Vec<String>,
    },
    /// The queue is exhausted FOR NOW: drain pending (last item runs with
    /// nextitem=None, releasing fixture finalizers), then keep listening -
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
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(test, derive(PartialEq))]
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
    /// Leak check: net Python threads after teardown vs before setup (on the
    /// teardown report; positive = a thread the test never joined).
    #[serde(default)]
    pub thread_delta: Option<i64>,
    /// Leak check: net open file descriptors after teardown vs before setup.
    #[serde(default)]
    pub fd_delta: Option<i64>,
    /// Captured stdout/stderr/log sections, present on failed reports only.
    #[serde(default)]
    pub sections: Vec<(String, String)>,
    /// Source line of the test (0-based, from pytest's report.location);
    /// None when pytest reports no location. Used for editor mapping.
    #[serde(default)]
    pub lineno: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[cfg_attr(test, derive(PartialEq, Serialize))]
pub struct WarningEntry {
    /// pytest phase: "config" / "collect" / "runtest" - config+collect
    /// warnings repeat in every worker session and must be counted once.
    pub when: String,
    pub category: String,
    pub message: String,
    pub filename: String,
    pub lineno: u64,
    pub count: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[cfg_attr(test, derive(PartialEq, Serialize))]
pub struct FixtureStat {
    pub name: String,
    pub scope: String,
    pub count: u64,
    pub total: f64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Serialize))]
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
    /// Item-dispatch mode: collection finished. Workers verify by count+hash;
    /// `ids` (session order) ride from one designated worker only - the
    /// orchestrator needs the list once, for duration-cache ordering (D5).
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
    /// signal - distinct from its phase Reports, per xdist lesson).
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
    /// on the literal "kind" string. These names ARE the wire protocol -
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

/// Property tests: the invariants that must hold across ALL wire values, not
/// just the hand-picked ones above. Roundtrip guards encode/decode symmetry;
/// the arbitrary-bytes test is a fuzz-lite crash check that runs in CI; the
/// command-kind test guards the wire tag set the Python worker matches on.
#[cfg(test)]
mod property {
    use super::*;
    use proptest::prelude::*;

    /// Finite only: NaN would break the roundtrip PartialEq (NaN != NaN),
    /// and the protocol never carries non-finite durations anyway.
    fn finite_f64() -> impl Strategy<Value = f64> {
        -1.0e9f64..1.0e9f64
    }

    /// Small arbitrary strings (proptest reads the literal as a regex).
    fn small_str() -> impl Strategy<Value = String> {
        ".{0,16}"
    }

    fn small_strs() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(small_str(), 0..4)
    }

    prop_compose! {
        fn arb_report()(
            nodeid in small_str(),
            when in small_str(),
            outcome in small_str(),
            duration in finite_f64(),
            longrepr in prop::option::of(small_str()),
            wasxfail in any::<bool>(),
            skip_reason in prop::option::of(small_str()),
            cpu in prop::option::of(finite_f64()),
            thread_delta in prop::option::of(any::<i64>()),
            fd_delta in prop::option::of(any::<i64>()),
            sections in prop::collection::vec((small_str(), small_str()), 0..4),
            lineno in prop::option::of(any::<u64>()),
        ) -> Report {
            Report {
                nodeid, when, outcome, duration, longrepr, wasxfail,
                skip_reason, cpu, thread_delta, fd_delta, sections, lineno,
            }
        }
    }

    prop_compose! {
        fn arb_fixture()(
            name in small_str(), scope in small_str(),
            count in any::<u64>(), total in finite_f64(),
        ) -> FixtureStat {
            FixtureStat { name, scope, count, total }
        }
    }

    prop_compose! {
        fn arb_warning()(
            when in small_str(), category in small_str(), message in small_str(),
            filename in small_str(), lineno in any::<u64>(), count in any::<u64>(),
        ) -> WarningEntry {
            WarningEntry { when, category, message, filename, lineno, count }
        }
    }

    prop_compose! {
        fn arb_collection_done()(
            count in any::<u64>(),
            hash in small_str(),
            ids in prop::option::of(small_strs()),
            locations in prop::option::of(prop::collection::vec(
                (small_str(), prop::option::of(any::<u64>())), 0..4)),
            marks in prop::option::of(prop::collection::vec(small_strs(), 0..4)),
            serial in prop::option::of(prop::collection::vec(any::<u64>(), 0..4)),
            cache_dir in prop::option::of(small_str()),
            flaky in prop::option::of(prop::collection::hash_map(small_str(), any::<u32>(), 0..4)),
            groups in prop::option::of(prop::collection::hash_map(small_str(), small_str(), 0..4)),
        ) -> Event {
            Event::CollectionDone {
                count, hash, ids, locations, marks, serial, cache_dir, flaky, groups,
            }
        }
    }

    prop_compose! {
        fn arb_file_collected()(
            path in small_str(),
            ids in small_strs(),
            serial in small_strs(),
            flaky in prop::collection::hash_map(small_str(), any::<u32>(), 0..4),
        ) -> Event {
            Event::FileCollected { path, ids, serial, flaky }
        }
    }

    /// A representative Event value. Skips NodeInput/NodeDown: their
    /// `serde_json::Value` payloads roundtrip through msgpack with ambiguous
    /// integer typing, which is a Value quirk, not a protocol invariant.
    fn arb_event() -> impl Strategy<Value = Event> {
        let group_a = prop_oneof![
            arb_report().prop_map(Event::Report),
            (small_str(), small_str())
                .prop_map(|(path, longrepr)| Event::CollectError { path, longrepr }),
            small_str().prop_map(|path| Event::CollectSkip { path }),
            prop::collection::vec(arb_fixture(), 0..4)
                .prop_map(|fixtures| Event::DoctorFixtures { fixtures }),
            prop::collection::vec(arb_warning(), 0..4)
                .prop_map(|entries| Event::Warnings { entries }),
            arb_collection_done(),
            prop::option::of(small_str()).prop_map(|cache_dir| Event::LazyReady { cache_dir }),
            arb_file_collected(),
        ];
        let group_b = prop_oneof![
            small_str().prop_map(|id| Event::ItemStartId { id }),
            small_str().prop_map(|id| Event::ItemDoneId { id }),
            small_strs().prop_map(|unrun| Event::StoppedIds { unrun }),
            any::<u64>().prop_map(|index| Event::ItemStart { index }),
            any::<u64>().prop_map(|index| Event::ItemDone { index }),
            prop::collection::vec(any::<u64>(), 0..4).prop_map(|unrun| Event::Stopped { unrun }),
            any::<i32>().prop_map(|exitstatus| Event::Done { exitstatus }),
        ];
        prop_oneof![group_a, group_b]
    }

    const KNOWN_COMMAND_KINDS: &[&str] = &[
        "run_tests",
        "run_items_session",
        "run_items",
        "run_lazy_session",
        "run_files",
        "run_ids",
        "no_more_items",
        "node_down",
        "end_session",
        "shutdown",
    ];

    fn arb_command() -> impl Strategy<Value = Command> {
        prop_oneof![
            small_strs().prop_map(|args| Command::RunTests { args }),
            small_strs().prop_map(|args| Command::RunItemsSession { args }),
            prop::collection::vec(any::<u64>(), 0..4)
                .prop_map(|indices| Command::RunItems { indices }),
            small_strs().prop_map(|args| Command::RunLazySession { args }),
            small_strs().prop_map(|paths| Command::RunFiles { paths }),
            small_strs().prop_map(|ids| Command::RunIds { ids }),
            Just(Command::NoMoreItems),
            small_str().prop_map(|error| Command::NodeDown {
                workerinput: serde_json::Value::Null,
                error,
            }),
            Just(Command::EndSession),
            Just(Command::Shutdown),
        ]
    }

    proptest! {
        /// Encode -> decode is the identity for every event the workers emit.
        #[test]
        fn event_roundtrips_through_msgpack(e in arb_event()) {
            let bytes = rmp_serde::encode::to_vec_named(&e).unwrap();
            let back: Event = rmp_serde::from_slice(&bytes)
                .expect("re-decoding our own encoding must succeed");
            prop_assert_eq!(e, back);
        }

        /// Hostile/garbage bytes must decode to Ok or Err - never panic, never
        /// hang. This is the same surface the fuzz target explores, kept in the
        /// unit suite so regressions surface without the fuzzing toolchain.
        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..1024)) {
            let _ = rmp_serde::from_slice::<Event>(&bytes);
        }

        /// The `kind` tag is the wire contract the Python worker matches on;
        /// every command must serialize to one of the known strings.
        #[test]
        fn command_kind_is_always_known(c in arb_command()) {
            let bytes = rmp_serde::encode::to_vec_named(&c).unwrap();
            let v: serde_json::Value = rmp_serde::from_slice(&bytes).unwrap();
            let kind = v["kind"].as_str().expect("kind must be a string");
            prop_assert!(KNOWN_COMMAND_KINDS.contains(&kind), "unknown kind: {kind}");
        }
    }
}
