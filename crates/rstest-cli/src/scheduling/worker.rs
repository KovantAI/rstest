use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::scheduling::proto;

/// Hard ceiling on the bytes a single [`proto::Event`] may consume off the
/// pipe. The wire is bare, self-describing msgpack (no length framing), so a
/// worker that emits a value whose length prefix claims a giant array/str/map
/// would otherwise drive an unbounded read - a crashed or wedged worker turns
/// into an orchestrator hang, and decode latency stops being predictable.
/// Capping per message keeps worst-case decode work bounded regardless of the
/// bytes on the wire. Legit payloads (even a CollectionDone for a millions-item
/// suite) sit far below this; override via `RSTEST_MAX_MESSAGE_BYTES` for the
/// rare suite that genuinely exceeds it.
const DEFAULT_MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;

fn max_message_bytes() -> usize {
    std::env::var("RSTEST_MAX_MESSAGE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_MESSAGE_BYTES)
}

/// A `Read` that refuses to yield more than a per-message budget. The budget is
/// reset before each [`EventReader::recv`], so any single event that tries to
/// read past the cap fails fast instead of allocating/looping unbounded. The
/// budget is `Arc<AtomicUsize>` (not a plain field) because the `Deserializer`
/// owns the reader after construction, yet `recv` still needs to reset it - and
/// `EventReader` is moved onto a dedicated reader thread, so the handle must be
/// `Send`.
struct LimitedReader<R> {
    inner: R,
    budget: Arc<AtomicUsize>,
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let allowed = self.budget.load(Ordering::Relaxed);
        if allowed == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker event exceeds RSTEST_MAX_MESSAGE_BYTES cap",
            ));
        }
        let cap = buf.len().min(allowed);
        let n = self.inner.read(&mut buf[..cap])?;
        self.budget.fetch_sub(n, Ordering::Relaxed);
        Ok(n)
    }
}

/// Run-wide parameters handed to a worker via its environment at spawn time
/// (thread-safe), rather than mutating the orchestrator's own process env with
/// `std::env::set_var` (a data race across threads, `unsafe` in edition 2024).
#[derive(Clone)]
pub struct WorkerEnv {
    /// Shared testrun uid (xdist `testrun_uid` contract); one per run.
    pub run_uid: String,
    /// Enable cpu/fixture instrumentation in the worker's shim plugin.
    pub doctor: bool,
    /// Per-test timeout in seconds (--timeout): the worker interrupts a test
    /// whose call phase overruns. None = disabled.
    pub timeout: Option<f64>,
    /// Enable per-test thread/fd leak measurement (--doctor or --fail-on-leak).
    pub leakcheck: bool,
    /// For a lone worker: ship the full id/location payload from collection
    /// (pooled workers derive this from their index instead).
    pub send_ids: bool,
    /// `--debug`: start debugpy in the worker on this port and wait for an
    /// editor to attach before collecting. Only ever set for the lone
    /// passthrough worker (a debug run forces single-worker mode). None = off.
    pub debug_port: Option<String>,
    /// A streaming consumer (`--output json` / `--stream-json`) is attached, so
    /// the worker ships captured stdout/stderr/log `sections` on **every**
    /// report, not just failures. Off by default to keep the wire lean.
    pub stream_output: bool,
}

/// Transport: a pair of anonymous OS pipes per worker (POSIX pipes on unix,
/// CreatePipe handles on Windows), never stdio (D4: fd 0/1/2 stay free). The
/// child gets its endpoints as numeric argv: fds on unix, HANDLEs on Windows.
pub struct Worker {
    proc: Proc,
    cmd_w: File,
    reader: Option<EventReader>,
}

/// How a worker's OS process is owned. The plain spawn path makes the worker a
/// direct child of the orchestrator (reap via `Child::wait`). The fork-prewarm
/// path (Unix) forks workers off a zygote that then exits, reparenting them to
/// init/launchd; the orchestrator never became their parent, so it signals them
/// by pid (`kill(pid, ...)`), polls for their exit, and relies on init to reap.
enum Proc {
    /// Direct child: the orchestrator spawned it and owns its exit status.
    Owned(Child),
    /// Fork-prewarmed worker, reparented to init after the zygote exited. Kill
    /// by pid; init reaps it. `None` once the worker is known to have exited, so
    /// a pid init already recycled is never signalled again.
    #[cfg(unix)]
    Reparented(Option<u32>),
}

/// Poll interval while waiting for a reparented worker to disappear.
#[cfg(unix)]
const REPARENTED_POLL: std::time::Duration = std::time::Duration::from_millis(10);
/// Upper bound on waiting for a reparented worker to exit after `Shutdown`. It
/// is not our child, so a worker stuck in teardown is abandoned rather than
/// hanging the run (it holds no pipe we still read).
#[cfg(unix)]
const REPARENTED_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Grace period `reap` gives a reparented worker to exit on its own (the
/// crash/EOF paths) before it falls back to SIGKILL.
#[cfg(unix)]
const REPARENTED_REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The read half of a worker's event pipe, split off from [`Worker`] so a
/// reader thread can own it while the orchestrator keeps the write half.
pub struct EventReader {
    events: rmp_serde::Deserializer<rmp_serde::decode::ReadReader<BufReader<LimitedReader<File>>>>,
    /// Per-message read budget, reset before every [`recv`] (see
    /// [`LimitedReader`]).
    budget: Arc<AtomicUsize>,
    cap: usize,
}

impl EventReader {
    fn new(evt_r: File) -> Self {
        let cap = max_message_bytes();
        let budget = Arc::new(AtomicUsize::new(cap));
        let reader = LimitedReader {
            inner: evt_r,
            budget: Arc::clone(&budget),
        };
        EventReader {
            events: rmp_serde::Deserializer::new(BufReader::new(reader)),
            budget,
            cap,
        }
    }

    /// Block for the next msgpack [`proto::Event`] from the worker. Each call
    /// refreshes the read budget so the cap applies per message, not per stream.
    pub fn recv(&mut self) -> Result<proto::Event> {
        use serde::Deserialize;
        self.budget.store(self.cap, Ordering::Relaxed);
        proto::Event::deserialize(&mut self.events).context("reading worker event")
    }
}

/// What the worker's stdout/stdin look like. The protocol always rides
/// dedicated pipes, so stdio is free to be either suppressed (we render)
/// or inherited (pytest renders: --co, -s, --pdb).
#[derive(Clone, Copy, PartialEq)]
pub enum Stdio {
    /// Suppress worker stdio; the orchestrator renders output itself.
    Null,
    /// Let the worker inherit stdio so pytest renders directly (`--co`, `-s`,
    /// `--pdb`).
    Inherit,
}

impl Worker {
    /// Spawn a worker with its stdio suppressed (the common case; the
    /// orchestrator renders output). `worker` is `(index, count)` in a pool,
    /// or `None` for the lone single-worker session.
    pub fn spawn(python: &Path, worker: Option<(usize, usize)>, env: &WorkerEnv) -> Result<Self> {
        Self::spawn_with_io(python, worker, Stdio::Null, env)
    }

    /// Spawn a worker, choosing whether its stdio is suppressed or inherited
    /// (see [`Stdio`]). Sets up the two dedicated pipes and the child's
    /// per-run environment.
    pub fn spawn_with_io(
        python: &Path,
        worker: Option<(usize, usize)>,
        io: Stdio,
        env: &WorkerEnv,
    ) -> Result<Self> {
        // cmd: parent writes -> child reads; evt: child writes -> parent reads.
        let cmd = transport::pipe()?;
        let evt = transport::pipe()?;
        transport::prepare_parent_end(cmd.write.raw())?;
        transport::prepare_parent_end(evt.read.raw())?;
        transport::prepare_child_end(cmd.read.raw())?;
        transport::prepare_child_end(evt.write.raw())?;

        let mut command =
            build_worker_command(python, worker, io, env, cmd.read.raw(), evt.write.raw());
        let child = command
            .spawn()
            .with_context(|| format!("spawning worker: {}", python.display()))?;

        // Close the child's ends in the parent or EOF detection breaks
        // (Endpoint::drop calls transport::close). The parent ends become
        // Files, which own the raw endpoint from here on.
        drop(cmd.read);
        drop(evt.write);
        let cmd_w = transport::into_file(cmd.write.into_raw());
        let evt_r = transport::into_file(evt.read.into_raw());
        Ok(Self {
            proc: Proc::Owned(child),
            cmd_w,
            reader: Some(EventReader::new(evt_r)),
        })
    }

    /// Spawn a pool of `n` workers with stdio suppressed. When `fork_prewarm` is
    /// set (Unix only) the workers are forked off one warm zygote that imports
    /// the vendored pytest core a single time; otherwise each is an independent
    /// `python -m rstest_worker` process (the portable path). Either way the
    /// caller drives the returned workers identically (send a session command,
    /// attach a reader). Respawn-after-crash always uses the plain per-worker
    /// [`Worker::spawn`], so this covers only the initial pool.
    pub fn spawn_pool(
        python: &Path,
        n: usize,
        env: &WorkerEnv,
        fork_prewarm: bool,
    ) -> Result<Vec<Self>> {
        #[cfg(unix)]
        if fork_prewarm && n > 0 {
            return Self::spawn_forked_pool(python, n, env);
        }
        #[cfg(not(unix))]
        let _ = fork_prewarm;
        (0..n)
            .map(|idx| Self::spawn(python, Some((idx, n)), env))
            .collect()
    }

    /// Send one msgpack [`proto::Command`] down the worker's command pipe.
    pub fn send(&mut self, cmd: &proto::Command) -> Result<()> {
        let buf = rmp_serde::encode::to_vec_named(cmd)?;
        self.cmd_w.write_all(&buf)?;
        self.cmd_w.flush()?;
        Ok(())
    }

    /// Detach the event stream (for a dedicated reader thread). Errors if the
    /// reader was already taken (would otherwise be a double-detach bug).
    pub fn take_reader(&mut self) -> Result<EventReader> {
        self.reader.take().context("event reader already detached")
    }

    /// Block for the next event on the still-attached reader. Errors if the
    /// reader was detached via [`Worker::take_reader`].
    pub fn recv(&mut self) -> Result<proto::Event> {
        self.reader
            .as_mut()
            .context("event reader was detached; recv is unavailable after take_reader")?
            .recv()
    }

    /// Ask the worker to exit cleanly (send `Shutdown`, then reap it).
    pub fn shutdown(mut self) -> Result<()> {
        self.send(&proto::Command::Shutdown)?;
        self.proc.wait();
        Ok(())
    }

    /// Hard-kill the worker process (hang watchdog). The reader thread
    /// sees EOF and the normal crash machinery takes over.
    pub fn kill(&mut self) {
        self.proc.kill();
    }

    /// Wait for the worker process to exit (after a Shutdown was sent).
    pub fn wait(mut self) -> Result<()> {
        self.proc.wait();
        Ok(())
    }

    /// Kill (if still running) and reap the child in place. Unlike [`Worker::wait`]
    /// this borrows `&mut self`, so a state slot that must stay in the pool's vec
    /// (marked dead, awaiting end-of-run cleanup) can be reaped immediately rather
    /// than lingering as an orphan / `<defunct>` zombie until then. Idempotent: a
    /// later `wait()` on the already-reaped child returns the cached `ExitStatus`
    /// (`std::process::Child::wait` stores it on first success and never calls
    /// `waitpid` again), so the end-of-run cleanup double-wait is safe.
    pub fn reap(&mut self) {
        self.proc.reap();
    }
}

impl Proc {
    /// Kill the process. `Child::kill` for an owned child; a SIGKILL by pid for
    /// a reparented fork-prewarmed worker (not our child, so no `Child` handle).
    fn kill(&mut self) {
        match self {
            Proc::Owned(child) => {
                let _ = child.kill();
            }
            #[cfg(unix)]
            Proc::Reparented(slot) => {
                if let Some(pid) = *slot {
                    // SAFETY: sending SIGKILL to a pid performs no memory access.
                    // Only reached while the worker has not been seen to exit, so
                    // init cannot have recycled the pid yet.
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                }
            }
        }
    }

    /// Reap the process. Owned children must be `wait`ed to avoid a `<defunct>`
    /// zombie. A reparented worker is polled until it is gone (bounded by
    /// [`REPARENTED_EXIT_TIMEOUT`]) so wind-down really waits for teardown.
    fn wait(&mut self) {
        match self {
            Proc::Owned(child) => {
                let _ = child.wait();
            }
            #[cfg(unix)]
            Proc::Reparented(slot) => {
                wait_reparented(slot, REPARENTED_EXIT_TIMEOUT);
            }
        }
    }

    /// Kill (if still running) and reap. A reparented worker on the crash/EOF
    /// paths has usually exited already and been reaped by init, whose pid may
    /// then be reused: give it [`REPARENTED_REAP_GRACE`] to disappear, and only
    /// SIGKILL if it is still present afterwards (then still ours).
    fn reap(&mut self) {
        #[cfg(unix)]
        self.reap_with_grace(REPARENTED_REAP_GRACE);
        #[cfg(not(unix))]
        {
            self.kill();
            self.wait();
        }
    }

    /// [`Proc::reap`] with an explicit grace for reparented workers (split out
    /// so tests can exercise the still-alive SIGKILL fallback without waiting).
    #[cfg(unix)]
    fn reap_with_grace(&mut self, grace: std::time::Duration) {
        match self {
            Proc::Owned(_) => {
                self.kill();
                self.wait();
            }
            Proc::Reparented(slot) => {
                if !wait_reparented(slot, grace) {
                    self.kill();
                    self.wait();
                }
            }
        }
    }
}

/// Poll a reparented worker until it has exited or `timeout` elapses. Returns
/// true (and clears `slot`, so the pid is never signalled again) once it is
/// gone. `waitpid(WNOHANG)` reaps it ourselves when the orchestrator is the
/// reaper it was reparented to (rstest as PID 1 / a subreaper in a container);
/// otherwise it yields ECHILD and init does the reaping.
#[cfg(unix)]
fn wait_reparented(slot: &mut Option<u32>, timeout: std::time::Duration) -> bool {
    let Some(pid) = *slot else {
        return true;
    };
    let pid = pid as libc::pid_t;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // SAFETY: WNOHANG waitpid on a specific pid with a null status pointer
        // performs no memory access; ECHILD (not our child) is the normal case.
        let reaped = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } == pid;
        // SAFETY: signal 0 only probes for existence; no memory access.
        if reaped || unsafe { libc::kill(pid, 0) } != 0 {
            *slot = None;
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(REPARENTED_POLL);
    }
}

/// Build the worker's [`Command`] (argv + per-run child environment + stdio)
/// without spawning it. Split out of [`Worker::spawn_with_io`] so the arg/env
/// wiring is unit-testable via `Command::get_args`/`get_envs` — no live process.
/// `cmd_fd`/`evt_fd` are the child's raw pipe endpoints (fd on unix, HANDLE on
/// Windows), passed to the worker as numeric argv.
fn build_worker_command(
    python: &Path,
    worker: Option<(usize, usize)>,
    io: Stdio,
    env: &WorkerEnv,
    cmd_fd: u64,
    evt_fd: u64,
) -> Command {
    let mut command = Command::new(python);
    // `--debug`: turn off frozen modules so debugpy's breakpoints land
    // reliably (frozen stdlib frames swallow them) and its startup
    // frozen-modules warning stays quiet. An interpreter flag, so it must
    // precede `-m`. Only for a debug run — no cost on the normal path.
    if env.debug_port.is_some() {
        command.args(["-X", "frozen_modules=off"]);
    }
    command
        .args([
            "-m",
            "rstest_worker",
            &cmd_fd.to_string(),
            &evt_fd.to_string(),
        ])
        .env("PYTHONPATH", worker_pythonpath())
        // Run-wide params ride the CHILD's environment (thread-safe), never
        // process-global `set_var` (which races across threads / is unsafe
        // in edition 2024).
        .env("RSTEST_RUN_UID", &env.run_uid)
        // Worker stdout is not ours to show: output is rendered Rust-side,
        // except passthrough mode which inherits so pytest renders. stderr
        // stays inherited for worker crash visibility.
        .stdout(match io {
            Stdio::Null => std::process::Stdio::null(),
            Stdio::Inherit => std::process::Stdio::inherit(),
        })
        // stdin is only the worker's in passthrough (pdb, `input()` under
        // -s). Elsewhere it must not inherit: `--watch` keeps a thread
        // blocked reading stdin for `q`, and on Windows a pending synchronous
        // read on a pipe blocks the child interpreter's startup probe of fd 0,
        // hanging every worker.
        .stdin(match io {
            Stdio::Null => std::process::Stdio::null(),
            Stdio::Inherit => std::process::Stdio::inherit(),
        });
    if env.doctor {
        command.env("RSTEST_DOCTOR", "1");
    }
    if let Some(secs) = env.timeout {
        command.env("RSTEST_TIMEOUT", secs.to_string());
    }
    if env.leakcheck {
        command.env("RSTEST_LEAKCHECK", "1");
    }
    if let Some(port) = &env.debug_port {
        command.env("RSTEST_DEBUGPY_PORT", port);
    }
    if env.stream_output {
        command.env("RSTEST_STREAM_OUTPUT", "1");
    }
    // Exactly one worker ships the full id list (D5); the rest verify their
    // collection by count+hash. Worker 0 in a pool; the lone worker only
    // when the caller asks (collect-only discovery / migrate-check).
    let send_ids = match worker {
        Some((idx, _)) => idx == 0,
        None => env.send_ids,
    };
    command.env("RSTEST_SEND_IDS", if send_ids { "1" } else { "0" });
    if let Some((idx, count)) = worker {
        command
            .env("RSTEST_WORKER_ID", format!("gw{idx}"))
            .env("RSTEST_WORKER_COUNT", count.to_string())
            // Workers get disjoint tmp roots (xdist popen-gwN pattern):
            // pytest's numbered-dir cleanup races when siblings share
            // a basetemp parent.
            .env(
                "RSTEST_BASETEMP",
                std::env::temp_dir().join(format!("rstest-{}", std::process::id())),
            );
    }
    command
}

#[cfg(unix)]
impl Worker {
    /// Fork-prewarm `n` workers off one warm zygote (see [`Worker::spawn_pool`]).
    /// The orchestrator creates every pipe, spawns the zygote passing all child
    /// endpoints via argv, and reads back the forked child pids. Each child is
    /// reparented to init when the zygote exits, so its [`Proc`] is
    /// `Reparented(pid)` — killable by signal, reaped by init.
    fn spawn_forked_pool(python: &Path, n: usize, env: &WorkerEnv) -> Result<Vec<Self>> {
        use std::io::Read;

        // One cmd + one evt pipe per worker, plus a report pipe the zygote uses
        // to hand the forked child pids back.
        let mut cmds = Vec::with_capacity(n);
        let mut evts = Vec::with_capacity(n);
        for _ in 0..n {
            cmds.push(transport::pipe()?);
            evts.push(transport::pipe()?);
        }
        let mut report = transport::pipe()?;

        // Parent keeps: each cmd write, each evt read, the report read — all
        // CLOEXEC so they never leak into the zygote (leaked evt-write ends would
        // defeat per-worker EOF/crash detection). Child ends inherit as-is.
        for c in &cmds {
            transport::prepare_parent_end(c.write.raw())?;
            transport::prepare_child_end(c.read.raw())?;
        }
        for e in &evts {
            transport::prepare_parent_end(e.read.raw())?;
            transport::prepare_child_end(e.write.raw())?;
        }
        transport::prepare_parent_end(report.read.raw())?;
        transport::prepare_child_end(report.write.raw())?;

        // argv: --fork-pool <n> <report_write_fd> <cmd0> <evt0> <cmd1> <evt1> ...
        // where cmd_i is the worker's command-READ end and evt_i its event-WRITE
        // end (matching _fork_pool in python/rstest_worker/__main__.py).
        let mut command = Command::new(python);
        command.args([
            "-m",
            "rstest_worker",
            "--fork-pool",
            &n.to_string(),
            &report.write.raw().to_string(),
        ]);
        for i in 0..n {
            command.arg(cmds[i].read.raw().to_string());
            command.arg(evts[i].write.raw().to_string());
        }
        apply_shared_worker_env(&mut command, n, env);
        command.stdout(std::process::Stdio::null());

        let mut zygote = command
            .spawn()
            .with_context(|| format!("spawning worker zygote: {}", python.display()))?;

        // Close the child ends in the parent so EOF detection works and no
        // endpoint leaks; the parent ends become owned Files below.
        for c in &mut cmds {
            c.read.close_now();
        }
        for e in &mut evts {
            e.write.close_now();
        }
        report.write.close_now();

        // Read the forked child pids (newline-separated) off the report pipe.
        let mut report_r = transport::into_file(report.read.take_raw());
        let mut pid_text = String::new();
        report_r
            .read_to_string(&mut pid_text)
            .context("reading forked worker pids from zygote")?;
        // The zygote exits right after forking; reap it (it IS our child) before
        // validating the report, so a bad report can't leave it a zombie.
        let _ = zygote.wait();
        let pids = parse_pid_report(&pid_text, n)?;

        let mut workers = Vec::with_capacity(n);
        for i in 0..n {
            let cmd_w = transport::into_file(cmds[i].write.take_raw());
            let evt_r = transport::into_file(evts[i].read.take_raw());
            workers.push(Self {
                proc: Proc::Reparented(Some(pids[i])),
                cmd_w,
                reader: Some(EventReader::new(evt_r)),
            });
        }
        Ok(workers)
    }
}

/// Parse the zygote's newline-separated pid report, requiring exactly `n` pids.
#[cfg(unix)]
fn parse_pid_report(text: &str, n: usize) -> Result<Vec<u32>> {
    let pids: Vec<u32> = text
        .split_whitespace()
        .map(|s| s.parse::<u32>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing zygote pid report {text:?}"))?;
    if pids.len() != n {
        anyhow::bail!("zygote reported {} worker pids, expected {n}", pids.len());
    }
    Ok(pids)
}

/// Set the run-wide worker environment shared by every forked child on the
/// zygote's [`Command`]. Per-worker identity (`RSTEST_WORKER_ID`,
/// `RSTEST_SEND_IDS`) is intentionally omitted: all children share this one
/// environment, so each applies its own identity by index post-fork.
#[cfg(unix)]
fn apply_shared_worker_env(command: &mut Command, n: usize, env: &WorkerEnv) {
    command
        .env("PYTHONPATH", worker_pythonpath())
        .env("RSTEST_RUN_UID", &env.run_uid)
        .env("RSTEST_WORKER_COUNT", n.to_string())
        .env(
            "RSTEST_BASETEMP",
            std::env::temp_dir().join(format!("rstest-{}", std::process::id())),
        );
    if env.doctor {
        command.env("RSTEST_DOCTOR", "1");
    }
    if let Some(secs) = env.timeout {
        command.env("RSTEST_TIMEOUT", secs.to_string());
    }
    if env.leakcheck {
        command.env("RSTEST_LEAKCHECK", "1");
    }
    if env.stream_output {
        command.env("RSTEST_STREAM_OUTPUT", "1");
    }
}

/// RAII owner for one raw pipe endpoint (a file descriptor on unix, a HANDLE
/// on Windows). Closes on drop so an early return between `transport::pipe()`
/// and a successful `spawn()` can't leak the endpoint. Call [`Endpoint::into_raw`]
/// to defuse it when ownership is deliberately handed off (the child inherits
/// it, or it becomes a parent-side `File`).
struct Endpoint(Option<u64>);

impl Endpoint {
    fn new(raw: u64) -> Self {
        Endpoint(Some(raw))
    }

    /// The raw value, without giving up ownership (for fcntl/argv/etc.).
    fn raw(&self) -> u64 {
        self.0.expect("endpoint used after into_raw")
    }

    /// Take ownership of the raw value; drop no longer closes it.
    fn into_raw(mut self) -> u64 {
        self.0.take().expect("endpoint already taken")
    }

    /// Take the raw value out (defusing Drop) without closing it, via `&mut`
    /// so it works on an endpoint still living inside a `Vec<Pipe>`. Used when
    /// the endpoint is being handed to a `File` that will own the close.
    #[cfg(unix)]
    fn take_raw(&mut self) -> u64 {
        self.0.take().expect("endpoint already taken")
    }

    /// Close the endpoint now and defuse Drop, via `&mut` (for endpoints living
    /// in a `Vec<Pipe>`, where the by-value [`Endpoint::into_raw`] can't move
    /// out). A no-op if already taken.
    #[cfg(unix)]
    fn close_now(&mut self) {
        if let Some(raw) = self.0.take() {
            transport::close(raw);
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Some(raw) = self.0.take() {
            transport::close(raw);
        }
    }
}

/// Endpoint values are numeric and platform-meaningful: file descriptors
/// on unix, HANDLEs on Windows.
struct Pipe {
    read: Endpoint,
    write: Endpoint,
}

#[cfg(unix)]
mod transport {
    use std::fs::File;
    use std::os::fd::FromRawFd;

    use anyhow::{Context, Result};

    use super::Pipe;

    pub fn pipe() -> Result<Pipe> {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a valid 2-element array; libc::pipe writes exactly
        // two fds into it. Return value is checked before the fds are read.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("pipe()");
        }
        Ok(Pipe {
            read: super::Endpoint::new(fds[0] as u64),
            write: super::Endpoint::new(fds[1] as u64),
        })
    }

    /// Parent ends must not leak into the child (CLOEXEC) or EOF
    /// detection breaks.
    pub fn prepare_parent_end(fd: u64) -> Result<()> {
        let fd = fd as i32;
        // SAFETY: F_GETFD reads the fd's flags; `fd` is a live pipe endpoint
        // owned by an Endpoint. Result checked for the -1 error sentinel below.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFD)");
        }
        // SAFETY: F_SETFD writes the flag int back to the same live `fd`.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl(FD_CLOEXEC)");
        }
        Ok(())
    }

    /// Child ends are inherited as-is on unix (pipe() fds are not CLOEXEC).
    pub fn prepare_child_end(_fd: u64) -> Result<()> {
        Ok(())
    }

    pub fn close(fd: u64) {
        // SAFETY: `fd` is a live pipe endpoint whose ownership is being given
        // up here (the caller is an Endpoint that no longer uses it).
        unsafe { libc::close(fd as i32) };
    }

    pub fn into_file(fd: u64) -> File {
        // SAFETY: `fd` is a live, owned pipe endpoint; ownership transfers to
        // the returned File (its Drop will close it exactly once).
        unsafe { File::from_raw_fd(fd as i32) }
    }
}

#[cfg(windows)]
mod transport {
    //! Anonymous pipes; child ends made inheritable, HANDLE values passed
    //! via argv, converted to CRT fds in the worker with msvcrt.open_osfhandle.

    use std::fs::File;
    use std::os::windows::io::FromRawHandle;

    use anyhow::{bail, Result};

    use super::Pipe;

    use windows_sys::Win32::Foundation::{
        CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;

    pub fn pipe() -> Result<Pipe> {
        let mut read: HANDLE = std::ptr::null_mut();
        let mut write: HANDLE = std::ptr::null_mut();
        // SAFETY: `read`/`write` are valid out-pointers; CreatePipe fills them
        // with two handles. Null security attrs = default. Return value checked
        // before the handles are used.
        let ok = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) };
        if ok == 0 {
            bail!("CreatePipe failed: {}", std::io::Error::last_os_error());
        }
        Ok(Pipe {
            read: super::Endpoint::new(read as u64),
            write: super::Endpoint::new(write as u64),
        })
    }

    /// Parent ends stay non-inheritable (CreatePipe default with a null
    /// security descriptor).
    pub fn prepare_parent_end(_handle: u64) -> Result<()> {
        Ok(())
    }

    /// Child ends must be explicitly inheritable; std's Command spawns
    /// with bInheritHandles=TRUE when stdio is configured (it is: stdout
    /// is always set), so inheritable handles reach the child.
    pub fn prepare_child_end(handle: u64) -> Result<()> {
        // SAFETY: `handle` is a live pipe endpoint owned by an Endpoint;
        // SetHandleInformation only flips its inherit flag. Return value checked.
        let ok = unsafe {
            SetHandleInformation(handle as HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
        };
        if ok == 0 {
            bail!(
                "SetHandleInformation failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    pub fn close(handle: u64) {
        // SAFETY: `handle` is a live pipe endpoint whose ownership is being
        // given up here (the Endpoint no longer uses it).
        unsafe { CloseHandle(handle as HANDLE) };
    }

    pub fn into_file(handle: u64) -> File {
        // SAFETY: `handle` is a live, owned pipe endpoint; ownership transfers
        // to the returned File (its Drop closes it exactly once).
        unsafe { File::from_raw_handle(handle as *mut _) }
    }
}

/// Locate the rstest_worker package. Dev layout: exe sits in target/<profile>/,
/// package in <repo>/python/. Installed wheels ship the package inside
/// site-packages instead, making this a no-op.
pub fn worker_pythonpath() -> String {
    build_pythonpath(
        std::env::var("RSTEST_WORKER_PATH").ok().as_deref(),
        std::env::var("PYTHONPATH").ok().as_deref(),
    )
}

/// Pure join logic behind [`worker_pythonpath`], split out so tests can exercise
/// the explicit-override + PYTHONPATH ordering without mutating process-global
/// env (`set_var` races other tests and is `unsafe` in edition 2024). The
/// `current_exe`-derived dev path is read-only, so it stays inline.
fn build_pythonpath(explicit: Option<&str>, existing_pythonpath: Option<&str>) -> String {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = explicit {
        paths.push(PathBuf::from(explicit));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(repo) = exe.ancestors().nth(3) {
            let dev = repo.join("python");
            if dev.join("rstest_worker").is_dir() {
                paths.push(dev);
            }
        }
    }
    if let Some(existing) = existing_pythonpath {
        for p in std::env::split_paths(existing) {
            paths.push(p);
        }
    }
    std::env::join_paths(paths)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{build_worker_command, Endpoint, LimitedReader, Stdio, WorkerEnv};
    use crate::scheduling::proto;
    use serde::Deserialize;
    use std::collections::HashMap;
    use std::io::{BufReader, Cursor, Read};
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The budget is a hard ceiling: once exhausted, reads fail instead of
    /// draining the underlying source (which is what bounds decode work).
    #[test]
    fn limited_reader_stops_at_budget() {
        let budget = Arc::new(AtomicUsize::new(10));
        let mut r = LimitedReader {
            inner: Cursor::new(vec![0u8; 1000]),
            budget: Arc::clone(&budget),
        };
        let mut sink = Vec::new();
        // read_to_end surfaces the cap as an error after exactly 10 bytes.
        let err = r.read_to_end(&mut sink).unwrap_err();
        assert_eq!(sink.len(), 10);
        assert_eq!(budget.load(Ordering::Relaxed), 0);
        assert!(
            err.to_string().contains("RSTEST_MAX_MESSAGE_BYTES"),
            "{err}"
        );
    }

    /// Decode an event through the same LimitedReader path recv() uses: a frame
    /// larger than the budget is rejected fast, never read to completion.
    #[test]
    fn oversized_event_is_rejected_not_read_to_completion() {
        // A legit-but-large event: a Stopped with 50k unrun indices.
        let big = proto::Event::Stopped {
            unrun: (0..50_000u64).collect(),
        };
        let bytes = rmp_serde::encode::to_vec_named(&big).unwrap();
        assert!(bytes.len() > 1024, "frame should be large: {}", bytes.len());

        let decode_with = |cap: usize| {
            let budget = Arc::new(AtomicUsize::new(cap));
            let reader = LimitedReader {
                inner: Cursor::new(bytes.clone()),
                budget,
            };
            let mut de = rmp_serde::Deserializer::new(BufReader::new(reader));
            proto::Event::deserialize(&mut de)
        };

        // Tiny cap trips before the frame is fully read.
        assert!(decode_with(64).is_err());
        // Ample cap decodes the exact same bytes fine.
        assert!(decode_with(bytes.len() + 1).is_ok());
    }

    /// A quiet baseline WorkerEnv (no doctor / timeout / debug / stream).
    fn base_env() -> WorkerEnv {
        WorkerEnv {
            run_uid: "uid-1".into(),
            doctor: false,
            timeout: None,
            leakcheck: false,
            send_ids: false,
            debug_port: None,
            stream_output: false,
        }
    }

    fn args_of(c: &Command) -> Vec<String> {
        c.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn envs_of(c: &Command) -> HashMap<String, String> {
        c.get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn build_command_default_run_has_no_debug_or_stream_env() {
        let cmd = build_worker_command(Path::new("python3"), None, Stdio::Null, &base_env(), 3, 4);
        // Plain argv: no `-X frozen_modules=off`, the two fds passed through.
        assert_eq!(args_of(&cmd), ["-m", "rstest_worker", "3", "4"]);
        let envs = envs_of(&cmd);
        assert_eq!(envs["RSTEST_RUN_UID"], "uid-1");
        // Lone worker, send_ids false → "0".
        assert_eq!(envs["RSTEST_SEND_IDS"], "0");
        for absent in [
            "RSTEST_DEBUGPY_PORT",
            "RSTEST_STREAM_OUTPUT",
            "RSTEST_DOCTOR",
            "RSTEST_TIMEOUT",
            "RSTEST_LEAKCHECK",
            "RSTEST_WORKER_ID",
        ] {
            assert!(!envs.contains_key(absent), "unexpected {absent}");
        }
    }

    #[test]
    fn build_command_debug_prepends_frozen_modules_and_sets_port() {
        let mut env = base_env();
        env.debug_port = Some("5678".into());
        let cmd = build_worker_command(Path::new("python3"), None, Stdio::Inherit, &env, 3, 4);
        // The interpreter flag must precede `-m`.
        assert_eq!(
            args_of(&cmd),
            ["-X", "frozen_modules=off", "-m", "rstest_worker", "3", "4"]
        );
        assert_eq!(envs_of(&cmd)["RSTEST_DEBUGPY_PORT"], "5678");
    }

    #[test]
    fn build_command_stream_output_sets_env() {
        let mut env = base_env();
        env.stream_output = true;
        let cmd = build_worker_command(Path::new("python3"), None, Stdio::Null, &env, 3, 4);
        assert_eq!(envs_of(&cmd)["RSTEST_STREAM_OUTPUT"], "1");
        // Still no debug flag when only streaming.
        assert!(!args_of(&cmd).contains(&"-X".to_string()));
    }

    #[test]
    fn build_command_doctor_timeout_leakcheck_envs() {
        let mut env = base_env();
        env.doctor = true;
        env.timeout = Some(1.5);
        env.leakcheck = true;
        let cmd = build_worker_command(Path::new("python3"), None, Stdio::Null, &env, 3, 4);
        let envs = envs_of(&cmd);
        assert_eq!(envs["RSTEST_DOCTOR"], "1");
        assert_eq!(envs["RSTEST_TIMEOUT"], "1.5");
        assert_eq!(envs["RSTEST_LEAKCHECK"], "1");
    }

    #[test]
    fn build_command_pool_worker_identity_and_send_ids() {
        // Worker 0 of a pool ships ids and carries the gwN identity.
        let cmd0 = build_worker_command(
            Path::new("python3"),
            Some((0, 4)),
            Stdio::Null,
            &base_env(),
            3,
            4,
        );
        let e0 = envs_of(&cmd0);
        assert_eq!(e0["RSTEST_WORKER_ID"], "gw0");
        assert_eq!(e0["RSTEST_WORKER_COUNT"], "4");
        assert_eq!(e0["RSTEST_SEND_IDS"], "1");
        assert!(e0.contains_key("RSTEST_BASETEMP"));

        // A non-zero pool worker does not ship ids.
        let cmd1 = build_worker_command(
            Path::new("python3"),
            Some((1, 4)),
            Stdio::Null,
            &base_env(),
            3,
            4,
        );
        let e1 = envs_of(&cmd1);
        assert_eq!(e1["RSTEST_WORKER_ID"], "gw1");
        assert_eq!(e1["RSTEST_SEND_IDS"], "0");
    }

    #[test]
    fn endpoint_into_raw_takes_ownership_without_closing() {
        // A fake, never-opened raw value: raw() reads it, into_raw() hands it
        // off and defuses Drop (no close of an fd we don't own).
        let e = Endpoint::new(0xDEAD_BEEF);
        assert_eq!(e.raw(), 0xDEAD_BEEF);
        assert_eq!(e.into_raw(), 0xDEAD_BEEF);
    }

    #[test]
    fn worker_pythonpath_includes_explicit_override() {
        // The explicit override is pushed first, so it must appear in the joined
        // result. Exercises build_pythonpath directly to avoid mutating the
        // process-global RSTEST_WORKER_PATH (which would race parallel tests).
        let sentinel = "/tmp/rstest-pp-sentinel";
        let pp = super::build_pythonpath(Some(sentinel), None);
        assert!(
            pp.contains(sentinel),
            "PYTHONPATH {pp:?} missing explicit override {sentinel:?}"
        );
    }

    // fcntl(F_GETFD) on an fd that was never opened fails with EBADF, driving
    // the parent-end error path. F_SETFD's own failure branch is left uncovered
    // (no way to make GETFD succeed but SETFD fail on the same live fd).
    #[cfg(unix)]
    #[test]
    fn prepare_parent_end_errors_on_a_bad_fd() {
        // A high fd number that is not open in the test process.
        let err = super::transport::prepare_parent_end(1_000_000).unwrap_err();
        assert!(err.to_string().contains("F_GETFD"), "{err}");
    }

    // WorkerEnv / Path already imported at the module top (used by the
    // build_worker_command tests on every platform).
    #[cfg(unix)]
    use super::Worker;
    #[cfg(unix)]
    use std::path::PathBuf;

    /// A repo python that can import pytest + rstest_worker, plus the worker
    /// PYTHONPATH root. None => skip (no suitable interpreter present).
    #[cfg(unix)]
    fn worker_python() -> Option<(PathBuf, PathBuf)> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)?
            .to_path_buf();
        let worker_path = repo.join("python");
        let candidates = [
            std::env::var("RSTEST_TEST_PYTHON").ok().map(PathBuf::from),
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

    /// Whether `pid` still exists. A zombie (killed but un-reaped) still counts
    /// as alive here — signal 0 succeeds until the parent `wait()`s it away.
    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 performs no action; it only probes whether `pid`
        // is a live, signalable process. Touches no memory.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    /// Both pools' respawn arm reaps the old worker with `kill()` + `wait()`
    /// before replacing it: a decode-error respawn can leave the child alive,
    /// and `Child`'s drop neither kills nor waits. After kill+wait the pid must
    /// be GONE (ESRCH) — a `<defunct>` zombie would still answer `kill(pid, 0)`,
    /// so this asserts the reaping `wait()`, not merely the `kill()`.
    #[cfg(unix)]
    #[test]
    fn kill_then_wait_reaps_the_worker_child() {
        // Held throughout: worker_python() and the spawn resolve python via
        // PATH, and the spawn reads RSTEST_WORKER_PATH.
        let held = crate::test_env::lock();
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping reap test: no python with pytest found");
            return;
        };
        // Point production `worker_pythonpath()` at the repo package.
        let _worker_path = crate::test_env::set_var(&held, "RSTEST_WORKER_PATH", &worker_path);

        let env = WorkerEnv {
            run_uid: format!("reap-{}", std::process::id()),
            doctor: false,
            timeout: None,
            leakcheck: false,
            send_ids: false,
            debug_port: None,
            stream_output: false,
        };
        // A freshly spawned worker blocks on its first command: alive, and never
        // sent anything — the decode-error/respawn precondition (child still
        // running against the pipe, not a clean exit).
        let mut worker = Worker::spawn(&python, None, &env).expect("spawn worker");
        let pid = match &worker.proc {
            super::Proc::Owned(child) => child.id(),
            #[cfg(unix)]
            super::Proc::Reparented(pid) => pid.expect("fresh worker has a pid"),
        };
        assert!(alive(pid), "worker should be alive right after spawn");

        // Exactly what the respawn arm now does with the old worker.
        worker.kill();
        let _ = worker.wait();

        // wait() returning is itself proof the child was reaped; the alive()
        // probe is the observable proxy. It can in theory false-fail if the
        // kernel recycles `pid` to another live process between wait() and the
        // probe, but this thread spawns nothing after wait(), so that window is
        // negligible (and reuse can only spuriously fail, never falsely pass).
        assert!(
            !alive(pid),
            "worker pid {pid} still present after kill+wait -> orphan or <defunct> zombie"
        );
    }

    /// Fork-prewarm spawns N live workers off one zygote, each a distinct
    /// reparented pid on its own pipe pair, and each shuts down cleanly with no
    /// zombie left behind (init reaps the orphans). Exercises the whole zygote
    /// round-trip: pid report parse, per-worker Files, and `Proc::Reparented`
    /// kill/wait.
    #[cfg(unix)]
    #[test]
    fn fork_prewarm_pool_spawns_distinct_live_workers_and_reaps_clean() {
        // Held throughout: see kill_then_wait_reaps_the_worker_child.
        let held = crate::test_env::lock();
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping fork-prewarm test: no python with pytest found");
            return;
        };
        let _worker_path = crate::test_env::set_var(&held, "RSTEST_WORKER_PATH", &worker_path);

        let env = WorkerEnv {
            run_uid: format!("fork-{}", std::process::id()),
            doctor: false,
            timeout: None,
            leakcheck: false,
            send_ids: false,
            debug_port: None,
            stream_output: false,
        };
        let n = 3;
        let workers = Worker::spawn_pool(&python, n, &env, true).expect("fork-prewarm pool");
        assert_eq!(workers.len(), n, "expected {n} forked workers");

        let mut pids = Vec::new();
        for w in &workers {
            match &w.proc {
                super::Proc::Reparented(Some(pid)) => {
                    assert!(alive(*pid), "forked worker pid {pid} should be alive");
                    pids.push(*pid);
                }
                super::Proc::Reparented(None) => panic!("fresh forked worker lost its pid"),
                super::Proc::Owned(_) => panic!("fork-prewarm worker should be Reparented"),
            }
        }
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(pids.len(), n, "forked worker pids must be distinct");

        // Clean shutdown: each worker blocks on its first command, so Shutdown
        // ends it. shutdown() waits until the pid is gone, so it must already be
        // absent on return — a lingering pid would mean wait() returned early or
        // a leaked/zombied worker.
        for w in workers {
            w.shutdown().expect("shutdown forked worker");
        }
        for pid in pids {
            assert!(
                !alive(pid),
                "forked worker pid {pid} still alive after shutdown"
            );
        }
    }

    /// `reap()` on a forked worker that already exited (the crash/EOF path) must
    /// see it gone and forget the pid instead of SIGKILLing it, so a pid init
    /// has recycled is never signalled.
    #[cfg(unix)]
    #[test]
    fn reap_forgets_pid_of_exited_forked_worker() {
        // Held throughout: see kill_then_wait_reaps_the_worker_child.
        let held = crate::test_env::lock();
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping fork-prewarm reap test: no python with pytest found");
            return;
        };
        let _worker_path = crate::test_env::set_var(&held, "RSTEST_WORKER_PATH", &worker_path);

        let env = WorkerEnv {
            run_uid: format!("fork-reap-{}", std::process::id()),
            doctor: false,
            timeout: None,
            leakcheck: false,
            send_ids: false,
            debug_port: None,
            stream_output: false,
        };
        let mut workers = Worker::spawn_pool(&python, 1, &env, true).expect("fork-prewarm pool");
        let mut worker = workers.pop().expect("one forked worker");
        let pid = match &worker.proc {
            super::Proc::Reparented(Some(pid)) => *pid,
            _ => panic!("fork-prewarm worker should be Reparented with a pid"),
        };

        // The worker exits on its own, as after a crash, before reap runs.
        worker
            .send(&crate::scheduling::proto::Command::Shutdown)
            .expect("send shutdown");
        worker.reap();

        assert!(
            !alive(pid),
            "forked worker pid {pid} still alive after reap"
        );
        assert!(
            matches!(worker.proc, super::Proc::Reparented(None)),
            "reap must forget the pid of an exited worker"
        );
    }

    /// A still-alive forked worker survives the reap grace, so `reap` falls
    /// back to SIGKILL (the watchdog/decode-error case) and then forgets it.
    /// A zero grace keeps the test instant.
    #[cfg(unix)]
    #[test]
    fn reap_kills_forked_worker_still_alive_after_grace() {
        // Held throughout: see kill_then_wait_reaps_the_worker_child.
        let held = crate::test_env::lock();
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping fork-prewarm kill test: no python with pytest found");
            return;
        };
        let _worker_path = crate::test_env::set_var(&held, "RSTEST_WORKER_PATH", &worker_path);

        let mut env = base_env();
        env.run_uid = format!("fork-kill-{}", std::process::id());
        let mut workers = Worker::spawn_pool(&python, 1, &env, true).expect("fork-prewarm pool");
        let mut worker = workers.pop().expect("one forked worker");
        let pid = match &worker.proc {
            super::Proc::Reparented(Some(pid)) => *pid,
            _ => panic!("fork-prewarm worker should be Reparented with a pid"),
        };
        assert!(
            alive(pid),
            "forked worker should block on its first command"
        );

        worker.proc.reap_with_grace(std::time::Duration::ZERO);

        assert!(
            !alive(pid),
            "forked worker pid {pid} survived the SIGKILL fallback"
        );
        assert!(matches!(worker.proc, super::Proc::Reparented(None)));
        // Once forgotten, kill and wait are no-ops (no signal to a recycled pid).
        worker.kill();
        worker.wait().expect("wait on a forgotten worker");
    }

    /// `wait_reparented` gives up (false) when the worker outlives the timeout,
    /// keeping the pid, and returns true at once for an already-forgotten slot.
    #[cfg(unix)]
    #[test]
    fn wait_reparented_times_out_on_live_pid_and_skips_forgotten_slot() {
        // Our own pid is always alive and never our child (waitpid -> ECHILD).
        let mut slot = Some(std::process::id());
        assert!(!super::wait_reparented(
            &mut slot,
            std::time::Duration::ZERO
        ));
        assert_eq!(slot, Some(std::process::id()), "a live pid must be kept");

        let mut gone = None;
        assert!(super::wait_reparented(&mut gone, std::time::Duration::ZERO));
    }

    /// `reap` on a directly spawned (owned) worker kills and waits it.
    #[cfg(unix)]
    #[test]
    fn reap_kills_and_waits_owned_worker() {
        // Held throughout: see kill_then_wait_reaps_the_worker_child.
        let held = crate::test_env::lock();
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping owned reap test: no python with pytest found");
            return;
        };
        let _worker_path = crate::test_env::set_var(&held, "RSTEST_WORKER_PATH", &worker_path);

        let mut worker = Worker::spawn(&python, None, &base_env()).expect("spawn worker");
        let pid = match &worker.proc {
            super::Proc::Owned(child) => child.id(),
            super::Proc::Reparented(_) => panic!("plain spawn should be Owned"),
        };
        worker.reap();
        assert!(
            !alive(pid),
            "owned worker pid {pid} still present after reap"
        );
    }

    /// Without fork-prewarm, `spawn_pool` spawns `n` independent owned workers.
    #[cfg(unix)]
    #[test]
    fn spawn_pool_without_prewarm_spawns_owned_workers() {
        // Held throughout: see kill_then_wait_reaps_the_worker_child.
        let held = crate::test_env::lock();
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping plain pool test: no python with pytest found");
            return;
        };
        let _worker_path = crate::test_env::set_var(&held, "RSTEST_WORKER_PATH", &worker_path);

        let workers = Worker::spawn_pool(&python, 2, &base_env(), false).expect("plain pool");
        assert_eq!(workers.len(), 2);
        for w in workers {
            assert!(matches!(w.proc, super::Proc::Owned(_)));
            w.shutdown().expect("shutdown plain worker");
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_pid_report_accepts_exact_count_and_rejects_mismatch_or_garbage() {
        assert_eq!(
            super::parse_pid_report("11\n22\n", 2).unwrap(),
            vec![11, 22]
        );
        let short = super::parse_pid_report("11\n", 2).unwrap_err();
        assert!(format!("{short:#}").contains("reported 1 worker pids, expected 2"));
        let bad = super::parse_pid_report("11\nxx\n", 2).unwrap_err();
        assert!(format!("{bad:#}").contains("parsing zygote pid report"));
    }

    /// The zygote carries every run-wide opt-in flag its children inherit.
    #[cfg(unix)]
    #[test]
    fn shared_worker_env_sets_run_wide_flags() {
        let mut env = base_env();
        env.doctor = true;
        env.timeout = Some(1.5);
        env.leakcheck = true;
        env.stream_output = true;
        let mut cmd = Command::new("python");
        super::apply_shared_worker_env(&mut cmd, 3, &env);
        let envs = envs_of(&cmd);
        assert_eq!(envs["RSTEST_WORKER_COUNT"], "3");
        assert_eq!(envs["RSTEST_DOCTOR"], "1");
        assert_eq!(envs["RSTEST_TIMEOUT"], "1.5");
        assert_eq!(envs["RSTEST_LEAKCHECK"], "1");
        assert_eq!(envs["RSTEST_STREAM_OUTPUT"], "1");
        // Per-worker identity is applied post-fork, never on the shared env.
        assert!(!envs.contains_key("RSTEST_WORKER_ID"));
        assert!(!envs.contains_key("RSTEST_SEND_IDS"));
    }
}
