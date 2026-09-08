"""Fast unit tests (minimal wait). Fill out the suite so the slow file isn't the
only work; each is trivial, standing in for ordinary quick unit tests."""

import time

import pytest


@pytest.mark.parametrize("i", range(30))
def test_fast_b(i):
    time.sleep(0.003)
    assert i == i
