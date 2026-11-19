//! The event/command pipes between orchestrator and worker: owned pipe
//! endpoints and the per-platform pipe primitives.

/// RAII owner for one raw pipe endpoint (a file descriptor on unix, a HANDLE
/// on Windows). Closes on drop so an early return between `transport::pipe()`
/// and a successful `spawn()` can't leak the endpoint. Call [`Endpoint::into_raw`]
/// to defuse it when ownership is deliberately handed off (the child inherits
/// it, or it becomes a parent-side `File`).
pub(super) struct Endpoint(pub(super) Option<u64>);

impl Endpoint {
    pub(super) fn new(raw: u64) -> Self {
        Endpoint(Some(raw))
    }

    /// The raw value, without giving up ownership (for fcntl/argv/etc.).
    pub(super) fn raw(&self) -> u64 {
        self.0.expect("endpoint used after into_raw")
    }

    /// Take ownership of the raw value; drop no longer closes it.
    pub(super) fn into_raw(mut self) -> u64 {
        self.0.take().expect("endpoint already taken")
    }

    /// Take the raw value out (defusing Drop) without closing it, via `&mut`
    /// so it works on an endpoint still living inside a `Vec<Pipe>`. Used when
    /// the endpoint is being handed to a `File` that will own the close.
    #[cfg(unix)]
    pub(super) fn take_raw(&mut self) -> u64 {
        self.0.take().expect("endpoint already taken")
    }

    /// Close the endpoint now and defuse Drop, via `&mut` (for endpoints living
    /// in a `Vec<Pipe>`, where the by-value [`Endpoint::into_raw`] can't move
    /// out). A no-op if already taken.
    #[cfg(unix)]
    pub(super) fn close_now(&mut self) {
        if let Some(raw) = self.0.take() {
            sys::close(raw);
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Some(raw) = self.0.take() {
            sys::close(raw);
        }
    }
}

/// Endpoint values are numeric and platform-meaningful: file descriptors
/// on unix, HANDLEs on Windows.
pub(super) struct Pipe {
    pub(super) read: Endpoint,
    pub(super) write: Endpoint,
}

#[cfg(unix)]
mod sys {
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
mod sys {
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

pub(super) use sys::*;
