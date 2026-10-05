"""Unit tests for the resource-leak instrumentation (`--doctor` / `--fail-on-leak`).

Covers `LeakTracker` (identity-based attribution: the setup/teardown window,
the first-test warm-up skip, wider-scoped fixture exclusion) and the teardown
report payload StreamPlugin builds from it. The worker subprocess that runs
this code under the e2e gate isn't seen by pytest-cov, so these direct tests
are what give the leak path Python coverage.
"""

import os
import threading
from types import SimpleNamespace

from rstest_worker._internal import leakcheck
from rstest_worker._internal.leakcheck import LeakTracker, live_threads, open_fds
from rstest_worker._internal.stream import StreamPlugin


class FakeConn:
    def __init__(self):
        self.sent = []

    def send(self, kind, payload):
        self.sent.append((kind, payload))


def _item(nodeid):
    return SimpleNamespace(nodeid=nodeid)


def _report(nodeid, when):
    # Minimal duck-typed report for pytest_runtest_logreport. No `wasxfail`
    # attribute so hasattr(...) is False; clean (not failed/skipped).
    return SimpleNamespace(
        nodeid=nodeid,
        when=when,
        outcome="passed",
        duration=0.1,
        longreprtext="",
        location=("t.py", 1, "t"),
        failed=False,
        skipped=False,
        sections=[],
        longrepr=None,
    )


def _wrap(gen, body=None):
    """Drive a wrapper=True hook generator: run up to the yield, run `body`
    (the hooked phase), then resume to completion and return its value."""
    next(gen)
    if body is not None:
        body()
    try:
        gen.send(None)
    except StopIteration as e:
        return e.value
    raise AssertionError("wrapper hook did not stop")


def _fixture(scope):
    return SimpleNamespace(scope=scope, argname="fx")


class World:
    """Scripted resources: `threads` / `fds` are the live sets the tracker sees."""

    def __init__(self, monkeypatch, fds=True):
        self.threads = {"main"}
        self.fds = {(0, 1, 1), (1, 1, 2)}
        self.fds_readable = fds
        monkeypatch.setattr(leakcheck, "live_threads", lambda: frozenset(self.threads))
        monkeypatch.setattr(
            leakcheck, "open_fds", lambda: frozenset(self.fds) if self.fds_readable else None
        )


def _test(tr, nodeid, setup=None, call=None, teardown=None):
    """One test protocol through the tracker: setup / call / teardown bodies."""
    _wrap(tr.pytest_runtest_setup(_item(nodeid)), setup)
    if call is not None:
        call()
    _wrap(tr.pytest_runtest_teardown(_item(nodeid), None), teardown)


def _warm(tr):
    _test(tr, "t.py::warmup")


# --- the raw snapshots -----------------------------------------------------


def test_live_threads_has_current_thread():
    assert threading.current_thread() in live_threads()


def test_open_fds_identity_or_none():
    v = open_fds()
    assert v is None or all(isinstance(fd, tuple) and len(fd) == 3 for fd in v)


def test_open_fds_sees_a_new_fd():
    before = open_fds()
    if before is None:
        return  # no fd listing on this platform
    r, w = os.pipe()
    try:
        assert len((open_fds() or frozenset()) - before) == 2
    finally:
        os.close(r)
        os.close(w)
    assert not ((open_fds() or frozenset()) - before)


def test_open_fds_none_when_no_fd_dir(monkeypatch):
    def boom(_):
        raise OSError("no such dir")

    monkeypatch.setattr(leakcheck.os, "listdir", boom)
    assert open_fds() is None


def test_open_fds_skips_unstatable_entries(monkeypatch):
    monkeypatch.setattr(leakcheck.os, "listdir", lambda _: ["x", "999999"])
    assert open_fds() == frozenset()


# --- warm-up skip ----------------------------------------------------------


def test_first_test_is_warmup_and_not_recorded(monkeypatch):
    w = World(monkeypatch)
    tr = LeakTracker()
    _test(tr, "t.py::warmup", call=lambda: w.threads.add("leak"))
    assert tr._warmed is True
    assert tr.pop("t.py::warmup") is None


# --- attribution -----------------------------------------------------------


def test_thread_and_fd_leak_charged_to_creator(monkeypatch):
    w = World(monkeypatch)
    tr = LeakTracker()
    _warm(tr)

    def leak():
        w.threads |= {"t1", "t2"}
        w.fds.add((7, 1, 70))

    _test(tr, "t.py::leaker", call=leak)
    assert tr.pop("t.py::leaker") == (2, 1)
    assert tr.pop("t.py::leaker") is None  # consumed


def test_open_and_close_is_clean(monkeypatch):
    w = World(monkeypatch)
    tr = LeakTracker()
    _warm(tr)
    _test(
        tr,
        "t.py::clean",
        call=lambda: w.threads.add("t"),
        teardown=lambda: w.threads.discard("t"),
    )
    assert tr.pop("t.py::clean") == (0, 0)


def test_release_of_older_resource_does_not_cancel_a_leak(monkeypatch):
    # MT-06a: a module fixture's teardown in this test's window joins an older
    # thread; the count is net 0 but this test still leaked its own thread.
    w = World(monkeypatch)
    w.threads.add("module_server")
    tr = LeakTracker()
    _warm(tr)
    _test(
        tr,
        "t.py::last_leaks",
        call=lambda: w.threads.add("permanent"),
        teardown=lambda: w.threads.discard("module_server"),
    )
    assert tr.pop("t.py::last_leaks") == (1, 0)


def test_thread_ending_in_a_later_test_stays_charged_to_its_creator(monkeypatch):
    # MT-06b: test_b's thread ends during test_c, which starts a permanent one.
    w = World(monkeypatch)
    tr = LeakTracker()
    _warm(tr)
    _test(tr, "t.py::b", call=lambda: w.threads.add("short"))

    def c():
        w.threads.discard("short")
        w.threads.add("permanent")

    _test(tr, "t.py::c", call=c)
    assert tr.pop("t.py::b") == (1, 0)
    assert tr.pop("t.py::c") == (1, 0)


def test_reused_fd_number_with_new_file_is_a_new_resource(monkeypatch):
    w = World(monkeypatch)
    w.fds.add((5, 1, 50))
    tr = LeakTracker()
    _warm(tr)

    def swap():
        w.fds.discard((5, 1, 50))
        w.fds.add((5, 1, 51))

    _test(tr, "t.py::swap", call=swap)
    assert tr.pop("t.py::swap") == (0, 1)


def test_fd_delta_none_when_fds_unreadable(monkeypatch):
    w = World(monkeypatch, fds=False)
    tr = LeakTracker()
    _warm(tr)
    _test(tr, "t.py::no_fds", call=lambda: w.threads.add("t"))
    assert tr.pop("t.py::no_fds") == (1, None)


# --- wider-scoped fixtures --------------------------------------------------


def _setup_fixture(tr, w, scope, thread, fd=None):
    def body():
        w.threads.add(thread)
        if fd is not None:
            w.fds.add(fd)

    _wrap(tr.pytest_fixture_setup(_fixture(scope), None), body)


def test_session_fixture_resources_are_not_the_test_leak(monkeypatch):
    # MT-07: the first user of a session server isn't charged with it, and its
    # shutdown in a later test's window changes nothing for that test either.
    w = World(monkeypatch)
    tr = LeakTracker()
    _warm(tr)
    _test(
        tr,
        "t.py::first_user",
        setup=lambda: _setup_fixture(tr, w, "session", "server", (9, 1, 90)),
    )
    assert tr.pop("t.py::first_user") == (0, 0)

    def shutdown():
        w.threads.discard("server")
        w.fds.discard((9, 1, 90))

    _test(tr, "t.py::last_user", teardown=shutdown)
    assert tr.pop("t.py::last_user") == (0, 0)


def test_module_fixture_set_up_and_torn_down_in_one_window(monkeypatch):
    # A process-lifetime resource a module fixture's setup created (libc's
    # cached resolver socket) outlives the fixture: still not the test's.
    w = World(monkeypatch)
    tr = LeakTracker()
    _warm(tr)

    def teardown():
        w.threads.discard("srv")

    _test(
        tr,
        "t.py::only_user",
        setup=lambda: _setup_fixture(tr, w, "module", "srv", (11, 1, 110)),
        teardown=teardown,
    )
    assert tr.pop("t.py::only_user") == (0, 0)


def test_function_fixture_leak_is_the_test_leak(monkeypatch):
    w = World(monkeypatch)
    tr = LeakTracker()
    _warm(tr)
    _test(
        tr,
        "t.py::fn_fixture",
        setup=lambda: _setup_fixture(tr, w, "function", "worker", (12, 1, 120)),
    )
    assert tr.pop("t.py::fn_fixture") == (1, 1)


def test_fixture_setup_outside_a_test_window_is_ignored(monkeypatch):
    w = World(monkeypatch)
    tr = LeakTracker()
    _setup_fixture(tr, w, "session", "early")
    assert tr._current is None


def test_wider_fixture_with_unreadable_fds(monkeypatch):
    w = World(monkeypatch, fds=False)
    tr = LeakTracker()
    _warm(tr)
    _test(tr, "t.py::x", setup=lambda: _setup_fixture(tr, w, "class", "srv"))
    assert tr.pop("t.py::x") == (0, None)


def test_real_threads_end_to_end():
    # The real snapshots, no scripting: a permanent thread is charged.
    tr = LeakTracker()
    _warm(tr)
    stop = threading.Event()
    t = threading.Thread(target=stop.wait, daemon=True)
    try:
        _test(tr, "t.py::real", call=t.start)
        res = tr.pop("t.py::real")
        assert res is not None and res[0] == 1
    finally:
        stop.set()
        t.join()


# --- StreamPlugin wiring -----------------------------------------------------


def _plugin(monkeypatch, *, leakcheck=True):
    if leakcheck:
        monkeypatch.setenv("RSTEST_LEAKCHECK", "1")
    else:
        monkeypatch.delenv("RSTEST_LEAKCHECK", raising=False)
    return StreamPlugin(FakeConn())


def test_teardown_report_carries_deltas(monkeypatch):
    p = _plugin(monkeypatch)
    p._leaks._res["t.py::leaker"] = (3, 2)
    p.pytest_runtest_logreport(_report("t.py::leaker", "teardown"))
    kind, payload = p._conn.sent[-1]
    assert kind == "report"
    assert payload["thread_delta"] == 3
    assert payload["fd_delta"] == 2
    assert p._leaks.pop("t.py::leaker") is None  # consumed


def test_teardown_report_omits_zero_deltas(monkeypatch):
    p = _plugin(monkeypatch)
    p._leaks._res["t.py::clean"] = (0, 0)
    p.pytest_runtest_logreport(_report("t.py::clean", "teardown"))
    _, payload = p._conn.sent[-1]
    assert "thread_delta" not in payload
    assert "fd_delta" not in payload


def test_teardown_report_omits_none_fd_delta(monkeypatch):
    p = _plugin(monkeypatch)
    p._leaks._res["t.py::t"] = (2, None)
    p.pytest_runtest_logreport(_report("t.py::t", "teardown"))
    _, payload = p._conn.sent[-1]
    assert payload["thread_delta"] == 2
    assert "fd_delta" not in payload


def test_non_teardown_report_ignores_deltas(monkeypatch):
    p = _plugin(monkeypatch)
    p._leaks._res["t.py::t"] = (3, 2)
    p.pytest_runtest_logreport(_report("t.py::t", "call"))
    _, payload = p._conn.sent[-1]
    assert "thread_delta" not in payload
    assert "t.py::t" in p._leaks._res  # not consumed on the call report


def test_leakcheck_disabled_has_no_tracker(monkeypatch):
    p = _plugin(monkeypatch, leakcheck=False)
    assert p._leakcheck is False
    assert p._leaks is None
    p.pytest_runtest_logreport(_report("t.py::t", "teardown"))
    _, payload = p._conn.sent[-1]
    assert "thread_delta" not in payload
