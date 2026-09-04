//! `rstest --serve <sock>`: a warm-pool daemon over a Unix socket.
//!
//! A persistent client (fermut, for mutation testing) opens a session once —
//! the worker collects and stays warm — then fires many `run` requests, each a
//! nodeid subset, and gets streamed reports back. Reuses the worker's
//! collect-once + run-subset machinery (`RunServeSession` / `ServeRun`).
//!
//! Wire: the same stream-of-msgpack `{kind, payload}` framing the worker pipe
//! uses (rmp_serde), so no new codec. Protocol per `docs/reference/serve-protocol.md`.
//!
//! Each `run` carries an optional overlay patch (the mutation) and executes in a
//! forked child off the warm template, so a mutation can never leak into the
//! next request. Single client, sequential runs; `cancel`/backpressure/
//! multi-session are future work.

use std::io::{BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cli::Cli;
use crate::discover;
use crate::scheduling::{proto, worker};

/// Run the serve daemon until the client sends `shutdown` (or disconnects).
pub fn serve(cli: &Cli, args: &[String], sock: &Path) -> Result<i32> {
    // A stale socket from a crashed prior run would make bind() fail with
    // EADDRINUSE; clear it (best-effort) before binding.
    let _ = std::fs::remove_file(sock);
    let listener = UnixListener::bind(sock)
        .with_context(|| format!("binding serve socket {}", sock.display()))?;
    eprintln!("rstest: serve listening on {}", sock.display());

    let scope = std::env::current_dir()?;
    let python = discover::resolve(&scope, cli.python.as_deref())?;

    // Phase 1: serve exactly one client, then exit.
    let (stream, _) = listener.accept().context("accepting serve client")?;
    let result = serve_client(stream, &python, args);
    let _ = std::fs::remove_file(sock);
    result
}

fn serve_client(stream: UnixStream, python: &Path, cli_args: &[String]) -> Result<i32> {
    let mut writer = stream.try_clone().context("cloning serve stream")?;
    let mut reader = rmp_serde::Deserializer::new(BufReader::new(stream));

    let mut worker: Option<worker::Worker> = None;

    // Each iteration reads one `{kind, payload}` envelope; a decode error / EOF
    // (client disconnected) ends the loop.
    while let Ok(msg) = Value::deserialize(&mut reader) {
        let kind = msg.get("kind").and_then(Value::as_str).unwrap_or("");
        let payload = msg.get("payload").cloned().unwrap_or(Value::Null);

        match kind {
            "hello" => {
                write_msg(
                    &mut writer,
                    "welcome",
                    json!({"proto": 1, "server": "rstest"}),
                )?;
            }
            "open_session" => {
                let sargs = session_args(&payload, cli_args);
                match open_session(python, &sargs) {
                    Ok((w, ids)) => {
                        worker = Some(w);
                        write_msg(
                            &mut writer,
                            "session_ready",
                            json!({"collected": ids.len()}),
                        )?;
                    }
                    Err(e) => {
                        write_msg(
                            &mut writer,
                            "error",
                            json!({"code": "collect_failed", "message": e.to_string()}),
                        )?;
                    }
                }
            }
            "run" => {
                let Some(w) = worker.as_mut() else {
                    write_msg(
                        &mut writer,
                        "error",
                        json!({"code": "bad_session", "message": "run before open_session"}),
                    )?;
                    continue;
                };
                let id = payload.get("id").and_then(Value::as_u64).unwrap_or(0);
                let ids = node_ids(&payload);
                let stop = payload
                    .get("stop_on_first_fail")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let overlay = overlay_files(&payload);
                run_subset(w, &mut writer, id, ids, overlay, stop)?;
            }
            "close_session" => {
                if let Some(mut w) = worker.take() {
                    // The serve plugin consumes a Shutdown command as session-end,
                    // so a graceful shutdown would leave the worker's outer loop
                    // blocked; SIGKILL + reap tears it down cleanly.
                    w.kill();
                    let _ = w.wait();
                }
                write_msg(&mut writer, "bye", json!({}))?;
            }
            "shutdown" => {
                if let Some(mut w) = worker.take() {
                    // The serve plugin consumes a Shutdown command as session-end,
                    // so a graceful shutdown would leave the worker's outer loop
                    // blocked; SIGKILL + reap tears it down cleanly.
                    w.kill();
                    let _ = w.wait();
                }
                write_msg(&mut writer, "bye", json!({}))?;
                break;
            }
            other => {
                write_msg(
                    &mut writer,
                    "error",
                    json!({"code": "bad_request", "message": format!("unknown kind {other}")}),
                )?;
            }
        }
    }
    Ok(0)
}

/// Spawn a warm serve worker: collect once, return it + the collected nodeids.
fn open_session(python: &Path, args: &[String]) -> Result<(worker::Worker, Vec<String>)> {
    let env = worker::WorkerEnv {
        run_uid: std::env::var("RSTEST_RUN_UID")
            .unwrap_or_else(|_| format!("serve-{}", std::process::id())),
        doctor: false,
        send_ids: true,
    };
    let mut w = worker::Worker::spawn_with_io(python, None, worker::Stdio::Null, &env)?;
    w.send(&proto::Command::RunServeSession {
        args: args.to_vec(),
    })?;
    loop {
        match w.recv()? {
            proto::Event::ServeReady { nodeids } => return Ok((w, nodeids)),
            proto::Event::CollectError { path, longrepr } => {
                anyhow::bail!("collection error in {path}: {longrepr}");
            }
            proto::Event::Done { .. } => anyhow::bail!("worker ended before collection"),
            _ => {}
        }
    }
}

/// Dispatch one run request to the warm worker and relay its reports to the
/// socket, closing with `run_done`.
fn run_subset(
    w: &mut worker::Worker,
    writer: &mut UnixStream,
    id: u64,
    ids: Vec<String>,
    overlay: std::collections::HashMap<String, String>,
    stop: bool,
) -> Result<()> {
    w.send(&proto::Command::ServeRun {
        req_id: id,
        ids,
        overlay,
        stop_on_first_fail: stop,
    })?;
    loop {
        match w.recv()? {
            proto::Event::ServeReport { req_id, report } if req_id == id => {
                write_msg(writer, "report", json!({"id": id, "report": report}))?;
            }
            proto::Event::ServeRunDone {
                req_id,
                killed,
                ran,
            } if req_id == id => {
                write_msg(
                    writer,
                    "run_done",
                    json!({"id": id, "killed": killed, "ran": ran}),
                )?;
                return Ok(());
            }
            _ => {}
        }
    }
}

/// Serialize `{kind, payload}` as msgpack and write it to the socket.
fn write_msg(stream: &mut UnixStream, kind: &str, payload: Value) -> Result<()> {
    let env = json!({"kind": kind, "payload": payload});
    let buf = rmp_serde::encode::to_vec_named(&env)?;
    stream.write_all(&buf)?;
    stream.flush()?;
    Ok(())
}

/// Collect a payload field that is an array of strings, dropping non-strings.
fn str_array(payload: &Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Session args for `open_session`: the client's `args`, or (when absent/empty)
/// the daemon's own CLI args.
fn session_args(payload: &Value, cli_args: &[String]) -> Vec<String> {
    let client = str_array(payload, "args");
    if client.is_empty() {
        cli_args.to_vec()
    } else {
        client
    }
}

/// The nodeid subset a `run` targets.
fn node_ids(payload: &Value) -> Vec<String> {
    str_array(payload, "node_ids")
}

/// The overlay carried by a `run`: `patch.files` is `{path: contents}` (the
/// mutation). Absent / `mode:"none"` (no `files`) -> empty, i.e. run the tree.
fn overlay_files(payload: &Value) -> std::collections::HashMap<String, String> {
    payload
        .get("patch")
        .and_then(|p| p.get("files"))
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_args_prefers_client_args() {
        let p = json!({"args": ["tests/a.py", "-k", "foo"]});
        assert_eq!(
            session_args(&p, &["cli".into()]),
            vec!["tests/a.py", "-k", "foo"]
        );
    }

    #[test]
    fn session_args_falls_back_to_cli_when_absent_or_empty() {
        let cli = vec!["daemon-arg".to_string()];
        assert_eq!(session_args(&json!({}), &cli), cli);
        assert_eq!(session_args(&json!({"args": []}), &cli), cli);
    }

    #[test]
    fn node_ids_extracts_strings_and_drops_non_strings() {
        let p = json!({"node_ids": ["t.py::a", 3, "t.py::b", null]});
        assert_eq!(node_ids(&p), vec!["t.py::a", "t.py::b"]);
        assert!(node_ids(&json!({})).is_empty());
    }

    #[test]
    fn overlay_files_parses_patch_files() {
        let p = json!({"patch": {"mode": "overlay", "files": {"m.py": "X = 1"}}});
        let ov = overlay_files(&p);
        assert_eq!(ov.get("m.py").map(String::as_str), Some("X = 1"));
    }

    #[test]
    fn overlay_files_empty_when_no_patch_or_no_files() {
        assert!(overlay_files(&json!({})).is_empty());
        assert!(overlay_files(&json!({"patch": {"mode": "none"}})).is_empty());
    }

    #[test]
    fn write_msg_roundtrips_through_msgpack() {
        // The envelope a client decodes must carry kind + payload intact.
        let env = json!({"kind": "run_done", "payload": {"id": 5, "killed": true, "ran": 2}});
        let buf = rmp_serde::encode::to_vec_named(&env).unwrap();
        let back: Value = rmp_serde::from_slice(&buf).unwrap();
        assert_eq!(back["kind"], "run_done");
        assert_eq!(back["payload"]["killed"], true);
        assert_eq!(back["payload"]["ran"], 2);
    }
}
