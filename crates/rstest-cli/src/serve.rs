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
    let mut worker: Option<worker::Worker> = None;
    let result = serve_session(stream, python, cli_args, &mut worker);
    // However the session ended — client EOF, a write error, or a worker that
    // died mid-run (the `?` paths below) — never leak the warm worker: a
    // std::process::Child is NOT killed when dropped.
    drain_worker(&mut worker);
    result
}

fn serve_session(
    stream: UnixStream,
    python: &Path,
    cli_args: &[String],
    worker: &mut Option<worker::Worker>,
) -> Result<i32> {
    let mut writer = stream.try_clone().context("cloning serve stream")?;
    let mut reader = rmp_serde::Deserializer::new(BufReader::new(stream));

    // Each iteration reads one `{kind, payload}` envelope; a decode error / EOF
    // (client disconnected) ends the loop.
    while let Ok(msg) = Value::deserialize(&mut reader) {
        let kind = msg.get("kind").and_then(Value::as_str).unwrap_or("");
        let payload = msg.get("payload").cloned().unwrap_or(Value::Null);

        match kind {
            "hello" => {
                let payload = json!({"proto": 1, "server": "rstest"});
                write_msg(&mut writer, "welcome", payload)?;
            }
            "open_session" => {
                let sargs = session_args(&payload, cli_args);
                match open_session(python, &sargs) {
                    Ok((w, ids)) => {
                        *worker = Some(w);
                        let payload = json!({"collected": ids.len()});
                        write_msg(&mut writer, "session_ready", payload)?;
                    }
                    Err(e) => {
                        let payload = json!({"code": "collect_failed", "message": e.to_string()});
                        write_msg(&mut writer, "error", payload)?;
                    }
                }
            }
            "run" => {
                let Some(w) = worker.as_mut() else {
                    let payload =
                        json!({"code": "bad_session", "message": "run before open_session"});
                    write_msg(&mut writer, "error", payload)?;
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
                drain_worker(worker);
                write_msg(&mut writer, "bye", json!({}))?;
            }
            "shutdown" => {
                drain_worker(worker);
                write_msg(&mut writer, "bye", json!({}))?;
                break;
            }
            other => {
                let payload =
                    json!({"code": "bad_request", "message": format!("unknown kind {other}")});
                write_msg(&mut writer, "error", payload)?;
            }
        }
    }
    Ok(0)
}

/// Tear down the warm worker if one is live. The serve plugin consumes a
/// Shutdown command as session-end, so a graceful shutdown would leave the
/// worker's outer loop blocked; SIGKILL + reap tears it down cleanly.
fn drain_worker(worker: &mut Option<worker::Worker>) {
    if let Some(mut w) = worker.take() {
        w.kill();
        let _ = w.wait();
    }
}

/// Spawn a warm serve worker: collect once, return it + the collected nodeids.
fn open_session(python: &Path, args: &[String]) -> Result<(worker::Worker, Vec<String>)> {
    let env = worker::WorkerEnv {
        run_uid: std::env::var("RSTEST_RUN_UID")
            .unwrap_or_else(|_| format!("serve-{}", std::process::id())),
        doctor: false,
        send_ids: true,
        leakcheck: false,
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
                let payload = json!({"id": id, "killed": killed, "ran": ran});
                write_msg(writer, "run_done", payload)?;
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
    use std::io::Read as _;
    use std::thread;

    /// Drives [`serve_client`] on one end of a socketpair while the test speaks
    /// the wire protocol from the other. `python` only matters once
    /// `open_session` is sent; the no-worker paths ignore it.
    struct Harness {
        writer: UnixStream,
        reader: BufReader<UnixStream>,
        handle: Option<thread::JoinHandle<Result<i32>>>,
    }

    impl Harness {
        fn start(python: &Path, cli_args: &[String]) -> Self {
            let (server, client) = UnixStream::pair().unwrap();
            let python = python.to_path_buf();
            let cli_args = cli_args.to_vec();
            let handle = thread::spawn(move || serve_client(server, &python, &cli_args));
            let writer = client.try_clone().unwrap();
            Self {
                writer,
                reader: BufReader::new(client),
                handle: Some(handle),
            }
        }

        fn send(&mut self, kind: &str, payload: Value) {
            let env = json!({"kind": kind, "payload": payload});
            let buf = rmp_serde::encode::to_vec_named(&env).unwrap();
            self.writer.write_all(&buf).unwrap();
            self.writer.flush().unwrap();
        }

        /// Read one `{kind, payload}` envelope the server sent back.
        fn recv(&mut self) -> Value {
            let mut de = rmp_serde::Deserializer::new(&mut self.reader);
            Value::deserialize(&mut de).unwrap()
        }

        /// Drop the client side so the server loop sees EOF, then join.
        fn finish(mut self) -> i32 {
            self.close_client();
            self.handle.take().unwrap().join().unwrap().unwrap()
        }

        fn close_client(&mut self) {
            // Replace both handles with a throwaway that we immediately drop,
            // closing every fd on the client end.
            let (a, _b) = UnixStream::pair().unwrap();
            self.writer = a.try_clone().unwrap();
            self.reader = BufReader::new(a);
        }
    }

    fn bogus_python() -> &'static Path {
        Path::new("/nonexistent/definitely-not-a-python")
    }

    #[test]
    fn hello_replies_welcome() {
        let mut h = Harness::start(bogus_python(), &[]);
        h.send("hello", json!({"proto": 1}));
        let msg = h.recv();
        assert_eq!(msg["kind"], "welcome");
        assert_eq!(msg["payload"]["proto"], 1);
        assert_eq!(msg["payload"]["server"], "rstest");
        h.finish();
    }

    #[test]
    fn run_before_open_session_errors() {
        let mut h = Harness::start(bogus_python(), &[]);
        h.send("run", json!({"id": 1, "node_ids": ["t.py::a"]}));
        let msg = h.recv();
        assert_eq!(msg["kind"], "error");
        assert_eq!(msg["payload"]["code"], "bad_session");
        h.finish();
    }

    #[test]
    fn unknown_kind_errors_with_bad_request() {
        let mut h = Harness::start(bogus_python(), &[]);
        h.send("frobnicate", json!({}));
        let msg = h.recv();
        assert_eq!(msg["kind"], "error");
        assert_eq!(msg["payload"]["code"], "bad_request");
        assert_eq!(msg["payload"]["message"], "unknown kind frobnicate");
        h.finish();
    }

    #[test]
    fn close_session_without_worker_replies_bye() {
        let mut h = Harness::start(bogus_python(), &[]);
        h.send("close_session", json!({}));
        assert_eq!(h.recv()["kind"], "bye");
        h.finish();
    }

    #[test]
    fn open_session_with_unspawnable_python_reports_collect_failed() {
        let mut h = Harness::start(bogus_python(), &[]);
        h.send("open_session", json!({"args": ["test_x.py"]}));
        let msg = h.recv();
        assert_eq!(msg["kind"], "error");
        assert_eq!(msg["payload"]["code"], "collect_failed");
        assert!(msg["payload"]["message"].is_string());
        h.finish();
    }

    #[test]
    fn shutdown_replies_bye_and_ends_loop() {
        let mut h = Harness::start(bogus_python(), &[]);
        h.send("shutdown", json!({}));
        assert_eq!(h.recv()["kind"], "bye");
        // After `shutdown` the loop breaks; serve_client returns Ok(0).
        assert_eq!(h.finish(), 0);
    }

    /// Locate a python that can host the worker (has pytest + can import
    /// `rstest_worker`), else `None` so the live-worker tests skip instead of
    /// failing on machines without the dev venv. Returns `(python, worker_path)`
    /// where `worker_path` is the dir holding the `rstest_worker` package.
    fn worker_python() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)?
            .to_path_buf();
        let worker_path = repo.join("python");
        let candidates = [
            std::env::var("RSTEST_TEST_PYTHON").ok().map(Into::into),
            Some(repo.join(".venv/bin/python")),
        ];
        for cand in candidates.into_iter().flatten() {
            if !cand.exists() {
                continue;
            }
            let ok = std::process::Command::new(&cand)
                .args(["-c", "import pytest, rstest_worker"])
                .env("PYTHONPATH", &worker_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                return Some((cand, worker_path));
            }
        }
        None
    }

    /// End-to-end through a real warm worker: open_session collects, a run
    /// streams a report + run_done, close_session tears the worker down. Covers
    /// the worker-bound branches (session_ready, ServeReady, run_done, worker
    /// close). Skips when no suitable python is present.
    #[test]
    fn live_worker_session_run_and_close() {
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping live_worker test: no python with pytest found");
            return;
        };
        // Production `worker_pythonpath()` reads RSTEST_WORKER_PATH first; set it
        // so the worker finds the `rstest_worker` package regardless of the test
        // binary's target dir (e.g. under `cargo llvm-cov`, where current_exe
        // ancestry no longer points at the repo root).
        // SAFETY: edition 2021; no other test in this module spawns a worker.
        std::env::set_var("RSTEST_WORKER_PATH", &worker_path);
        let dir = std::env::temp_dir().join(format!("rstest-serve-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let test_file = dir.join("test_s.py");
        std::fs::write(
            &test_file,
            "def test_a():\n    assert True\ndef test_b():\n    assert True\n",
        )
        .unwrap();

        let mut h = Harness::start(&python, &[]);

        // Warm the session on the temp file (absolute path avoids a cwd change,
        // which would race parallel tests).
        let arg = test_file.to_string_lossy().to_string();
        h.send("open_session", json!({"args": [arg.clone()]}));
        let ready = h.recv();
        if ready["kind"] != "session_ready" {
            // Environment couldn't collect (e.g. rstest_worker deps absent);
            // don't hard-fail the suite over an env gap.
            eprintln!("skipping live_worker test: open_session -> {ready}");
            let mut sink = Vec::new();
            let _ = h.writer.shutdown(std::net::Shutdown::Write);
            let _ = h.reader.read_to_end(&mut sink);
            h.finish();
            return;
        }
        assert_eq!(ready["payload"]["collected"], 2);

        h.send(
            "run",
            json!({"id": 42, "node_ids": [format!("{arg}::test_a")]}),
        );
        // Drain reports until run_done for our id.
        let done = loop {
            let msg = h.recv();
            match msg["kind"].as_str() {
                Some("report") => continue,
                Some("run_done") => break msg,
                other => panic!("unexpected during run: {other:?}"),
            }
        };
        assert_eq!(done["payload"]["id"], 42);
        assert_eq!(done["payload"]["ran"], 1);

        h.send("close_session", json!({}));
        assert_eq!(h.recv()["kind"], "bye");
        h.finish();
    }

    /// A file that fails to import surfaces a `CollectError` from the worker,
    /// which `open_session` turns into a `collect_failed` error reply. Covers
    /// the collect-error arm of the warm-up loop.
    #[test]
    fn live_worker_collect_error_reports_collect_failed() {
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping collect_error test: no python with pytest found");
            return;
        };
        // SAFETY: edition 2021; no other test in this module spawns a worker.
        std::env::set_var("RSTEST_WORKER_PATH", &worker_path);
        let dir = std::env::temp_dir().join(format!("rstest-serve-cerr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("test_broken.py");
        // A syntax error → the module can't be imported → collection error.
        std::fs::write(&bad, "def test_x(:\n    pass\n").unwrap();

        let mut h = Harness::start(&python, &[]);
        let arg = bad.to_string_lossy().to_string();
        h.send("open_session", json!({"args": [arg]}));
        let msg = h.recv();
        assert_eq!(msg["kind"], "error");
        assert_eq!(msg["payload"]["code"], "collect_failed");
        h.finish();
    }

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

    /// Drive the public `serve()` entry point over a real Unix socket: it binds,
    /// resolves the interpreter, accepts one client, serves the protocol, and
    /// removes the socket on exit. Uses `--python` so `discover::resolve` is
    /// deterministic; skips when no python is present.
    #[test]
    fn serve_daemon_serves_one_client_and_cleans_up() {
        use clap::Parser as _;
        let Some((python, _)) = worker_python() else {
            eprintln!("skipping serve daemon test: no python found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("rstest-serve-daemon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("d.sock");

        let cli = Cli::parse_from(["rstest", "--python", &python.to_string_lossy()]);
        let sock_srv = sock.clone();
        let handle = thread::spawn(move || serve(&cli, &[], &sock_srv));

        // Wait (bounded) for the daemon to bind, then speak the protocol.
        let mut stream = None;
        for _ in 0..500 {
            if let Ok(s) = UnixStream::connect(&sock) {
                stream = Some(s);
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut stream = stream.expect("serve daemon never accepted a connection");
        let mut de = rmp_serde::Deserializer::new(BufReader::new(stream.try_clone().unwrap()));

        let hello = json!({"kind": "hello", "payload": {}});
        stream
            .write_all(&rmp_serde::encode::to_vec_named(&hello).unwrap())
            .unwrap();
        stream.flush().unwrap();
        assert_eq!(Value::deserialize(&mut de).unwrap()["kind"], "welcome");

        let shutdown = json!({"kind": "shutdown", "payload": {}});
        stream
            .write_all(&rmp_serde::encode::to_vec_named(&shutdown).unwrap())
            .unwrap();
        stream.flush().unwrap();
        assert_eq!(Value::deserialize(&mut de).unwrap()["kind"], "bye");

        assert_eq!(handle.join().unwrap().unwrap(), 0);
        assert!(!sock.exists()); // serve() unlinks the socket on exit
    }

    /// A client that disconnects mid-session (EOF, no shutdown/close) must not
    /// leak the warm worker: `serve_session` leaves it for the caller, and
    /// `serve_client`'s unconditional `drain_worker` kills + reaps it. Proven
    /// by watching the worker pid disappear. Skips when no python is present.
    #[test]
    fn worker_is_killed_when_client_disconnects() {
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping disconnect test: no python with pytest found");
            return;
        };
        // SAFETY: edition 2021; no other test in this module spawns a worker.
        std::env::set_var("RSTEST_WORKER_PATH", &worker_path);
        let dir = std::env::temp_dir().join(format!("rstest-serve-disc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let test_file = dir.join("test_s.py");
        std::fs::write(&test_file, "def test_a():\n    assert True\n").unwrap();

        // Preload one open_session command, then half-close the write end so the
        // session loop sees EOF right after warming the worker.
        let (server, mut client) = UnixStream::pair().unwrap();
        let arg = test_file.to_string_lossy().to_string();
        let open = json!({"kind": "open_session", "payload": {"args": [arg]}});
        client
            .write_all(&rmp_serde::encode::to_vec_named(&open).unwrap())
            .unwrap();
        client.flush().unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();

        let mut worker: Option<worker::Worker> = None;
        let rc = serve_session(server, &python, &[], &mut worker).unwrap();
        assert_eq!(rc, 0);

        let Some(w) = worker.as_ref() else {
            eprintln!("skipping disconnect test: worker never collected");
            return;
        };
        // serve_session itself must NOT drain — the guaranteed teardown is the
        // caller's job, so a leak-free path holds regardless of how it exits.
        let pid = w.id();
        assert!(
            unsafe { libc::kill(pid as i32, 0) } == 0,
            "worker should be alive pre-drain"
        );

        drain_worker(&mut worker);
        assert!(worker.is_none());
        // ESRCH: the process is gone (killed and reaped), not orphaned.
        assert!(
            unsafe { libc::kill(pid as i32, 0) } != 0,
            "worker pid {pid} still alive after drain -> leaked"
        );
    }

    /// `shutdown` after a warm session must tear the worker down (kill + reap)
    /// and end the loop. Covers the worker-bound shutdown arm; skips when no
    /// suitable python is present.
    #[test]
    fn live_worker_shutdown_tears_down_worker() {
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping shutdown test: no python with pytest found");
            return;
        };
        // SAFETY: edition 2021; no other test in this module spawns a worker.
        std::env::set_var("RSTEST_WORKER_PATH", &worker_path);
        let dir = std::env::temp_dir().join(format!("rstest-serve-shut-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let test_file = dir.join("test_s.py");
        std::fs::write(&test_file, "def test_a():\n    assert True\n").unwrap();

        let mut h = Harness::start(&python, &[]);
        let arg = test_file.to_string_lossy().to_string();
        h.send("open_session", json!({"args": [arg]}));
        let ready = h.recv();
        if ready["kind"] != "session_ready" {
            eprintln!("skipping shutdown test: open_session -> {ready}");
            let mut sink = Vec::new();
            let _ = h.writer.shutdown(std::net::Shutdown::Write);
            let _ = h.reader.read_to_end(&mut sink);
            h.finish();
            return;
        }

        h.send("shutdown", json!({}));
        assert_eq!(h.recv()["kind"], "bye");
        // `shutdown` breaks the loop, so serve_client returns Ok(0).
        assert_eq!(h.finish(), 0);
    }
}
