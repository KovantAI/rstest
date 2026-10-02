//! SIGINT/SIGTERM during a parallel run. With the default action the
//! orchestrator dies on the spot: no summary, no journal, no reports, and its
//! workers keep running, reparented to init. A CI job killed by its timeout
//! then leaves nothing to `rstest replay` and a hung interpreter behind.
//!
//! [`Guard::install`] swaps in a handler that only records the signal; the pool
//! loops poll [`requested`] on every event and idle tick (at most 500ms late),
//! then stop the workers, name what each was running and fall through to the
//! normal wind-down, so the journal and reports are still written. A second
//! signal exits at once, as the default action would. The guard restores the
//! previous handlers when the pool returns, so `--watch`'s Ctrl+C between runs
//! still quits. No-op off Unix.

use std::sync::atomic::{AtomicI32, Ordering};

/// The first signal caught since the last [`Guard::install`], 0 for none.
static CAUGHT: AtomicI32 = AtomicI32::new(0);

/// The signal that asked the run to stop, if one arrived.
pub(crate) fn requested() -> Option<i32> {
    match CAUGHT.load(Ordering::SeqCst) {
        0 => None,
        sig => Some(sig),
    }
}

/// The signal's name, for the "interrupted by" line.
pub(crate) fn name(sig: i32) -> String {
    #[cfg(unix)]
    {
        if sig == libc::SIGINT {
            return "SIGINT".into();
        }
        if sig == libc::SIGTERM {
            return "SIGTERM".into();
        }
    }
    format!("signal {sig}")
}

/// Holds the interrupt handlers for one pool run; the previous ones come back
/// on drop.
pub(crate) struct Guard {
    #[cfg(unix)]
    prev: Vec<(libc::c_int, libc::sigaction)>,
}

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    if CAUGHT
        .compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // The second signal: the user insists. Exit now, as the default
        // action would have. `_exit` is async-signal-safe; `exit` is not.
        // SAFETY: `_exit` touches no Rust state and never returns.
        unsafe { libc::_exit(128 + sig) };
    }
}

impl Guard {
    /// Catch SIGINT and SIGTERM until dropped, clearing any signal left over
    /// from an earlier run. A signal the parent set to ignore (`nohup`, a
    /// background job's SIGINT) stays ignored.
    pub(crate) fn install() -> Self {
        CAUGHT.store(0, Ordering::SeqCst);
        #[cfg(unix)]
        {
            let mut prev = Vec::new();
            for sig in [libc::SIGINT, libc::SIGTERM] {
                // SAFETY: all-zero is a valid `sigaction` (plain data), and
                // `sigaction` only reads `act` and writes `old`, both live.
                unsafe {
                    let mut old: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(sig, std::ptr::null(), &mut old) != 0
                        || old.sa_sigaction == libc::SIG_IGN
                    {
                        continue;
                    }
                    let mut act: libc::sigaction = std::mem::zeroed();
                    act.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                    // Restart interrupted syscalls: the worker reader threads
                    // block in pipe reads that must not fail with EINTR.
                    act.sa_flags = libc::SA_RESTART;
                    libc::sigemptyset(&mut act.sa_mask);
                    if libc::sigaction(sig, &act, &mut old) == 0 {
                        prev.push((sig, old));
                    }
                }
            }
            Guard { prev }
        }
        #[cfg(not(unix))]
        Guard {}
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(unix)]
        for (sig, old) in &self.prev {
            // SAFETY: restores a `sigaction` the kernel handed back above.
            unsafe {
                libc::sigaction(*sig, old, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn names_the_two_caught_signals() {
        assert_eq!(name(libc::SIGINT), "SIGINT");
        assert_eq!(name(libc::SIGTERM), "SIGTERM");
        assert_eq!(name(99), "signal 99");
    }
}
