"""Unit tests for the resource-leak instrumentation in StreamPlugin.

Covers the per-test thread/fd delta path (`--doctor` / `--fail-on-leak`): the
setup/teardown snapshot hooks, the first-test warm-up skip, and the teardown
report payload that carries the net deltas. The worker subprocess that runs this
code under the e2e gate isn't seen by pytest-cov, so these direct tests are what
give the leak path Python coverage.
"""

from types import SimpleNamespace

from rstest_worker._internal import stream
from rstest_worker._internal.stream import StreamPlugin, _count_fds, _count_threads


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


def _run_wrapper(gen):
    """Drive a wrapper=True hook generator to completion, returning its value."""
    next(gen)  # run up to the yield (baseline / pre-teardown work)
    try:
        gen.send(None)  # resume; runs the finally / post-teardown work
    except StopIteration as e:
        return e.value
    raise AssertionError("wrapper hook did not stop")


def _plugin(monkeypatch, *, leakcheck=True):
    if leakcheck:
        monkeypatch.setenv("RSTEST_LEAKCHECK", "1")
    else:
        monkeypatch.delenv("RSTEST_LEAKCHECK", raising=False)
    return StreamPlugin(FakeConn())


def _script_counts(monkeypatch, threads, fds):
    """Feed scripted return values to _count_threads / _count_fds in call order."""
    t = iter(threads)
    f = iter(fds)
    monkeypatch.setattr(stream, "_count_threads", lambda: next(t))
    monkeypatch.setattr(stream, "_count_fds", lambda: next(f))


def _measure(plugin, nodeid):
    """Run one test's setup+teardown through the leak hooks."""
    _run_wrapper(plugin.pytest_runtest_setup(_item(nodeid)))
    _run_wrapper(plugin.pytest_runtest_teardown(_item(nodeid), None))


# --- the raw counters ------------------------------------------------------


def test_count_threads_is_positive():
    # Always at least the main thread.
    assert _count_threads() >= 1


def test_count_fds_int_or_none():
    v = _count_fds()
    assert v is None or (isinstance(v, int) and v >= 0)


def test_count_fds_none_when_no_fd_dir(monkeypatch):
    # No /proc/self/fd and no /dev/fd (e.g. Windows): fd tracking disabled.
    def boom(_):
        raise OSError("no such dir")

    monkeypatch.setattr(stream.os, "listdir", boom)
    assert _count_fds() is None


# --- warm-up skip ----------------------------------------------------------


def test_first_test_is_warmup_and_not_recorded(monkeypatch):
    p = _plugin(monkeypatch)
    # Only the warm-up's setup reads counts; its teardown skips the delta.
    _script_counts(monkeypatch, threads=[5, 99], fds=[10, 99])
    _measure(p, "t.py::warmup")
    assert p._leak_warmed is True
    assert "t.py::warmup" not in p._res  # first test never attributed a delta


# --- delta measurement -----------------------------------------------------


def test_thread_and_fd_leak_recorded_after_warmup(monkeypatch):
    p = _plugin(monkeypatch)
    p._leak_warmed = True  # pretend the warm-up already ran
    # setup: (5,10); teardown: (8,12) -> delta (3,2).
    _script_counts(monkeypatch, threads=[5, 8], fds=[10, 12])
    _measure(p, "t.py::leaker")
    assert p._res["t.py::leaker"] == (3, 2)


def test_clean_test_records_zero_delta(monkeypatch):
    p = _plugin(monkeypatch)
    p._leak_warmed = True
    _script_counts(monkeypatch, threads=[5, 5], fds=[10, 10])
    _measure(p, "t.py::clean")
    assert p._res["t.py::clean"] == (0, 0)


def test_fd_delta_none_when_fds_unreadable(monkeypatch):
    p = _plugin(monkeypatch)
    p._leak_warmed = True
    # fds unreadable on this platform -> None both snapshots -> fd_delta None.
    monkeypatch.setattr(stream, "_count_fds", lambda: None)
    threads = iter([5, 7])
    monkeypatch.setattr(stream, "_count_threads", lambda: next(threads))
    _measure(p, "t.py::no_fds")
    assert p._res["t.py::no_fds"] == (2, None)


# --- teardown report payload ----------------------------------------------


def test_teardown_report_carries_deltas(monkeypatch):
    p = _plugin(monkeypatch)
    p._res["t.py::leaker"] = (3, 2)
    p.pytest_runtest_logreport(_report("t.py::leaker", "teardown"))
    kind, payload = p._conn.sent[-1]
    assert kind == "report"
    assert payload["thread_delta"] == 3
    assert payload["fd_delta"] == 2
    assert "t.py::leaker" not in p._res  # consumed


def test_teardown_report_omits_zero_deltas(monkeypatch):
    p = _plugin(monkeypatch)
    p._res["t.py::clean"] = (0, 0)
    p.pytest_runtest_logreport(_report("t.py::clean", "teardown"))
    _, payload = p._conn.sent[-1]
    assert "thread_delta" not in payload  # 0 is falsy -> omitted
    assert "fd_delta" not in payload


def test_teardown_report_omits_none_fd_delta(monkeypatch):
    p = _plugin(monkeypatch)
    p._res["t.py::t"] = (2, None)
    p.pytest_runtest_logreport(_report("t.py::t", "teardown"))
    _, payload = p._conn.sent[-1]
    assert payload["thread_delta"] == 2
    assert "fd_delta" not in payload  # None fd delta not shipped


def test_non_teardown_report_ignores_deltas(monkeypatch):
    p = _plugin(monkeypatch)
    p._res["t.py::t"] = (3, 2)
    p.pytest_runtest_logreport(_report("t.py::t", "call"))
    _, payload = p._conn.sent[-1]
    assert "thread_delta" not in payload
    assert "t.py::t" in p._res  # not consumed on the call report


# --- disabled ---------------------------------------------------------------


def test_leakcheck_disabled_is_noop(monkeypatch):
    p = _plugin(monkeypatch, leakcheck=False)
    assert p._leakcheck is False
    # Even with scripted counts, no baseline is taken and nothing is recorded.
    _script_counts(monkeypatch, threads=[5, 8], fds=[10, 12])
    _measure(p, "t.py::x")
    assert p._res == {}
    assert p._res_base == {}
