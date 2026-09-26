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
    /// When `Shutdown` was first sent. A reparented worker's exit timeout runs
    /// from here, so a wind-down that sends every Shutdown and then waits the
    /// workers one by one shares one deadline instead of stacking N of them.
    shutdown_sent: Option<std::time::Instant>,
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
    Reparented(Option<Tracked>),
}

/// A reparented worker's pid plus its process start time, captured right after
/// the fork. The worker is not our child, so init can reap it (and the kernel
/// recycle the pid) at any moment without us noticing; comparing start times
/// before signalling tells a recycled pid apart from our worker. `start` is
/// `None` where the platform exposes no start time (then only liveness is
/// checked).
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tracked {
    pid: u32,
    start: Option<u64>,
}

#[cfg(unix)]
impl Tracked {
    /// Start tracking `pid`; `None` if it is gone (nothing to track). Called
    /// while the zygote is still alive and the worker's parent, so the pid can't
    /// have been recycled yet: a worker that already died is still a zombie.
    /// A zombie is tracked too when its start time is known, so whoever ends
    /// up reaping it (possibly us, as PID 1 / a subreaper) can still verify it.
    fn capture(pid: u32) -> Option<Self> {
        match probe(pid) {
            Probe::Alive { start } => Some(Tracked { pid, start }),
            Probe::Zombie { start: Some(start) } => Some(Tracked {
                pid,
                start: Some(start),
            }),
            Probe::Zombie { start: None } | Probe::Gone => None,
        }
    }

    /// Whether `start` (a fresh probe's) identifies this worker. Without a
    /// recorded start time only liveness can be checked, so anything matches.
    fn same_start(&self, start: Option<u64>) -> bool {
        self.start.is_none() || start == self.start
    }

    /// This worker's current state. A pid now naming a different process (the
    /// worker was reaped and the pid recycled) or a zombie that can't be proven
    /// to be this worker is reported [`Status::Gone`]: never signalled or reaped.
    fn status(&self) -> Status {
        match probe(self.pid) {
            Probe::Alive { start } if self.same_start(start) => Status::Running,
            Probe::Zombie { start } if self.same_start(start) => Status::Zombie,
            _ => Status::Gone,
        }
    }
}

/// A tracked worker's state, identity already verified (see [`Tracked::status`]).
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum Status {
    Running,
    /// Exited, not yet reaped, and verifiably this worker.
    Zombie,
    /// Exited and reaped, or its pid now belongs to someone else.
    Gone,
}

/// What the OS reports for a pid we are not necessarily the parent of.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    /// No such process.
    Gone,
    /// Exited but not yet reaped. Counts as exited: a PID 1 that never reaps
    /// adopted orphans (common in containers) would otherwise leave every
    /// worker looking alive until the full exit timeout. `start` where the
    /// platform still exposes it for a zombie (Linux; not macOS).
    Zombie { start: Option<u64> },
    /// Running; `start` is its start time where the platform exposes one.
    Alive { start: Option<u64> },
}

/// Probe `pid` via `/proc/<pid>/stat` (state field + starttime).
#[cfg(target_os = "linux")]
fn probe(pid: u32) -> Probe {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_proc_stat(&stat).unwrap_or_else(|| probe_by_signal(pid)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Probe::Gone,
        Err(_) => probe_by_signal(pid),
    }
}

/// Parse a `/proc/<pid>/stat` line. Fields after the parenthesised comm (which
/// may itself contain spaces/parens, hence `rfind`) start at field 3 (state);
/// starttime is field 22.
#[cfg(target_os = "linux")]
fn parse_proc_stat(stat: &str) -> Option<Probe> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?;
    let start = fields.nth(22 - 4).and_then(|f| f.parse::<u64>().ok());
    if matches!(state, "Z" | "X" | "x") {
        return Some(Probe::Zombie { start });
    }
    Some(Probe::Alive {
        start: Some(start?),
    })
}

/// Probe `pid` via `proc_pidinfo(PROC_PIDTBSDINFO)` (status + start time).
#[cfg(target_os = "macos")]
fn probe(pid: u32) -> Probe {
    /// `SZOMB` from `<sys/proc.h>`; not exported by the libc crate.
    const SZOMB: u32 = 5;
    // SAFETY: an all-zero proc_bsdinfo is a valid value of this plain-data
    // struct; proc_pidinfo writes at most `size` bytes into it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a live, correctly sized buffer for PROC_PIDTBSDINFO.
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if got != size {
        let esrch = std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        return match probe_by_signal(pid) {
            // proc_pidinfo has no task info for a zombie (ESRCH) even though the
            // pid still exists, which is how a zombie is told apart here.
            Probe::Alive { .. } if esrch => Probe::Zombie { start: None },
            other => other,
        };
    }
    if info.pbi_status == SZOMB {
        return Probe::Zombie {
            start: Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec),
        };
    }
    Probe::Alive {
        start: Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec),
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn probe(pid: u32) -> Probe {
    probe_by_signal(pid)
}

/// Fallback probe: signal 0 checks existence only (a zombie still counts as
/// present, and there is no start time to detect pid reuse).
#[cfg(unix)]
fn probe_by_signal(pid: u32) -> Probe {
    // SAFETY: signal 0 only probes for existence; no memory access.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        Probe::Alive { start: None }
    } else {
        Probe::Gone
    }
}

/// Upper bound on waiting for the zygote's pid report. The zygote only imports
/// the vendored pytest core and forks (no collection), so this is generous; it
/// exists so a wedged interpreter start (a hanging `sitecustomize`, say) fails
/// the run with a diagnostic instead of hanging it before any watchdog exists.
#[cfg(unix)]
const ZYGOTE_REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Poll interval while waiting for a reparented worker to disappear.
#[cfg(unix)]
const REPARENTED_POLL: std::time::Duration = std::time::Duration::from_millis(10);
/// Upper bound on waiting for a reparented worker to exit after `Shutdown`. A
/// worker stuck in teardown past it is SIGKILLed rather than hanging the run.
#[cfg(unix)]
const REPARENTED_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long to wait for a SIGKILLed reparented worker to disappear. Normally
/// milliseconds; bounded so a process stuck in uninterruptible sleep can't hang
/// the run.
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
            shutdown_sent: None,
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
        if Self::prewarms(fork_prewarm, n) {
            ensure_fd_headroom(n);
            match Self::spawn_forked_pool(python, n, env) {
                // The zygote holds both ends of all 2n pipes at once (~4n fds,
                // vs ~2n for plain spawns, which close each worker's child ends
                // as they go), so it can hit the fd limit where the plain path
                // still fits: fall back to plain spawns rather than failing.
                Err(e) if is_fd_exhaustion(&e) => {}
                result => return result,
            }
        }
        #[cfg(not(unix))]
        let _ = fork_prewarm;
        (0..n)
            .map(|idx| Self::spawn(python, Some((idx, n)), env))
            .collect()
    }

    /// Whether [`Worker::spawn_pool`] with these arguments tries to fork off a
    /// zygote (Unix only, and only for a non-empty pool). It may still fall
    /// back to plain spawns; see [`Worker::is_forked`] for what happened.
    #[cfg(unix)]
    fn prewarms(fork_prewarm: bool, n: usize) -> bool {
        fork_prewarm && n > 0
    }

    /// Whether this worker was actually forked off a zygote (vs spawned).
    pub fn is_forked(&self) -> bool {
        match self.proc {
            Proc::Owned(_) => false,
            #[cfg(unix)]
            Proc::Reparented(_) => true,
        }
    }

    /// Send one msgpack [`proto::Command`] down the worker's command pipe.
    pub fn send(&mut self, cmd: &proto::Command) -> Result<()> {
        if matches!(cmd, proto::Command::Shutdown) && self.shutdown_sent.is_none() {
            self.shutdown_sent = Some(std::time::Instant::now());
        }
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
        self.proc.wait(self.shutdown_sent);
        Ok(())
    }

    /// Hard-kill the worker process (hang watchdog). The reader thread
    /// sees EOF and the normal crash machinery takes over.
    pub fn kill(&mut self) {
        self.proc.kill();
    }

    /// Wait for the worker process to exit (after a Shutdown was sent).
    pub fn wait(mut self) -> Result<()> {
        self.proc.wait(self.shutdown_sent);
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
                let Some(tracked) = *slot else { return };
                // Re-verify identity right before signalling: init may have
                // reaped the worker and the kernel recycled its pid since we last
                // looked. A gone/recycled pid is forgotten, never signalled.
                match tracked.status() {
                    Status::Running => {
                        // SAFETY: sending SIGKILL to a pid performs no memory
                        // access. The residual window between the probe and the
                        // signal is a few instructions, versus the unbounded one
                        // of trusting a stale pid.
                        unsafe { libc::kill(tracked.pid as libc::pid_t, libc::SIGKILL) };
                    }
                    // Already exited: nothing to signal, but keep tracking so the
                    // following wait/reap can `waitpid` it when we are its reaper
                    // (PID 1 / subreaper); forgetting it here would leave it
                    // <defunct>. If init reaps it instead and the pid is recycled,
                    // the start-time check rules the new process out.
                    Status::Zombie => {}
                    Status::Gone => *slot = None,
                }
            }
        }
    }

    /// Reap the process. Owned children must be `wait`ed to avoid a `<defunct>`
    /// zombie. A reparented worker is polled until it is gone so wind-down
    /// really waits for teardown, bounded by [`REPARENTED_EXIT_TIMEOUT`]
    /// counted from `since` (when Shutdown was sent; now if never).
    fn wait(&mut self, since: Option<std::time::Instant>) {
        #[cfg(unix)]
        let timeout = REPARENTED_EXIT_TIMEOUT
            .saturating_sub(since.map_or(std::time::Duration::ZERO, |t| t.elapsed()));
        #[cfg(not(unix))]
        let _ = since;
        #[cfg(unix)]
        self.wait_or_kill(timeout);
        #[cfg(not(unix))]
        {
            let Proc::Owned(child) = self;
            let _ = child.wait();
        }
    }

    /// [`Proc::wait`] with an explicit timeout for reparented workers (split out
    /// so tests can exercise the stuck-in-teardown fallback without waiting).
    /// A worker still running at the deadline (e.g. `sys.exit` joining a
    /// non-daemon thread a test left behind) is SIGKILLed: nothing else ever
    /// would, so it would outlive the run holding ports, connections and tmp.
    #[cfg(unix)]
    fn wait_or_kill(&mut self, timeout: std::time::Duration) {
        match self {
            Proc::Owned(child) => {
                let _ = child.wait();
            }
            Proc::Reparented(slot) => {
                if !wait_reparented(slot, timeout) {
                    self.kill();
                    if let Proc::Reparented(slot) = self {
                        wait_reparented(slot, REPARENTED_REAP_GRACE);
                    }
                }
            }
        }
    }

    /// Kill (if still running) and reap, immediately like an owned child: this
    /// runs on the event-loop thread, so it must not stall other workers. Safe
    /// for a reparented worker even if it already exited and init recycled its
    /// pid, because [`Proc::kill`] verifies identity before signalling; the
    /// short bounded wait after it only covers the SIGKILL taking effect.
    fn reap(&mut self) {
        self.kill();
        match self {
            Proc::Owned(child) => {
                let _ = child.wait();
            }
            #[cfg(unix)]
            Proc::Reparented(slot) => {
                wait_reparented(slot, REPARENTED_REAP_GRACE);
            }
        }
    }
}

/// Reap `pid` if it is an exited child of ours (`waitpid(WNOHANG)`); a no-op
/// (ECHILD) when init is its reaper.
#[cfg(unix)]
fn try_reap(pid: u32) -> bool {
    let pid = pid as libc::pid_t;
    // SAFETY: WNOHANG waitpid on a specific pid with a null status pointer
    // performs no memory access; ECHILD (not our child) is the normal case.
    let reaped = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    reaped == pid
}

/// Poll a reparented worker until it has exited or `timeout` elapses. Returns
/// true (and clears `slot`, so the pid is never signalled again) once it is
/// gone. `waitpid(WNOHANG)` reaps it ourselves when the orchestrator is the
/// reaper it was reparented to (rstest as PID 1 / a subreaper in a container);
/// otherwise it yields ECHILD and init does the reaping.
#[cfg(unix)]
fn wait_reparented(slot: &mut Option<Tracked>, timeout: std::time::Duration) -> bool {
    let Some(tracked) = *slot else {
        return true;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // Only `waitpid` a zombie proven to be this worker: blindly reaping the
        // pid could collect an unrelated child of ours that inherited it after
        // init reaped the worker, stealing that child's exit status. A zombie
        // counts as exited either way: if the reaper it was reparented to never
        // reaps (a non-init PID 1 in a container), it would otherwise look alive
        // until the timeout.
        let status = tracked.status();
        if status == Status::Zombie {
            try_reap(tracked.pid);
        }
        if status != Status::Running {
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
        // One cmd + one evt pipe per worker, plus a report pipe the zygote uses
        // to hand the forked child pids back.
        let mut cmds = Vec::with_capacity(n);
        let mut evts = Vec::with_capacity(n);
        for _ in 0..n {
            cmds.push(transport::pipe()?);
            evts.push(transport::pipe()?);
        }
        let mut report = transport::pipe()?;
        // Held open until every worker's identity is recorded: the zygote blocks
        // on it, staying the workers' parent so none of their pids can be
        // recycled before `Tracked::capture` has seen them.
        let mut release = transport::pipe()?;

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
        transport::prepare_parent_end(release.write.raw())?;
        transport::prepare_child_end(release.read.raw())?;

        // argv: --fork-pool <n> <report_write_fd> <release_read_fd> <cmd0> <evt0> ...
        // where cmd_i is the worker's command-READ end and evt_i its event-WRITE
        // end (matching _fork_pool in python/rstest_worker/__main__.py).
        let mut command = Command::new(python);
        command.args([
            "-m",
            "rstest_worker",
            "--fork-pool",
            &n.to_string(),
            &report.write.raw().to_string(),
            &release.read.raw().to_string(),
        ]);
        for i in 0..n {
            command.arg(cmds[i].read.raw().to_string());
            command.arg(evts[i].write.raw().to_string());
        }
        apply_shared_worker_env(&mut command, n, env);
        // Same stdio as a plain non-passthrough worker (see build_worker_command):
        // stdout is rendered Rust-side, and stdin must not be inherited because
        // `--watch` keeps a thread blocked reading it for `q`. Every forked
        // worker inherits the zygote's stdio.
        command
            .stdout(std::process::Stdio::null())
            .stdin(std::process::Stdio::null());

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
        release.read.close_now();

        // Read the forked child pids (newline-separated) off the report pipe.
        // Bounded: this runs before the pool's event loop and hang watchdog, so
        // an unbounded read would hang the run silently on a wedged zygote.
        let report_r = transport::into_file(report.read.take_raw());
        let pid_text = match read_with_timeout(report_r, ZYGOTE_REPORT_TIMEOUT) {
            Ok(text) => text,
            Err(e) => {
                // Killing the zygote also ends any child it already forked:
                // those see EOF on their command pipe once our write ends drop.
                let _ = zygote.kill();
                let _ = zygote.wait();
                return Err(e).context("reading forked worker pids from zygote");
            }
        };
        let pids = match parse_pid_report(&pid_text, n) {
            Ok(pids) => pids,
            Err(e) => {
                // Reap the zygote (it IS our child) so a bad report can't leave
                // it a zombie; it is blocked on the release pipe, so kill it.
                let _ = zygote.kill();
                let _ = zygote.wait();
                return Err(e);
            }
        };
        // Record identities while the zygote is still every worker's parent,
        // then release it: it exits and the workers reparent to init.
        let tracked: Vec<_> = pids.iter().map(|&pid| Tracked::capture(pid)).collect();
        release.write.close_now();
        let _ = zygote.wait();

        let mut workers = Vec::with_capacity(n);
        for (i, tracked) in tracked.into_iter().enumerate() {
            let cmd_w = transport::into_file(cmds[i].write.take_raw());
            let evt_r = transport::into_file(evts[i].read.take_raw());
            workers.push(Self {
                proc: Proc::Reparented(tracked),
                cmd_w,
                reader: Some(EventReader::new(evt_r)),
                shutdown_sent: None,
            });
        }
        Ok(workers)
    }
}

/// Raise the soft `RLIMIT_NOFILE` (never above the hard limit) so the zygote's
/// ~4n pipe fds fit alongside what the process already holds. Best effort: on
/// failure the fork path may still hit EMFILE and fall back to plain spawns.
#[cfg(unix)]
// `rlim_t` is u64 on Linux/macOS but signed on some BSDs.
#[allow(clippy::unnecessary_cast)]
fn ensure_fd_headroom(n: usize) {
    /// Allowance for fds the orchestrator already holds (stdio, caches, the
    /// watcher, ...) on top of the zygote's pipes.
    const BASELINE: u64 = 256;
    let want = (4 * n as u64 + 4).saturating_add(BASELINE);
    // SAFETY: an all-zero rlimit is a valid value of this plain-data struct.
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: `lim` is a live rlimit for getrlimit to fill.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return;
    }
    if (lim.rlim_cur as u64) >= want {
        return;
    }
    lim.rlim_cur = (want as libc::rlim_t).min(lim.rlim_max);
    // SAFETY: `lim` is a valid rlimit; the soft limit stays <= the hard one.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
}

/// Whether `err` is fd exhaustion (EMFILE/ENFILE) anywhere in its chain.
#[cfg(unix)]
fn is_fd_exhaustion(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| code == libc::EMFILE || code == libc::ENFILE)
    })
}

/// Read `file` to EOF as UTF-8, failing if EOF doesn't arrive within `timeout`.
#[cfg(unix)]
fn read_with_timeout(mut file: std::fs::File, timeout: std::time::Duration) -> Result<String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    let deadline = std::time::Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out after {}s", timeout.as_secs());
        }
        let mut pfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remaining.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: `pfd` is one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut pfd, 1, ms.max(1)) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if ready == 0 {
            continue;
        }
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(k) => buf.extend_from_slice(&chunk[..k]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(String::from_utf8(buf)?)
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
            super::Proc::Reparented(t) => t.expect("fresh worker has a pid").pid,
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
                super::Proc::Reparented(Some(t)) => {
                    assert!(alive(t.pid), "forked worker pid {} should be alive", t.pid);
                    pids.push(t.pid);
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

    /// `reap()` on a forked worker that is exiting on its own (the crash/EOF
    /// path) leaves it gone and forgets the pid, so it is never signalled
    /// again.
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
            super::Proc::Reparented(Some(t)) => t.pid,
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

    /// `reap` SIGKILLs a still-alive forked worker at once (the watchdog /
    /// decode-error case, on the event-loop thread) and then forgets it.
    #[cfg(unix)]
    #[test]
    fn reap_kills_live_forked_worker_without_grace() {
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
            super::Proc::Reparented(Some(t)) => t.pid,
            _ => panic!("fork-prewarm worker should be Reparented with a pid"),
        };
        assert!(
            alive(pid),
            "forked worker should block on its first command"
        );

        let started = std::time::Instant::now();
        worker.reap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "reap waited for a live worker instead of killing it at once"
        );

        assert!(
            !alive(pid),
            "forked worker pid {pid} survived the SIGKILL fallback"
        );
        assert!(matches!(worker.proc, super::Proc::Reparented(None)));
        // Once forgotten, kill and wait are no-ops (no signal to a recycled pid).
        worker.kill();
        worker.wait().expect("wait on a forgotten worker");
    }

    /// A forked worker still running when the exit timeout lapses (stuck in
    /// teardown) is SIGKILLed rather than abandoned to outlive the run.
    #[cfg(unix)]
    #[test]
    fn wait_kills_forked_worker_stuck_past_exit_timeout() {
        // Held throughout: see kill_then_wait_reaps_the_worker_child.
        let held = crate::test_env::lock();
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping fork-prewarm wait test: no python with pytest found");
            return;
        };
        let _worker_path = crate::test_env::set_var(&held, "RSTEST_WORKER_PATH", &worker_path);

        let mut env = base_env();
        env.run_uid = format!("fork-stuck-{}", std::process::id());
        let mut workers = Worker::spawn_pool(&python, 1, &env, true).expect("fork-prewarm pool");
        let mut worker = workers.pop().expect("one forked worker");
        let pid = match &worker.proc {
            super::Proc::Reparented(Some(t)) => t.pid,
            _ => panic!("fork-prewarm worker should be Reparented with a pid"),
        };
        // Never sent Shutdown: it blocks on its first command, i.e. it will not
        // exit on its own within the (zero) timeout.
        worker.proc.wait_or_kill(std::time::Duration::ZERO);

        assert!(!alive(pid), "stuck forked worker {pid} left running");
        assert!(matches!(worker.proc, super::Proc::Reparented(None)));
    }

    /// Only `Shutdown` stamps the wind-down deadline, and only the first one.
    #[cfg(unix)]
    #[test]
    fn shutdown_deadline_is_stamped_once_by_first_shutdown() {
        let held = crate::test_env::lock();
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping shutdown stamp test: no python with pytest found");
            return;
        };
        let _worker_path = crate::test_env::set_var(&held, "RSTEST_WORKER_PATH", &worker_path);

        let mut worker = Worker::spawn(&python, None, &base_env()).expect("spawn worker");
        assert!(worker.shutdown_sent.is_none());
        worker
            .send(&crate::scheduling::proto::Command::Shutdown)
            .expect("send shutdown");
        let first = worker.shutdown_sent.expect("stamped by Shutdown");
        let _ = worker.send(&crate::scheduling::proto::Command::Shutdown);
        assert_eq!(
            worker.shutdown_sent,
            Some(first),
            "restamped by a later Shutdown"
        );
        worker.wait().expect("wait");
    }

    #[cfg(unix)]
    #[test]
    fn fd_exhaustion_is_detected_through_context() {
        let emfile = anyhow::Error::from(std::io::Error::from_raw_os_error(libc::EMFILE))
            .context("pipe()")
            .context("spawning pool");
        assert!(super::is_fd_exhaustion(&emfile));
        let other = anyhow::Error::from(std::io::Error::from_raw_os_error(libc::EACCES));
        assert!(!super::is_fd_exhaustion(&other));
    }

    /// Raising headroom never lowers the soft limit and never exceeds the hard.
    #[cfg(unix)]
    #[test]
    fn ensure_fd_headroom_raises_soft_limit_within_hard() {
        let get = || {
            // SAFETY: an all-zero rlimit is a valid value of this plain-data struct.
            let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
            // SAFETY: `lim` is a live rlimit for getrlimit to fill.
            assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
            lim
        };
        let before = get();
        super::ensure_fd_headroom(64);
        let after = get();
        assert!(after.rlim_cur >= before.rlim_cur);
        assert!(after.rlim_cur <= after.rlim_max);
        assert!(after.rlim_cur >= (4 * 64 + 4 + 256).min(after.rlim_max));
    }

    /// `wait_reparented` gives up (false) when the worker outlives the timeout,
    /// keeping the pid, and returns true at once for an already-forgotten slot.
    #[cfg(unix)]
    #[test]
    fn wait_reparented_times_out_on_live_pid_and_skips_forgotten_slot() {
        // Our own pid is always alive and never our child (waitpid -> ECHILD).
        let me = super::Tracked::capture(std::process::id()).expect("self is alive");
        let mut slot = Some(me);
        assert!(!super::wait_reparented(
            &mut slot,
            std::time::Duration::ZERO
        ));
        assert_eq!(slot, Some(me), "a live pid must be kept");

        let mut gone = None;
        assert!(super::wait_reparented(&mut gone, std::time::Duration::ZERO));
    }

    /// A tracked pid whose start time no longer matches (the pid was recycled
    /// to another process) counts as exited: `wait_reparented` forgets it and
    /// `kill` never signals it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn recycled_pid_is_treated_as_exited_and_never_signalled() {
        let me = super::Tracked::capture(std::process::id()).expect("self is alive");
        let start = me.start.expect("linux/macos expose a start time");
        let recycled = super::Tracked {
            start: Some(start.wrapping_add(1)),
            ..me
        };
        assert_eq!(recycled.status(), super::Status::Gone);

        let mut slot = Some(recycled);
        assert!(super::wait_reparented(&mut slot, std::time::Duration::ZERO));
        assert_eq!(slot, None);

        // Would SIGKILL this test process if identity weren't re-checked.
        let mut proc = super::Proc::Reparented(Some(recycled));
        proc.kill();
        assert!(matches!(proc, super::Proc::Reparented(None)));
    }

    /// An exited-but-unreaped child (zombie) counts as exited, so a PID 1 that
    /// never reaps orphans can't stall wind-down for the full exit timeout.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn zombie_counts_as_exited() {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let tracked = super::Tracked {
            pid: child.id(),
            start: None,
        };
        // Our own child: it stays a zombie until we wait() it. Poll for the
        // zombie state rather than sleeping a fixed time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !matches!(super::probe(tracked.pid), super::Probe::Zombie { .. }) {
            assert!(
                std::time::Instant::now() < deadline,
                "child never became a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_ne!(tracked.status(), super::Status::Running);
        child.wait().expect("reap zombie");
    }

    /// Spawn `true` and wait until it is a zombie (our own child, so it stays
    /// one until we reap it: the PID 1 / subreaper case for a forked worker).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn zombie_child() -> u32 {
        let child = Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        // Dropping a Child neither kills nor waits it.
        drop(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !matches!(super::probe(pid), super::Probe::Zombie { .. }) {
            assert!(
                std::time::Instant::now() < deadline,
                "child never became a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        pid
    }

    /// `kill` on a zombie worker keeps it tracked, so the following `wait`
    /// reaps it instead of leaving it <defunct> when we are its reaper.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn kill_keeps_zombie_so_wait_reaps_it() {
        let pid = zombie_child();
        let tracked = super::Tracked { pid, start: None };
        let mut proc = super::Proc::Reparented(Some(tracked));
        proc.kill();
        assert!(
            matches!(proc, super::Proc::Reparented(Some(_))),
            "kill must not forget an unreaped zombie"
        );
        proc.wait(None);
        assert!(matches!(proc, super::Proc::Reparented(None)));
        assert_eq!(
            super::probe(pid),
            super::Probe::Gone,
            "zombie left unreaped"
        );
    }

    /// A worker already dead when first tracked: on Linux (zombie start time
    /// visible) it stays tracked so the following wait reaps it; on macOS
    /// (no zombie start time, and launchd is always the reaper) it is dropped.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn captured_zombie_is_reaped_only_when_its_identity_is_known() {
        let pid = zombie_child();
        let mut slot = super::Tracked::capture(pid);
        if cfg!(target_os = "linux") {
            assert!(slot.is_some(), "linux exposes a zombie's start time");
            assert!(super::wait_reparented(&mut slot, std::time::Duration::ZERO));
            assert_eq!(slot, None);
            assert_eq!(
                super::probe(pid),
                super::Probe::Gone,
                "zombie left unreaped"
            );
        } else {
            assert_eq!(slot, None);
            assert!(super::try_reap(pid), "clean up the test zombie");
        }
    }

    /// A zombie whose start time doesn't match the tracked worker (the worker
    /// was reaped and its pid recycled to another, now-exited child of ours) is
    /// forgotten without `waitpid`, leaving that child's exit status intact.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn zombie_with_other_identity_is_forgotten_not_reaped() {
        let pid = zombie_child();
        let mut slot = Some(super::Tracked {
            pid,
            start: Some(1),
        });
        assert!(super::wait_reparented(&mut slot, std::time::Duration::ZERO));
        assert_eq!(slot, None);
        assert!(
            matches!(super::probe(pid), super::Probe::Zombie { .. }),
            "someone else's zombie must not be reaped"
        );
        assert!(super::try_reap(pid), "clean up the test zombie");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_stat_reads_state_and_start_time() {
        // comm with spaces and a ')' exercises the rfind split.
        let line = "42 (a b) c) S 1 42 42 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 777 0 0";
        assert_eq!(
            super::parse_proc_stat(line),
            Some(super::Probe::Alive { start: Some(777) })
        );
        let zombie = "42 (w) Z 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 777 0 0";
        assert_eq!(
            super::parse_proc_stat(zombie),
            Some(super::Probe::Zombie { start: Some(777) })
        );
        assert_eq!(super::parse_proc_stat("garbage"), None);
    }

    /// The zygote-report read gives up when the writer never closes, and
    /// returns everything written once it does.
    #[cfg(unix)]
    #[test]
    fn read_with_timeout_times_out_on_open_writer_and_reads_to_eof() {
        let mut p = super::transport::pipe().expect("pipe");
        let r = super::transport::into_file(p.read.take_raw());
        let w = super::transport::into_file(p.write.take_raw());
        let err = super::read_with_timeout(r, std::time::Duration::from_millis(20)).unwrap_err();
        assert!(format!("{err:#}").contains("timed out"));

        let mut p = super::transport::pipe().expect("pipe");
        let r = super::transport::into_file(p.read.take_raw());
        let mut w2 = super::transport::into_file(p.write.take_raw());
        std::io::Write::write_all(&mut w2, b"11\n22\n").expect("write");
        drop(w2);
        let text = super::read_with_timeout(r, std::time::Duration::from_secs(5)).expect("read");
        assert_eq!(text, "11\n22\n");
        drop(w);
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
