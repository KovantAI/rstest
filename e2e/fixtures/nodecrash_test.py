
import os, signal


def _hard_crash():
    sig = getattr(signal, "SIGKILL", None)
    if sig is not None:
        os.kill(os.getpid(), sig)
    os._exit(137)

import time


def test_a(): time.sleep(0.05)
def test_b(): time.sleep(0.05)


def test_killer():
    _hard_crash()


def test_c(): time.sleep(0.05)
def test_d(): time.sleep(0.05)
def test_e(): time.sleep(0.05)
