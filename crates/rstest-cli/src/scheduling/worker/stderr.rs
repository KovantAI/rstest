//! Worker stderr: the bounded tail kept for crash reports, the startup
//! stderr dedup across workers, and the forwarding thread.

use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::ExitStatus;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::scheduling::proto;

use super::DEATH_GRACE;

/// Stderr lines kept per worker for [`super::Worker::death_detail`].
pub(super) const STDERR_TAIL_LINES: usize = 8;

/// Longest stderr line kept, in bytes (cut on a character boundary).
pub(super) const STDERR_TAIL_LINE_BYTES: usize = 500;

/// A worker's stderr, forwarded live to rstest's own stderr by a thread that
/// also keeps the last [`STDERR_TAIL_LINES`] lines. The worker used to inherit
/// stderr directly; forwarding keeps that visibility, and the tail lets a dead
/// worker's failure name its error (a `ModuleNotFoundError` at startup, a
/// fatal Python error) instead of only "terminated unexpectedly".
pub(super) struct StderrTail {
    pub(super) lines: Arc<Mutex<VecDeque<String>>>,
    /// Set once the pipe hits EOF (the worker and anything that inherited its
    /// stderr are gone).
    pub(super) done: Arc<(Mutex<bool>, Condvar)>,
    /// Pool workers only: stderr held back until the worker starts running
    /// tests (see [`StartupStderr`]).
    pub(super) startup: Option<Arc<StartupStderr>>,
}

impl StderrTail {
    pub(super) fn capture(
        stderr: impl Read + Send + 'static,
        startup: Option<Arc<StartupStderr>>,
    ) -> Self {
        let lines = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let (t_lines, t_done) = (Arc::clone(&lines), Arc::clone(&done));
        let t_startup = startup.clone();
        let spawned = std::thread::Builder::new()
            .name("rstest-worker-stderr".into())
            .spawn(move || {
                forward_stderr(
                    BufReader::new(stderr),
                    &mut io::stderr(),
                    &t_lines,
                    t_startup.as_deref(),
                );
                let (flag, cv) = &*t_done;
                *flag.lock().unwrap_or_else(|p| p.into_inner()) = true;
                cv.notify_all();
            });
        if spawned.is_err() {
            // No thread, no reader: mark done so snapshot never waits. The
            // worker then writes into a pipe nobody drains, which only blocks
            // it once the pipe buffer fills.
            *done.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
        }
        StderrTail {
            lines,
            done,
            startup,
        }
    }

    /// The kept lines, after waiting up to `wait` for the pipe to reach EOF.
    pub(super) fn snapshot(&self, wait: Duration) -> Vec<String> {
        let (flag, cv) = &*self.done;
        let guard = flag.lock().unwrap_or_else(|p| p.into_inner());
        let _ = cv
            .wait_timeout_while(guard, wait, |done| !*done)
            .unwrap_or_else(|p| p.into_inner());
        let lines = self.lines.lock().unwrap_or_else(|p| p.into_inner());
        lines.iter().cloned().collect()
    }
}

impl Drop for StderrTail {
    /// A worker that never got past startup (a usage error, a conftest that
    /// fails to import, a nodeid that matches nothing) still holds its stderr:
    /// let the pipe drain, then print it, once per distinct text across the
    /// pool, before the run's summary.
    fn drop(&mut self) {
        let Some(startup) = &self.startup else { return };
        if !startup.holding() {
            return;
        }
        let _ = self.snapshot(DEATH_GRACE);
        startup.finish(&mut io::stderr());
    }
}

/// The stderr pipes of one pool's workers share one of these: the startup
/// texts already printed.
pub(crate) type SeenStartupStderr = Arc<Mutex<HashSet<Vec<u8>>>>;

/// Most stderr bytes held back per worker during startup; past this it is
/// printed and forwarding goes live.
pub(super) const STARTUP_HOLD_BYTES: usize = 1 << 20;

/// A pool worker's stderr from spawn until it starts running tests. Every
/// worker of a pool runs the same session, so a startup failure (a usage
/// error, an unknown flag, a missing path or nodeid, a conftest ImportError)
/// prints the same text from each of them. Held back, the identical copies
/// are printed once when the workers exit, while a worker whose text differs
/// still prints its own. Once an event shows the worker running tests
/// ([`StartupStderr::ends_startup`]), the held bytes are printed verbatim and
/// everything after is forwarded live and never deduplicated, so test output
/// is untouched.
pub(crate) struct StartupStderr {
    /// `Some` while startup output is being held back.
    pub(super) held: Mutex<Option<Vec<u8>>>,
    pub(super) seen: SeenStartupStderr,
}

impl StartupStderr {
    pub(super) fn new(seen: SeenStartupStderr) -> Self {
        StartupStderr {
            held: Mutex::new(Some(Vec::new())),
            seen,
        }
    }

    /// Whether `ev` shows the worker past startup: it collected a file (lazy)
    /// or is running items.
    pub(super) fn ends_startup(ev: &proto::Event) -> bool {
        matches!(
            ev,
            proto::Event::Report(_)
                | proto::Event::ItemStart { .. }
                | proto::Event::ItemDone { .. }
                | proto::Event::ItemStartId { .. }
                | proto::Event::ItemDoneId { .. }
                | proto::Event::FileCollected { .. }
        )
    }

    pub(super) fn lock_held(&self) -> std::sync::MutexGuard<'_, Option<Vec<u8>>> {
        self.held.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn holding(&self) -> bool {
        self.lock_held().is_some()
    }

    /// Forward one line: held while starting up (until the cap), else written.
    pub(super) fn forward(&self, line: &[u8], to: &mut impl Write) {
        let mut held = self.lock_held();
        let Some(buf) = held.as_mut() else {
            let _ = to.write_all(line);
            let _ = to.flush();
            return;
        };
        buf.extend_from_slice(line);
        if buf.len() > STARTUP_HOLD_BYTES {
            let buf = held.take().unwrap_or_default();
            let _ = to.write_all(&buf);
            let _ = to.flush();
        }
    }

    /// The worker is running tests: print what was held, verbatim, and go
    /// live.
    pub(super) fn release(&self, to: &mut impl Write) {
        let mut held = self.lock_held();
        if let Some(buf) = held.take() {
            if !buf.is_empty() {
                let _ = to.write_all(&buf);
                let _ = to.flush();
            }
        }
    }

    /// The worker is gone without running tests: print what it held unless
    /// another worker of the pool already printed the same text.
    pub(super) fn finish(&self, to: &mut impl Write) {
        let mut held = self.lock_held();
        let Some(buf) = held.take() else { return };
        if buf.is_empty() {
            return;
        }
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        if seen.contains(&buf) {
            return;
        }
        let _ = to.write_all(&buf);
        let _ = to.flush();
        seen.insert(buf);
    }
}

/// Copy `from` to `to` line by line (bytes untouched), keeping the last
/// non-blank lines in `tail`. Returns at EOF or a read error. With `startup`,
/// lines go through it instead (held back while the worker starts up).
pub(super) fn forward_stderr(
    mut from: impl BufRead,
    to: &mut impl Write,
    tail: &Mutex<VecDeque<String>>,
    startup: Option<&StartupStderr>,
) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match from.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => {
                if let Some(startup) = startup {
                    startup.finish(to);
                }
                return;
            }
            Ok(_) => {}
        }
        match startup {
            Some(startup) => startup.forward(&buf, to),
            None => {
                let _ = to.write_all(&buf);
                let _ = to.flush();
            }
        }
        let mut line = String::from_utf8_lossy(&buf).trim_end().to_string();
        if line.trim().is_empty() {
            continue;
        }
        crate::text::truncate_on_boundary(&mut line, STDERR_TAIL_LINE_BYTES);
        let mut tail = tail.lock().unwrap_or_else(|p| p.into_inner());
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

/// Render [`super::Worker::death_detail`]: an indented exit line and stderr tail.
pub(super) fn format_death_detail(
    status: Option<ExitStatus>,
    tail: &[String],
    killed_by_us: bool,
) -> String {
    use std::fmt::Write as _;
    #[cfg(not(unix))]
    let _ = killed_by_us;
    let mut s = String::new();
    if let Some(status) = status {
        match status.code() {
            Some(code) => {
                let _ = write!(s, "\n  exited with code {code}");
            }
            None => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(sig) = status.signal() {
                        let _ = write!(s, "\n  killed by signal {sig}");
                        // Nobody here sent it: on Linux the OOM killer and on
                        // macOS jetsam end a process that way.
                        if sig == libc::SIGKILL && !killed_by_us {
                            s.push_str(
                                " (SIGKILL: likely the out-of-memory killer; lower -n, or \
                                 let -n auto size the pool by memory)",
                            );
                        }
                    }
                }
            }
        }
    }
    if !tail.is_empty() {
        s.push_str("\n  last lines of its stderr:");
        for line in tail {
            let _ = write!(s, "\n    {line}");
        }
    }
    s
}
