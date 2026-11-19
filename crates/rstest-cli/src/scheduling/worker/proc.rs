//! Worker process handles: the spawned or forked child, liveness probes,
//! killing and reaping (including reparented forks), and fd headroom.

use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

/// How a worker's OS process is owned. The plain spawn path makes the worker a
/// direct child of the orchestrator (reap via `Child::wait`). The fork-prewarm
/// path (Unix) forks workers off a zygote that then exits, reparenting them to
/// init/launchd; the orchestrator never became their parent, so it signals them
/// by pid (`kill(pid, ...)`), polls for their exit, and relies on init to reap.
pub(super) enum Proc {
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
pub(super) struct Tracked {
    pub(super) pid: u32,
    pub(super) start: Option<u64>,
}

#[cfg(unix)]
impl Tracked {
    /// Start tracking `pid`; `None` if it is gone (nothing to track). Called
    /// while the zygote is still alive and the worker's parent, so the pid can't
    /// have been recycled yet: a worker that already died is still a zombie.
    /// A zombie is tracked too when its start time is known, so whoever ends
    /// up reaping it (possibly us, as PID 1 / a subreaper) can still verify it.
    pub(super) fn capture(pid: u32) -> Option<Self> {
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
    pub(super) fn same_start(&self, start: Option<u64>) -> bool {
        self.start.is_none() || start == self.start
    }

    /// This worker's current state. A pid now naming a different process (the
    /// worker was reaped and the pid recycled) or a zombie that can't be proven
    /// to be this worker is reported [`Status::Gone`]: never signalled or reaped.
    pub(super) fn status(&self) -> Status {
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
pub(super) enum Status {
    Running,
    /// Exited, not yet reaped, and verifiably this worker.
    Zombie,
    /// Exited and reaped, or its pid now belongs to someone else.
    Gone,
}

/// What the OS reports for a pid we are not necessarily the parent of.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Probe {
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
pub(super) fn probe(pid: u32) -> Probe {
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
pub(super) fn parse_proc_stat(stat: &str) -> Option<Probe> {
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
pub(super) fn probe(pid: u32) -> Probe {
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
pub(super) fn probe(pid: u32) -> Probe {
    probe_by_signal(pid)
}

/// Fallback probe: signal 0 checks existence only (a zombie still counts as
/// present, and there is no start time to detect pid reuse).
#[cfg(unix)]
pub(super) fn probe_by_signal(pid: u32) -> Probe {
    // SAFETY: signal 0 only probes for existence; no memory access.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        Probe::Alive { start: None }
    } else {
        Probe::Gone
    }
}

/// Poll interval while waiting for a reparented worker to disappear: starts at
/// the first value and doubles up to the second, so a worker that exits within
/// a few ms of Shutdown (the common case) is noticed within ~1ms, while a slow
/// teardown still costs only a status check every 10ms.
#[cfg(unix)]
pub(super) const REPARENTED_POLL_FIRST: std::time::Duration = std::time::Duration::from_micros(500);

#[cfg(unix)]
pub(super) const REPARENTED_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Upper bound on waiting for a reparented worker to exit after `Shutdown`. A
/// worker stuck in teardown past it is SIGKILLed rather than hanging the run.
#[cfg(unix)]
pub(super) const REPARENTED_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long to wait for a SIGKILLed reparented worker to disappear. Normally
/// milliseconds; bounded so a process stuck in uninterruptible sleep can't hang
/// the run.
#[cfg(unix)]
pub(super) const REPARENTED_REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl Proc {
    /// The exit status, if the process exits within `grace`. Only an owned
    /// child has one to read; a reparented worker's belongs to init.
    pub(super) fn exit_status_within(&mut self, grace: Duration) -> Option<ExitStatus> {
        match self {
            Proc::Owned(child) => {
                let deadline = Instant::now() + grace;
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => return Some(status),
                        Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        _ => return None,
                    }
                }
            }
            #[cfg(unix)]
            Proc::Reparented(_) => None,
        }
    }

    /// Kill the process. `Child::kill` for an owned child; a SIGKILL by pid for
    /// a reparented fork-prewarmed worker (not our child, so no `Child` handle).
    pub(super) fn kill(&mut self) {
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
    pub(super) fn wait(&mut self, since: Option<std::time::Instant>) {
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
    pub(super) fn wait_or_kill(&mut self, timeout: std::time::Duration) {
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
    pub(super) fn reap(&mut self) {
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
pub(super) fn try_reap(pid: u32) -> bool {
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
pub(super) fn wait_reparented(slot: &mut Option<Tracked>, timeout: std::time::Duration) -> bool {
    let Some(tracked) = *slot else {
        return true;
    };
    let deadline = std::time::Instant::now() + timeout;
    let mut pause = REPARENTED_POLL_FIRST;
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
        std::thread::sleep(pause);
        pause = (pause * 2).min(REPARENTED_POLL);
    }
}

/// Raise the soft `RLIMIT_NOFILE` (never above the hard limit) so the zygote's
/// ~6n pipe fds fit alongside what the process already holds. Best effort: on
/// failure the fork path may still hit EMFILE and fall back to plain spawns.
#[cfg(unix)]
// `rlim_t` is u64 on Linux/macOS but signed on some BSDs.
#[allow(clippy::unnecessary_cast)]
pub(super) fn ensure_fd_headroom(n: usize) {
    /// Allowance for fds the orchestrator already holds (stdio, caches, the
    /// watcher, ...) on top of the worker pipes.
    const BASELINE: u64 = 256;
    let want = (6 * n as u64 + 4).saturating_add(BASELINE);
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
pub(super) fn is_fd_exhaustion(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| code == libc::EMFILE || code == libc::ENFILE)
    })
}
