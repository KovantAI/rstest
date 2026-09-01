
import time


def test_quick_a():
    assert True


def test_hangs_forever():
    time.sleep(600)


def test_quick_b():
    assert True
