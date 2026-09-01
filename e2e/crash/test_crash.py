import os
import signal


def test_before_a():
    assert True


def test_before_b():
    assert True


def test_killer():
    # simulate a segfaulting C extension: hard-kill the worker process
    os.kill(os.getpid(), signal.SIGKILL)


def test_after_a():
    assert True


def test_after_b():
    assert True


def test_after_c():
    assert True
