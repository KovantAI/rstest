"""`@pytest.mark.isolated`: the test runs in a forked child, its state dies
with it, and fixture setup/teardown stays balanced across the fork."""

from __future__ import annotations

import os
import sys
import uuid

import pytest
from rstest_worker._internal import runner_pytest
from rstest_worker._internal.isolate import IsolatedTests

pytestmark = pytest.mark.skipif(not hasattr(os, "fork"), reason="isolation needs fork()")

CONFTEST = """
import os
import pytest

LOG = os.environ["ISO_LOG"]


def log(msg):
    with open(LOG, "a") as f:
        f.write(f"{os.getpid()} {msg}\\n")


@pytest.fixture(scope="module")
def parent_owned():
    log("setup parent_owned")
    yield
    log("teardown parent_owned")


@pytest.fixture(scope="module")
def child_first():
    log("setup child_first")
    yield
    log("teardown child_first")
"""

SRC = """
import os
import sys
import pytest

STATE = {"polluted": False}


def test_1_before(parent_owned):
    assert not STATE["polluted"]


@pytest.mark.isolated
def test_2_polluter(parent_owned, child_first):
    STATE["polluted"] = True
    os.environ["RSTEST_ISO_POLLUTED"] = "1"
    sys.modules["fake_polluter_mod"] = object()


def test_3_after(parent_owned, child_first):
    assert not STATE["polluted"]
    assert "RSTEST_ISO_POLLUTED" not in os.environ
    assert "fake_polluter_mod" not in sys.modules


@pytest.mark.isolated
def test_4_crash():
    os._exit(3)


@pytest.mark.isolated
def test_5_fails():
    assert 1 == 2


@pytest.mark.isolated
def test_6_last_in_module(parent_owned):
    assert os.getpid() != PARENT


PARENT = os.getpid()
"""


def _session(tmp_path, monkeypatch):
    """Run SRC in-process with the isolation plugin; returns (outcomes by
    (test, phase), the fixture event log as (pid, event) pairs)."""
    name = f"test_iso_{uuid.uuid4().hex[:8]}"
    (tmp_path / "conftest.py").write_text(CONFTEST)
    (tmp_path / f"{name}.py").write_text(SRC)
    log = tmp_path / "events.log"
    monkeypatch.setenv("ISO_LOG", str(log))
    monkeypatch.syspath_prepend(str(tmp_path))
    seen: dict[tuple[str, str], tuple[str, str]] = {}

    class Recorder:
        def pytest_runtest_logreport(self, report):
            test = report.nodeid.split("::", 1)[1]
            seen[(test, report.when)] = (report.outcome, str(report.longrepr or ""))

    runner_pytest._pytest_main(
        [str(tmp_path / f"{name}.py"), "-q", "-p", "no:cacheprovider", "-p", "no:randomly"],
        plugins=[IsolatedTests(), Recorder()],
    )
    sys.modules.pop(name, None)
    sys.modules.pop("conftest", None)
    events = [tuple(line.split(" ", 1)) for line in log.read_text().splitlines()]
    return seen, events


def test_isolated_state_dies_with_the_child(tmp_path, monkeypatch):
    seen, _ = _session(tmp_path, monkeypatch)
    assert seen[("test_2_polluter", "call")][0] == "passed"
    # The polluter's module global, env var and sys.modules entry never
    # reached the next test, which runs in the parent.
    assert seen[("test_3_after", "call")][0] == "passed"
    assert seen[("test_6_last_in_module", "call")][0] == "passed"


def test_failures_and_crashes_come_back_as_reports(tmp_path, monkeypatch):
    seen, _ = _session(tmp_path, monkeypatch)
    assert seen[("test_5_fails", "call")][0] == "failed"
    outcome, text = seen[("test_4_crash", "call")]
    assert outcome == "failed"
    assert "isolated test crashed: its child process exited with code 3" in text


def test_fixtures_are_finalized_once_by_their_owner(tmp_path, monkeypatch):
    _, events = _session(tmp_path, monkeypatch)
    me = str(os.getpid())

    def where(event):
        return [pid == me for pid, e in events if e == event]

    # Set up by the parent for test_1, torn down once, by the parent.
    assert where("setup parent_owned") == [True]
    assert where("teardown parent_owned") == [True]
    # First used by the isolated test: set up and torn down in its child,
    # then again by the parent for test_3.
    assert where("setup child_first") == [False, True]
    assert where("teardown child_first") == [False, True]
