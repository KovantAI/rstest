//! `--collect-only --report-json`: a single collect-only session written out as
//! a structured discovery doc (nodeid + abs file + line + markers), the
//! machine-readable surface editors/CI consume.

use anyhow::Result;
use serde::Serialize;

use crate::config;
use crate::scheduling::{proto, worker};
use crate::text::strip_verbatim;

/// The `--collect-only --report-json` discovery document (schema 1). Field
/// order is alphabetical to match the historical `serde_json::Map` output
/// (serde_json has no `preserve_order` here), so the emitted bytes are
/// unchanged by the move to a typed struct.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DiscoveryDoc {
    /// Per-collector import/collection errors (empty on a clean collection).
    pub collect_errors: Vec<CollectError>,
    pub meta: DiscoveryMeta,
    /// One entry per collected test item, in collection order.
    pub tests: Vec<DiscoveredTest>,
}

/// Envelope metadata for the discovery document.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DiscoveryMeta {
    /// Number of collected test items (`tests.len()`).
    pub count: usize,
    /// Constant discriminator: always `"discovery"`.
    pub kind: String,
    /// Absolute project root; `file` paths are resolved against it.
    pub rootdir: String,
    /// Constant producer tag: always `"rstest"`.
    pub runner: String,
    /// Document schema version.
    pub schema: u32,
}

/// One discovered test item.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DiscoveredTest {
    /// Absolute source file, or empty when pytest reported no location.
    pub file: String,
    /// 0-based definition line, or null when pytest reported none.
    pub lineno: Option<u64>,
    /// All pytest marker names on the item (own + inherited).
    pub markers: Vec<String>,
    /// The pytest node id.
    pub nodeid: String,
}

/// A collection-time error (one collector that failed to import/collect).
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CollectError {
    /// The failure text (traceback / repr).
    pub longrepr: String,
    /// The path pytest was collecting when it failed.
    pub path: String,
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
        debug_port: None,
        stream_output: false,
        junitxml: None,
    };
    let mut w = worker::Worker::spawn_with_io(python, None, worker::Stdio::Null, &env)?;
    // Item-dispatch session: its `pytest_collection_finish` emits the
    // id+location payload (runtestloop returns early on --collect-only). The
    // plain run_tests session has no collection_finish, so can't feed discovery.
    w.send(&proto::Command::RunItemsSession {
        args: args.to_vec(),
    })?;

    let mut acc = Collected::default();
    let exitstatus = loop {
        if let Some(code) = fold_collect_event(w.recv()?, &mut acc) {
            break code;
        }
    };
    w.shutdown()?;

    let rootdir = discovery_rootdir(acc.rootdir.as_deref(), &std::env::current_dir()?);
    let tests = build_tests(&acc.ids, &acc.locations, &acc.marks, &rootdir);
    let doc = discovery_doc(tests, &acc.collect_errors, &rootdir);
    std::fs::write(out, serde_json::to_vec_pretty(&doc)?)?;
    Ok(exitstatus)
}

/// Assemble the discovery envelope from the built tests and the
/// `(path, longrepr)` collect errors.
fn discovery_doc(
    tests: Vec<DiscoveredTest>,
    collect_errors: &[(String, String)],
    rootdir: &std::path::Path,
) -> DiscoveryDoc {
    DiscoveryDoc {
        collect_errors: collect_errors
            .iter()
            .map(|(p, l)| CollectError {
                longrepr: l.clone(),
                path: p.clone(),
            })
            .collect(),
        meta: DiscoveryMeta {
            count: tests.len(),
            kind: "discovery".to_string(),
            rootdir: rootdir.to_string_lossy().into_owned(),
            runner: "rstest".to_string(),
            schema: 1,
        },
        tests,
    }
}

/// What a collect-only session reported, accumulated by [`fold_collect_event`].
#[derive(Default)]
struct Collected {
    ids: Vec<String>,
    /// (rootdir-relative file, 0-based lineno), aligned to `ids`.
    locations: Vec<(String, Option<u64>)>,
    marks: Vec<Vec<String>>,
    collect_errors: Vec<(String, String)>,
    /// pytest's `config.rootpath`: what `locations` are relative to.
    rootdir: Option<String>,
}

/// Fold one collect-session event into the discovery accumulator. Returns
/// `Some(exitstatus)` on `Done` (loop terminator); `None` otherwise. The
/// designated worker's `CollectionDone` carries the id+location+marker payload
/// and pytest's rootdir; per-collector `CollectError`s accumulate; every other
/// event is a no-op here (the collect-only session emits no run events).
fn fold_collect_event(event: proto::Event, acc: &mut Collected) -> Option<i32> {
    match event {
        proto::Event::CollectionDone {
            ids,
            locations,
            marks,
            rootdir,
            ..
        } => {
            if let Some(i) = ids {
                acc.ids = i;
            }
            if let Some(l) = locations {
                acc.locations = l;
            }
            if let Some(m) = marks {
                acc.marks = m;
            }
            if rootdir.is_some() {
                acc.rootdir = rootdir;
            }
            None
        }
        proto::Event::CollectError { path, longrepr } => {
            acc.collect_errors.push((path, longrepr));
            None
        }
        proto::Event::Done { exitstatus } => Some(exitstatus),
        _ => None,
    }
}

/// The absolute, canonical rootdir `file` paths are joined onto, so each one is
/// an editor-usable URI. pytest's own rootdir (`reported`) is what item
/// locations are relative to; it can differ from what rstest's config discovery
/// finds from `cwd` (a project marked only by `setup.py`, run from `tests/`).
/// The discovered one is only a fallback for a session that never reported.
fn discovery_rootdir(reported: Option<&str>, cwd: &std::path::Path) -> std::path::PathBuf {
    let rootdir = match reported {
        Some(r) => std::path::PathBuf::from(r),
        None => config::discover(cwd, &mut std::io::stderr()).rootdir,
    };
    let rootdir = if rootdir.is_absolute() {
        rootdir
    } else {
        cwd.join(rootdir)
    };
    strip_verbatim(std::fs::canonicalize(&rootdir).unwrap_or(rootdir))
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
) -> Vec<DiscoveredTest> {
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
            DiscoveredTest {
                file,
                lineno,
                markers,
                nodeid: nodeid.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        build_tests, discovery_doc, discovery_rootdir, fold_collect_event, strip_verbatim,
        Collected,
    };
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
        rootdir: Option<String>,
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
            rootdir,
            args_source: None,
            root_args: None,
            inifile: None,
            order_flags: None,
            confcutdir: None,
            maxfail: None,
        }
    }

    #[test]
    fn fold_collect_event_accumulates_payload_errors_and_ignores_rest() {
        let mut acc = Collected::default();

        // CollectionDone loads the designated worker's id+location+marker payload.
        assert_eq!(
            fold_collect_event(
                collection_done(
                    Some(vec!["t.py::a".into()]),
                    Some(vec![("t.py".into(), Some(3))]),
                    Some(vec![vec!["slow".into()]]),
                    Some("/repo".into()),
                ),
                &mut acc,
            ),
            None
        );
        assert_eq!(acc.ids, vec!["t.py::a".to_string()]);
        assert_eq!(acc.locations, vec![("t.py".to_string(), Some(3))]);
        assert_eq!(acc.marks, vec![vec!["slow".to_string()]]);
        assert_eq!(acc.rootdir.as_deref(), Some("/repo"));

        // An all-None CollectionDone (older worker sent no payload) leaves the
        // accumulators untouched — exercises the None branches.
        assert_eq!(
            fold_collect_event(collection_done(None, None, None, None), &mut acc),
            None
        );
        assert_eq!(acc.ids, vec!["t.py::a".to_string()]);
        assert_eq!(acc.locations, vec![("t.py".to_string(), Some(3))]);
        assert_eq!(acc.marks, vec![vec!["slow".to_string()]]);
        assert_eq!(acc.rootdir.as_deref(), Some("/repo"));

        // CollectError accumulates (path, longrepr).
        assert_eq!(
            fold_collect_event(
                proto::Event::CollectError {
                    path: "bad.py".into(),
                    longrepr: "boom".into(),
                },
                &mut acc,
            ),
            None
        );
        assert_eq!(
            acc.collect_errors,
            vec![("bad.py".to_string(), "boom".to_string())]
        );

        // A non-discovery event (collect-only emits no run events) is a no-op.
        assert_eq!(
            fold_collect_event(
                proto::Event::ItemStart {
                    index: 0,
                    timeout: None,
                },
                &mut acc,
            ),
            None
        );

        // Done terminates the loop with the exit status.
        assert_eq!(
            fold_collect_event(proto::Event::Done { exitstatus: 5 }, &mut acc),
            Some(5)
        );
    }

    #[test]
    fn discovery_rootdir_prefers_pytests_rootdir_over_cwd_discovery() {
        // B12: a project marked only by `setup.py`, run from `tests/`. rstest's
        // config discovery stops at the cwd, but pytest's rootdir is the parent,
        // and item locations (`tests/test_x.py`) are relative to that.
        let base = std::env::temp_dir().join(format!("rstest-disc-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("tests")).unwrap();
        std::fs::write(base.join("setup.py"), "").unwrap();
        std::fs::write(base.join("tests/test_x.py"), "def test_x(): pass").unwrap();
        let root = strip_verbatim(base.canonicalize().unwrap());

        let got = discovery_rootdir(Some(&base.to_string_lossy()), &base.join("tests"));
        assert_eq!(got, root);
        let tests = build_tests(
            &["tests/test_x.py::test_x".to_string()],
            &[("tests/test_x.py".to_string(), Some(0))],
            &[],
            &got,
        );
        assert!(
            std::path::Path::new(&tests[0].file).is_file(),
            "{}",
            tests[0].file
        );

        // A relative report still comes out absolute (anchored at the cwd).
        assert_eq!(discovery_rootdir(Some("."), &base), root);
        let _ = std::fs::remove_dir_all(&base);
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

        assert_eq!(tests[0].nodeid, "t.py::a");
        // `./` stripped, joined onto the absolute rootdir.
        assert_eq!(
            tests[0].file,
            std::path::Path::new("/repo")
                .join("t.py")
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(tests[0].lineno, Some(10));
        assert_eq!(tests[0].markers[0], "slow");

        // Empty file_rel => empty file string; missing marks row => [].
        assert_eq!(tests[1].file, "");
        assert!(tests[1].lineno.is_none());
        assert!(tests[1].markers.is_empty());
    }

    #[test]
    fn discovery_doc_carries_meta_tests_and_collect_errors() {
        let rootdir = std::path::Path::new("/repo");
        let ids = vec!["t.py::a".to_string()];
        let tests = build_tests(&ids, &[("t.py".to_string(), Some(3))], &[], rootdir);
        let errors = vec![("bad.py".to_string(), "ImportError: nope".to_string())];

        let doc = serde_json::to_value(discovery_doc(tests, &errors, rootdir)).unwrap();
        assert_eq!(doc["meta"]["kind"], "discovery");
        assert_eq!(doc["meta"]["runner"], "rstest");
        assert_eq!(doc["meta"]["schema"], 1);
        assert_eq!(doc["meta"]["count"], 1);
        assert_eq!(doc["meta"]["rootdir"], rootdir.to_string_lossy().as_ref());
        assert_eq!(doc["tests"][0]["nodeid"], "t.py::a");
        assert_eq!(
            doc["collect_errors"],
            serde_json::json!([{ "path": "bad.py", "longrepr": "ImportError: nope" }])
        );
    }
}
