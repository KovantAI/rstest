"""closest_marker: the same answer as get_closest_marker, without the walk on a miss."""

from __future__ import annotations

import uuid
from types import SimpleNamespace

import pytest
from rstest_worker._internal import runner_pytest
from rstest_worker._internal.marks import closest_marker
from rstest_worker._internal.stream import StreamPlugin

MARKED_SRC = """
import pytest

pytestmark = pytest.mark.module_mark

@pytest.mark.class_mark
class TestC:
    @pytest.mark.func_mark
    @pytest.mark.parametrize("x", [pytest.param(1, marks=pytest.mark.param_mark)])
    def test_a(self, x):
        pass

@pytest.fixture
def added(request):
    request.node.add_marker(pytest.mark.setup_mark)

def test_b(added):
    pass
"""

NAMES = ("module_mark", "class_mark", "func_mark", "param_mark", "setup_mark", "absent")


def test_matches_get_closest_marker_on_real_items(tmp_path):
    name = f"test_marks_{uuid.uuid4().hex[:8]}.py"
    (tmp_path / name).write_text(MARKED_SRC)
    seen: dict[str, dict[str, bool]] = {}

    class Probe:
        @pytest.hookimpl(trylast=True)
        def pytest_runtest_call(self, item):
            for n in NAMES:
                assert closest_marker(item, n) is item.get_closest_marker(n), n
            seen[item.name] = {n: closest_marker(item, n) is not None for n in NAMES}

    status = runner_pytest._pytest_main(
        [str(tmp_path / name), "-q", "-p", "no:cacheprovider", "-p", "no:randomly"],
        plugins=[Probe()],
    )
    assert int(status) == 0
    assert seen["test_a[1]"] == {
        "module_mark": True,
        "class_mark": True,
        "func_mark": True,
        "param_mark": True,
        "setup_mark": False,
        "absent": False,
    }
    assert seen["test_b"]["setup_mark"] is True, "a marker added during setup"
    assert seen["test_b"]["absent"] is False


def test_unmarked_timeout_lookup_skips_the_marker_walk(monkeypatch):
    monkeypatch.delenv("RSTEST_TIMEOUT", raising=False)
    monkeypatch.delenv("RSTEST_DOCTOR", raising=False)

    def walk(name):
        raise AssertionError(f"walked the node chain for {name!r}")

    item = SimpleNamespace(nodeid="t.py::t", keywords={"t", "t.py"}, get_closest_marker=walk)
    assert StreamPlugin(SimpleNamespace(send=lambda *a: None))._effective_timeout(item) is None
