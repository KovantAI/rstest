//! `--collect-only --report-json`: a single collect-only session written out as
//! a structured discovery doc (nodeid + abs file + line + markers), the
//! machine-readable surface editors/CI consume.

use anyhow::Result;

use crate::config;
use crate::scheduling::{proto, worker};

fn strip_verbatim(p: std::path::PathBuf) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        std::path::PathBuf::from(rest.to_string())
    } else {
        p
    }
}

/// Run a single collect-only session and write a structured discovery doc
/// (meta, tests, collect_errors). Bypasses passthrough so collection rides
/// the wire (RSTEST_SEND_IDS), not pytest's text tree. Returns exit status.
pub(super) fn run_collect_discovery(
    python: &std::path::Path,
    args: &[String],
    out: &std::path::Path,
    run_uid: &str,
) -> Result<i32> {
    // The lone worker ships the full id+location payload from
    // `pytest_collection_finish` (single session, so no per-worker designate).
    let env = worker::WorkerEnv {
        run_uid: run_uid.to_string(),
        doctor: false,
        timeout: None,
        leakcheck: false,
        send_ids: true,
    };
    let mut w = worker::Worker::spawn_with_io(python, None, worker::Stdio::Null, &env)?;
    // Item-dispatch session: its `pytest_collection_finish` emits the
    // id+location payload (runtestloop returns early on --collect-only). The
    // plain run_tests session has no collection_finish, so can't feed discovery.
    w.send(&proto::Command::RunItemsSession {
        args: args.to_vec(),
    })?;

    let mut ids: Vec<String> = Vec::new();
    let mut locations: Vec<(String, Option<u64>)> = Vec::new();
    let mut marks: Vec<Vec<String>> = Vec::new();
    let mut collect_errors: Vec<(String, String)> = Vec::new();
    let exitstatus = loop {
        if let Some(code) = fold_collect_event(
            w.recv()?,
            &mut ids,
            &mut locations,
            &mut marks,
            &mut collect_errors,
        ) {
            break code;
        }
    };
    w.shutdown()?;

    // Absolute rootdir so `file` resolves to an editor-usable URI.
    let cwd = std::env::current_dir()?;
    let rootdir = config::discover(&cwd).rootdir;
    let rootdir = if rootdir.is_absolute() {
        rootdir
    } else {
        cwd.join(rootdir)
    };
    let rootdir = strip_verbatim(std::fs::canonicalize(&rootdir).unwrap_or(rootdir));
    let tests = build_tests(&ids, &locations, &marks, &rootdir);
    let doc = serde_json::json!({
        "meta": {
            "runner": "rstest",
            "kind": "discovery",
            "schema": 1,
            "count": ids.len(),
            "rootdir": rootdir.to_string_lossy(),
        },
        "tests": tests,
        "collect_errors": collect_errors
            .iter()
            .map(|(p, l)| serde_json::json!({"path": p, "longrepr": l}))
            .collect::<Vec<_>>(),
    });
    std::fs::write(out, serde_json::to_vec_pretty(&doc)?)?;
    Ok(exitstatus)
}

/// Fold one collect-session event into the discovery accumulators. Returns
/// `Some(exitstatus)` on `Done` (loop terminator); `None` otherwise. The
/// designated worker's `CollectionDone` carries the id+location+marker payload;
/// per-collector `CollectError`s accumulate; every other event is a no-op here
/// (the collect-only session emits no run events).
fn fold_collect_event(
    event: proto::Event,
    ids: &mut Vec<String>,
    locations: &mut Vec<(String, Option<u64>)>,
    marks: &mut Vec<Vec<String>>,
    collect_errors: &mut Vec<(String, String)>,
) -> Option<i32> {
    match event {
        proto::Event::CollectionDone {
            ids: i,
            locations: l,
            marks: m,
            ..
        } => {
            if let Some(i) = i {
                *ids = i;
            }
            if let Some(l) = l {
                *locations = l;
            }
            if let Some(m) = m {
                *marks = m;
            }
            None
        }
        proto::Event::CollectError { path, longrepr } => {
            collect_errors.push((path, longrepr));
            None
        }
        proto::Event::Done { exitstatus } => Some(exitstatus),
        _ => None,
    }
}

/// Build the per-test discovery docs (nodeid + absolute file + lineno + markers)
/// aligned to `ids`. An empty `file_rel` means pytest reported no location, so
/// `file` stays empty; otherwise the rootdir-relative path (with a leading `./`
/// stripped) is joined onto the absolute rootdir for an editor-usable URI.
fn build_tests(
    ids: &[String],
    locations: &[(String, Option<u64>)],
    marks: &[Vec<String>],
    rootdir: &std::path::Path,
) -> Vec<serde_json::Value> {
    ids.iter()
        .enumerate()
        .map(|(i, nodeid)| {
            let (file_rel, lineno) = locations.get(i).cloned().unwrap_or_default();
            // Absolute path for editor URIs; empty rel means pytest gave none.
            let file = if file_rel.is_empty() {
                String::new()
            } else {
                let rel = file_rel.strip_prefix("./").unwrap_or(&file_rel);
                rootdir.join(rel).to_string_lossy().into_owned()
            };
            // All pytest marker names on the item (own + inherited); empty
            // when the worker is older / sent none.
            let markers = marks.get(i).cloned().unwrap_or_default();
            serde_json::json!({
                "nodeid": nodeid,
                "file": file,
                "lineno": lineno,
                "markers": markers,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{build_tests, fold_collect_event, strip_verbatim};
    use crate::scheduling::proto;

    #[test]
    fn strip_verbatim_removes_windows_extended_prefix() {
        // The `\\?\` extended-length prefix is dropped so paths render as
        // editor-usable URIs; anything else is returned untouched.
        assert_eq!(
            strip_verbatim(r"\\?\C:\foo\bar".into()),
            std::path::PathBuf::from(r"C:\foo\bar")
        );
        assert_eq!(
            strip_verbatim("/home/u/proj".into()),
            std::path::PathBuf::from("/home/u/proj")
        );
        // A `?` that is not the exact prefix must not be stripped.
        assert_eq!(
            strip_verbatim("a/?b".into()),
            std::path::PathBuf::from("a/?b")
        );
    }

    fn collection_done(
        ids: Option<Vec<String>>,
        locations: Option<Vec<(String, Option<u64>)>>,
        marks: Option<Vec<Vec<String>>>,
    ) -> proto::Event {
        proto::Event::CollectionDone {
            count: ids.as_ref().map(|v| v.len() as u64).unwrap_or(0),
            hash: String::new(),
            ids,
            locations,
            marks,
            serial: None,
            cache_dir: None,
            flaky: None,
            groups: None,
        }
    }

    #[test]
    fn fold_collect_event_accumulates_payload_errors_and_ignores_rest() {
        let mut ids = Vec::new();
        let mut locations = Vec::new();
        let mut marks = Vec::new();
        let mut errors = Vec::new();

        // CollectionDone loads the designated worker's id+location+marker payload.
        assert_eq!(
            fold_collect_event(
                collection_done(
                    Some(vec!["t.py::a".into()]),
                    Some(vec![("t.py".into(), Some(3))]),
                    Some(vec![vec!["slow".into()]]),
                ),
                &mut ids,
                &mut locations,
                &mut marks,
                &mut errors,
            ),
            None
        );
        assert_eq!(ids, vec!["t.py::a".to_string()]);
        assert_eq!(locations, vec![("t.py".to_string(), Some(3))]);
        assert_eq!(marks, vec![vec!["slow".to_string()]]);

        // An all-None CollectionDone (older worker sent no payload) leaves the
        // accumulators untouched — exercises the None branches.
        assert_eq!(
            fold_collect_event(
                collection_done(None, None, None),
                &mut ids,
                &mut locations,
                &mut marks,
                &mut errors,
            ),
            None
        );
        assert_eq!(ids, vec!["t.py::a".to_string()]);
        assert_eq!(locations, vec![("t.py".to_string(), Some(3))]);
        assert_eq!(marks, vec![vec!["slow".to_string()]]);

        // CollectError accumulates (path, longrepr).
        assert_eq!(
            fold_collect_event(
                proto::Event::CollectError {
                    path: "bad.py".into(),
                    longrepr: "boom".into(),
                },
                &mut ids,
                &mut locations,
                &mut marks,
                &mut errors,
            ),
            None
        );
        assert_eq!(errors, vec![("bad.py".to_string(), "boom".to_string())]);

        // A non-discovery event (collect-only emits no run events) is a no-op.
        assert_eq!(
            fold_collect_event(
                proto::Event::ItemStart { index: 0 },
                &mut ids,
                &mut locations,
                &mut marks,
                &mut errors,
            ),
            None
        );

        // Done terminates the loop with the exit status.
        assert_eq!(
            fold_collect_event(
                proto::Event::Done { exitstatus: 5 },
                &mut ids,
                &mut locations,
                &mut marks,
                &mut errors,
            ),
            Some(5)
        );
    }

    #[test]
    fn build_tests_maps_locations_markers_and_empty_files() {
        let rootdir = std::path::Path::new("/repo");
        let ids = vec!["t.py::a".to_string(), "t.py::b".to_string()];
        // First has a `./`-prefixed rel path + lineno + markers; second has an
        // empty file_rel (pytest gave no location) and no markers row.
        let locations = vec![("./t.py".to_string(), Some(10)), (String::new(), None)];
        let marks = vec![vec!["slow".to_string()]];

        let tests = build_tests(&ids, &locations, &marks, rootdir);
        assert_eq!(tests.len(), 2);

        assert_eq!(tests[0]["nodeid"], "t.py::a");
        // `./` stripped, joined onto the absolute rootdir.
        assert_eq!(
            tests[0]["file"],
            std::path::Path::new("/repo")
                .join("t.py")
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(tests[0]["lineno"], 10);
        assert_eq!(tests[0]["markers"][0], "slow");

        // Empty file_rel => empty file string; missing marks row => [].
        assert_eq!(tests[1]["file"], "");
        assert!(tests[1]["lineno"].is_null());
        assert_eq!(tests[1]["markers"].as_array().unwrap().len(), 0);
    }
}
