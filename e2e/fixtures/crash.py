
import os, signal


def _hard_crash():
    sig = getattr(signal, "SIGKILL", None)
    if sig is not None:
        os.kill(os.getpid(), sig)
    os._exit(137)

def test_before_a(): assert True
def test_before_b(): assert True

def test_killer():
    _hard_crash()

def test_after_a(): assert True
def test_after_b(): assert True
def test_after_c(): assert True
