//! One lock for child spawns that can race across threads.
//!
//! The run stamp's `git` calls run on a thread while the workers spawn. On
//! macOS, std creates a child's stdout pipe with `pipe()` and only then marks
//! it CLOEXEC, so a worker forked in between inherits git's stdout write end
//! and holds it open for life: the stamp thread never sees EOF and the run
//! hangs (the worker waits for a command the stamp gates). Holding this lock
//! across both kinds of spawn closes that window.

use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};

/// Held while spawning a worker or a [`output_locked`] child.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// [`Command::output`], spawned under [`lock`] and waited on outside it.
pub(crate) fn output_locked(cmd: &mut Command) -> std::io::Result<Output> {
    let child = {
        let _held = lock();
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?
    };
    child.wait_with_output()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A child spawned while another thread captures output must not inherit
    /// that capture's pipe: if it did, the capture would block until the child
    /// exits. Without the lock on macOS, eight threads of captures hit the window
    /// and one stalls for the child's full half second.
    #[test]
    fn captured_output_never_leaks_into_a_concurrent_spawn() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let spawner = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut kids: Vec<std::process::Child> = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    kids.retain_mut(|k| k.try_wait().ok().flatten().is_none());
                    let kid = {
                        let _held = lock();
                        Command::new("sleep")
                            .arg("0.5")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                            .unwrap()
                    };
                    kids.push(kid);
                }
                kids
            })
        };
        // Several capturing threads keep one inside std's pipe window often.
        let capturers: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    let mut slowest = Duration::ZERO;
                    for _ in 0..200 {
                        let t = Instant::now();
                        let out = output_locked(Command::new("echo").arg("x")).unwrap();
                        assert!(out.status.success());
                        slowest = slowest.max(t.elapsed());
                    }
                    slowest
                })
            })
            .collect();
        let slowest = capturers
            .into_iter()
            .map(|c| c.join().unwrap())
            .max()
            .unwrap();
        stop.store(true, Ordering::Relaxed);
        for mut k in spawner.join().unwrap() {
            let _ = k.kill();
            let _ = k.wait();
        }
        assert!(
            slowest < Duration::from_millis(400),
            "a capture waited {slowest:?}: its pipe leaked into a concurrent spawn"
        );
    }
}
