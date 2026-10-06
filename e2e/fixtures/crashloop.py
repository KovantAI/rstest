
import os, signal


def _hard_crash():
    sig = getattr(signal, "SIGKILL", None)
    if sig is not None:
        os.kill(os.getpid(), sig)
    os._exit(137)

def test_k1(): _hard_crash()
def test_k2(): _hard_crash()
def test_k3(): _hard_crash()
def test_k4(): _hard_crash()
def test_k5(): _hard_crash()
def test_k6(): _hard_crash()
def test_ok(): assert True
