
import os, signal


def _hard_crash():
    sig = getattr(signal, "SIGKILL", None)
    if sig is not None:
        os.kill(os.getpid(), sig)
    os._exit(137)

import pathlib


def test_crashes_once():
    marker = pathlib.Path(os.environ["FLAKY_MARKER"])
    if not marker.exists():
        marker.write_text("crashed")
        _hard_crash()
    assert True


def test_other():
    assert True
