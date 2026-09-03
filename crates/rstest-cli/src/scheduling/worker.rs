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

pub struct EventReader {
    events: rmp_serde::Deserializer<rmp_serde::decode::ReadReader<BufReader<File>>>,
}

impl EventReader {
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
    Null,
    Inherit,
}

impl Worker {
    pub fn spawn(python: &Path, worker: Option<(usize, usize)>, env: &WorkerEnv) -> Result<Self> {
        Self::spawn_with_io(python, worker, Stdio::Null, env)
    }

    pub fn spawn_with_io(
        python: &Path,
        worker: Option<(usize, usize)>,
        io: Stdio,
        env: &WorkerEnv,
    ) -> Result<Self> {
        // cmd: parent writes -> child reads; evt: child writes -> parent reads.
        let cmd = transport::pipe()?;
        let evt = transport::pipe()?;
        transport::prepare_parent_end(cmd.write)?;
        transport::prepare_parent_end(evt.read)?;
        transport::prepare_child_end(cmd.read)?;
        transport::prepare_child_end(evt.write)?;

        let mut command = Command::new(python);
        command
            .args([
                "-m",
                "rstest_worker",
                &cmd.read.to_string(),
                &evt.write.to_string(),
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

        // Close the child's ends in the parent or EOF detection breaks.
        transport::close(cmd.read);
        transport::close(evt.write);
        let cmd_w = transport::into_file(cmd.write);
        let evt_r = transport::into_file(evt.read);
        Ok(Self {
            child,
            cmd_w,
            reader: Some(EventReader {
                events: rmp_serde::Deserializer::new(BufReader::new(evt_r)),
            }),
        })
    }

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

    pub fn recv(&mut self) -> Result<proto::Event> {
        self.reader
            .as_mut()
            .context("event reader was detached; recv is unavailable after take_reader")?
            .recv()
    }

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

/// Endpoint values are numeric and platform-meaningful: file descriptors
/// on unix, HANDLEs on Windows.
struct Pipe {
    read: u64,
    write: u64,
}

#[cfg(unix)]
mod transport {
    use std::fs::File;
    use std::os::fd::FromRawFd;

    use anyhow::{Context, Result};

    use super::Pipe;

    pub fn pipe() -> Result<Pipe> {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("pipe()");
        }
        Ok(Pipe {
            read: fds[0] as u64,
            write: fds[1] as u64,
        })
    }

    /// Parent ends must not leak into the child (CLOEXEC) or EOF
    /// detection breaks.
    pub fn prepare_parent_end(fd: u64) -> Result<()> {
        let fd = fd as i32;
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl(FD_CLOEXEC)");
        }
        Ok(())
    }

    /// Child ends are inherited as-is on unix (pipe() fds are not CLOEXEC).
    pub fn prepare_child_end(_fd: u64) -> Result<()> {
        Ok(())
    }

    pub fn close(fd: u64) {
        unsafe { libc::close(fd as i32) };
    }

    pub fn into_file(fd: u64) -> File {
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
        let ok = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) };
        if ok == 0 {
            bail!("CreatePipe failed: {}", std::io::Error::last_os_error());
        }
        Ok(Pipe {
            read: read as u64,
            write: write as u64,
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
        unsafe { CloseHandle(handle as HANDLE) };
    }

    pub fn into_file(handle: u64) -> File {
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
