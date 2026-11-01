"""`@pytest.mark.isolated`: the test runs in a forked child, its state dies
with it, and fixture setup/teardown stays balanced across the fork."""

from __future__ import annotations

import os
import signal
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
import signal
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
def test_4b_killed():
    os.kill(os.getpid(), signal.SIGKILL)


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
    outcome, text = seen[("test_4b_killed", "call")]
    assert outcome == "failed"
    assert f"killed by signal {signal.SIGKILL}" in text


def test_without_fork_runs_in_process_and_warns_once(tmp_path, monkeypatch):
    monkeypatch.delattr(os, "fork")
    name = f"test_nofork_{uuid.uuid4().hex[:8]}"
    (tmp_path / f"{name}.py").write_text(
        "import os, pytest\n"
        f"PARENT = {os.getpid()}\n"
        "@pytest.mark.isolated\n"
        "def test_a():\n    assert os.getpid() == PARENT\n"
        "@pytest.mark.isolated\n"
        "def test_b():\n    assert os.getpid() == PARENT\n"
    )
    monkeypatch.syspath_prepend(str(tmp_path))
    warned: list[str] = []

    class Recorder:
        def pytest_warning_recorded(self, warning_message):
            warned.append(str(warning_message.message))

    rc = runner_pytest._pytest_main(
        [str(tmp_path / f"{name}.py"), "-q", "-p", "no:cacheprovider", "-p", "no:randomly"],
        plugins=[IsolatedTests(), Recorder()],
    )
    sys.modules.pop(name, None)
    # Both ran in this process (the pid asserts passed), warned about once.
    assert rc == 0
    assert len([w for w in warned if "needs fork" in w]) == 1


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


def test_parent_teardown_error_is_the_isolated_tests_teardown_report(tmp_path, monkeypatch):
    # A parent-owned module fixture whose teardown raises, torn down after the
    # isolated test (last in its module): the parent reports it as that test's
    # teardown error, after the child's own reports.
    name = f"test_isotd_{uuid.uuid4().hex[:8]}"
    (tmp_path / f"{name}.py").write_text(
        "import pytest\n"
        "@pytest.fixture(scope='module')\n"
        "def shared():\n"
        "    yield\n"
        "    raise RuntimeError('parent teardown boom')\n"
        "def test_a(shared):\n    pass\n"
        "@pytest.mark.isolated\n"
        "def test_b(shared):\n    pass\n"
    )
    monkeypatch.syspath_prepend(str(tmp_path))
    seen: list[tuple[str, str, str, str]] = []

    class Recorder:
        def pytest_runtest_logreport(self, report):
            test = report.nodeid.split("::", 1)[1]
            seen.append((test, report.when, report.outcome, str(report.longrepr or "")))

    runner_pytest._pytest_main(
        [str(tmp_path / f"{name}.py"), "-q", "-p", "no:cacheprovider", "-p", "no:randomly"],
        plugins=[IsolatedTests(), Recorder()],
    )
    sys.modules.pop(name, None)
    b = [(when, outcome) for test, when, outcome, _ in seen if test == "test_b"]
    assert b == [
        ("setup", "passed"),
        ("call", "passed"),
        ("teardown", "passed"),
        ("teardown", "failed"),
    ]
    assert "parent teardown boom" in seen[-1][3]


class _State:
    def __init__(self, stack=None):
        self.stack = stack if stack is not None else {}
        self.torn: list = []

    def teardown_exact(self, nextitem):
        self.torn.append(nextitem)


def test_parent_teardown_tears_everything_down_when_the_session_stops():
    # As pytest's runner: a stopping session tears down every scope (nextitem
    # None), else only what the next test does not share.
    from types import SimpleNamespace

    from rstest_worker._internal.isolate import _parent_teardown

    nxt = object()
    for shouldfail, shouldstop, expected in (
        (False, False, nxt),
        ("maxfail", False, None),
        (False, "interrupted", None),
    ):
        state = _State()
        session = SimpleNamespace(_setupstate=state, shouldfail=shouldfail, shouldstop=shouldstop)
        assert _parent_teardown(SimpleNamespace(session=session), nxt) is None
        assert state.torn == [expected]


def test_hand_back_new_finalizers_moves_post_fork_ones_onto_the_childs_node():
    from rstest_worker._internal.isolate import _hand_back_new_finalizers

    session, module, item = object(), object(), object()
    state = _State(
        {
            session: (["s0", "s1_new"], None),
            module: (["m0"], None),
            item: (["i0"], None),
        }
    )
    _hand_back_new_finalizers(state, {session: 1, module: 1})
    # The parent keeps exactly what it had at the fork...
    assert state.stack[session][0] == ["s0"]
    assert state.stack[module][0] == ["m0"]
    # ...and the child's first node runs the handed-back ones last (LIFO).
    assert state.stack[item][0] == ["s1_new", "i0"]


def test_hand_back_new_finalizers_without_child_nodes_leaves_the_stack():
    from rstest_worker._internal.isolate import _hand_back_new_finalizers

    session = object()
    state = _State({session: (["s0", "s1_new"], None)})
    _hand_back_new_finalizers(state, {session: 1})
    assert state.stack[session][0] == ["s0", "s1_new"]


class _Exit(BaseException):
    def __init__(self, code):
        self.code = code


def _run_child_inline(monkeypatch, item):
    """Run `_run_in_child` in this process: os._exit raises instead of
    leaving, so the exit code and what reached the pipe can be checked."""
    import json

    from rstest_worker._internal import isolate

    def fake_exit(code):
        raise _Exit(code)

    monkeypatch.setattr(isolate.os, "_exit", fake_exit)
    r, w = os.pipe()
    with pytest.raises(_Exit) as exited:
        isolate._run_in_child(item, None, w)
    os.close(w)
    with os.fdopen(r, "rb") as f:
        data = f.read()
    return exited.value.code, (json.loads(data) if data else None)


def test_run_in_child_writes_serialized_reports_and_tears_down_only_its_own(monkeypatch):
    from types import SimpleNamespace

    import _pytest.runner

    parent_node, child_node = object(), object()
    state = _State({parent_node: (["p0"], None)})
    original_teardown = state.teardown_exact

    def fake_protocol(item, log, nextitem):
        assert log is False
        # The protocol's teardown goes through the child's own teardown.
        state.stack[child_node] = (["c0"], None)
        state.stack[parent_node][0].append("p1_new")
        item.session._setupstate.teardown_exact(nextitem)
        return ["r1", "r2"]

    monkeypatch.setattr(_pytest.runner, "runtestprotocol", fake_protocol)
    hook = SimpleNamespace(pytest_report_to_serializable=lambda config, report: {"r": report})
    item = SimpleNamespace(
        session=SimpleNamespace(_setupstate=state), config=SimpleNamespace(hook=hook)
    )
    code, data = _run_child_inline(monkeypatch, item)
    assert code == 0
    assert data == [{"r": "r1"}, {"r": "r2"}]
    # Torn down to the parent's fork-time chain, never past it, with the
    # post-fork finalizer handed to the child's node.
    assert state.teardown_exact is not original_teardown
    assert [c.listchain() for c in state.torn] == [[parent_node]]
    assert state.stack[parent_node][0] == ["p0"]
    assert state.stack[child_node][0] == ["p1_new", "c0"]


def test_run_in_child_exits_1_without_reports_when_anything_raises(monkeypatch):
    from types import SimpleNamespace

    code, data = _run_child_inline(monkeypatch, SimpleNamespace(session=None))
    assert code == 1
    assert data is None
