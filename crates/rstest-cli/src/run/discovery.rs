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
        match w.recv()? {
            proto::Event::CollectionDone {
                ids: i,
                locations: l,
                marks: m,
                ..
            } => {
                if let Some(i) = i {
                    ids = i;
                }
                if let Some(l) = l {
                    locations = l;
                }
                if let Some(m) = m {
                    marks = m;
                }
            }
            proto::Event::CollectError { path, longrepr } => collect_errors.push((path, longrepr)),
            proto::Event::Done { exitstatus } => break exitstatus,
            _ => {}
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
    let tests: Vec<serde_json::Value> = ids
        .iter()
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
        .collect();
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

#[cfg(test)]
mod tests {
    use super::strip_verbatim;

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
}
