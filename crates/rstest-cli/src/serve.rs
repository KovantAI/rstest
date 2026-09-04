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
                // Session args: the client's, else the daemon's CLI args.
                let sargs: Vec<String> = payload
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .filter(|v: &Vec<String>| !v.is_empty())
                    .unwrap_or_else(|| cli_args.to_vec());
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
                let ids: Vec<String> = payload
                    .get("node_ids")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let stop = payload
                    .get("stop_on_first_fail")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // Overlay patch: `patch.files` is {path: contents} (mutation
                // carrier). Absent / `mode:"none"` -> run current disk.
                let overlay: std::collections::HashMap<String, String> = payload
                    .get("patch")
                    .and_then(|p| p.get("files"))
                    .and_then(Value::as_object)
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
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
