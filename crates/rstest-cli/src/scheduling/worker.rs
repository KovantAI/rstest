use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use anyhow::{Context, Result};

use crate::scheduling::proto;

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
    child: Child,
    cmd_w: File,
    reader: Option<EventReader>,
}

/// The read half of a worker's event pipe, split off from [`Worker`] so a
/// reader thread can own it while the orchestrator keeps the write half.
pub struct EventReader {
    events: rmp_serde::Deserializer<rmp_serde::decode::ReadReader<BufReader<File>>>,
}

impl EventReader {
    /// Block for the next msgpack [`proto::Event`] from the worker.
    pub fn recv(&mut self) -> Result<proto::Event> {
        use serde::Deserialize;
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
            child,
            cmd_w,
            reader: Some(EventReader {
                events: rmp_serde::Deserializer::new(BufReader::new(evt_r)),
            }),
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

    /// Kill (if still running) and reap the child in place. Unlike [`Worker::wait`]
    /// this borrows `&mut self`, so a state slot that must stay in the pool's vec
    /// (marked dead, awaiting end-of-run cleanup) can be reaped immediately rather
    /// than lingering as an orphan / `<defunct>` zombie until then. Idempotent: a
    /// later `wait()` on the already-reaped child returns the cached `ExitStatus`
    /// (`std::process::Child::wait` stores it on first success and never calls
    /// `waitpid` again), so the end-of-run cleanup double-wait is safe.
    pub fn reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    use super::{build_worker_command, Endpoint, Stdio, WorkerEnv};
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

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
        let Some((python, worker_path)) = worker_python() else {
            eprintln!("skipping reap test: no python with pytest found");
            return;
        };
        // Point production `worker_pythonpath()` at the repo package. SAFETY:
        // edition 2021; every worker-spawning test writes this same repo path,
        // so concurrent writes converge on one value (no divergent read). Left
        // set on exit, matching serve.rs's live-worker tests.
        std::env::set_var("RSTEST_WORKER_PATH", &worker_path);

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
        let pid = worker.child.id();
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
}
