
# Ten hard-crashers, far more than the restart budget (n.max(4)). Once
# restarts_left hits 0 a further crash is NON-restartable: the worker is
# recorded as an internal error (exit 3) via collect_error, not respawned.
import os
import signal


def _hard_crash():
    sig = getattr(signal, "SIGKILL", None)
    if sig is not None:
        os.kill(os.getpid(), sig)
    os._exit(137)


def test_k01(): _hard_crash()
def test_k02(): _hard_crash()
def test_k03(): _hard_crash()
def test_k04(): _hard_crash()
def test_k05(): _hard_crash()
def test_k06(): _hard_crash()
def test_k07(): _hard_crash()
def test_k08(): _hard_crash()
def test_k09(): _hard_crash()
def test_k10(): _hard_crash()
