
import os
import pathlib


def test_flaky_once():
    marker = pathlib.Path(os.environ["FLAKY_MARKER"])
    if not marker.exists():
        marker.write_text("attempted")
        assert False, "first attempt fails"
    assert True


def test_stable():
    assert True
