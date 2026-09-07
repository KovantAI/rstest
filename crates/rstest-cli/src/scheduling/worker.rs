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
}

/// Transport: a pair of anonymous OS pipes per worker (POSIX pipes on unix,
/// CreatePipe handles on Windows), never stdio (D4: fd 0/1/2 stay free). The
/// child gets its endpoints as numeric argv: fds on unix, HANDLEs on Windows.
pub struct Worker {
    child: Child,
    cmd_w: File,
    reader: Option<EventReader>,
}

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

        let mut command = Command::new(python);
        command
            .args([
                "-m",
                "rstest_worker",
                &cmd.read.raw().to_string(),
                &evt.write.raw().to_string(),
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
            child,
            cmd_w,
            reader: Some(EventReader::new(evt_r)),
        })
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
        self.child.wait()?;
        Ok(())
    }

    /// Hard-kill the worker process (hang watchdog). The reader thread
    /// sees EOF and the normal crash machinery takes over.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Wait for the worker process to exit (after a Shutdown was sent).
    pub fn wait(mut self) -> Result<()> {
        self.child.wait()?;
        Ok(())
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
    //! EXPERIMENTAL: exercised by CI's windows wheel smoke test, not the full
    //! gate. Anonymous pipes; child ends made inheritable, HANDLE values passed
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
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(explicit) = std::env::var("RSTEST_WORKER_PATH") {
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
    if let Ok(existing) = std::env::var("PYTHONPATH") {
        for p in std::env::split_paths(&existing) {
            paths.push(p);
        }
    }
    std::env::join_paths(paths)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{Endpoint, LimitedReader};
    use crate::scheduling::proto;
    use serde::Deserialize;
    use std::io::{BufReader, Cursor, Read};
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

    #[test]
    fn endpoint_into_raw_takes_ownership_without_closing() {
        // A fake, never-opened raw value: raw() reads it, into_raw() hands it
        // off and defuses Drop (no close of an fd we don't own).
        let e = Endpoint::new(0xDEAD_BEEF);
        assert_eq!(e.raw(), 0xDEAD_BEEF);
        assert_eq!(e.into_raw(), 0xDEAD_BEEF);
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
}
