
import os
import pathlib

import pytest


@pytest.mark.flaky(reruns=2)
def test_marked_flaky():
    marker = pathlib.Path(os.environ["MK"])
    if not marker.exists():
        marker.write_text("x")
        assert False, "transient blip"
    assert True


def test_unmarked_fails():
    cnt = pathlib.Path(os.environ["CNT"])
    n = int(cnt.read_text()) if cnt.exists() else 0
    cnt.write_text(str(n + 1))
    assert False, "permanent failure"


def test_ok():
    assert True
