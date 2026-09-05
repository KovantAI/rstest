"""Unit coverage for the native --timeout path in StreamPlugin.

The `_parse_timeout` helper plus the three timeout-related methods
(`_effective_timeout`, `_arm_timeout`, and the `pytest_runtest_call` wrapper),
and the worker cache-write guard installed in `pytest_sessionstart`. The worker
subprocess that runs this code under the e2e gate isn't seen by pytest-cov, so
these direct tests are what give the timeout path Python coverage.
"""

import signal
import threading
import time
from types import SimpleNamespace

import pytest
from rstest_worker._internal.stream import StreamPlugin, Timeout, _parse_timeout

HAS_SIGALRM = hasattr(signal, "SIGALRM")


class FakeConn:
    def __init__(self):
        self.sent = []

    def send(self, kind, payload):
        self.sent.append((kind, payload))


def _item(nodeid, marker=None):
    """Duck-typed item. `marker` is what get_closest_marker('timeout') returns."""
    return SimpleNamespace(nodeid=nodeid, get_closest_marker=lambda name: marker)


def _plugin(monkeypatch, *, timeout=None, doctor=False):
    if timeout is None:
        monkeypatch.delenv("RSTEST_TIMEOUT", raising=False)
    else:
        monkeypatch.setenv("RSTEST_TIMEOUT", str(timeout))
    if doctor:
        monkeypatch.setenv("RSTEST_DOCTOR", "1")
    else:
        monkeypatch.delenv("RSTEST_DOCTOR", raising=False)
    return StreamPlugin(FakeConn())


def _drive_call(plugin, item, retval="RESULT"):
    """Drive the pytest_runtest_call wrapper hook to completion (no timer fire)."""
    gen = plugin.pytest_runtest_call(item)
    next(gen)  # arm timer / take t0, run up to the yield
    try:
        gen.send(retval)  # resume; runs the finally (cancel + cpu record)
    except StopIteration as e:
        return e.value
    raise AssertionError("wrapper hook did not stop")


# --- _parse_timeout --------------------------------------------------------


def test_parse_timeout_accepts_positive_floats():
    assert _parse_timeout("1") == 1.0
    assert _parse_timeout("0.3") == 0.3
    assert _parse_timeout("12.5") == 12.5
    assert _parse_timeout(2.5) == 2.5  # already a float


def test_parse_timeout_rejects_non_positive_and_bad_input():
    assert _parse_timeout(None) is None
    assert _parse_timeout("") is None
    assert _parse_timeout("0") is None
    assert _parse_timeout("-2") is None
    assert _parse_timeout("garbage") is None


# --- _effective_timeout: marker overrides the global -----------------------


def test_effective_timeout_falls_back_to_global(monkeypatch):
    p = _plugin(monkeypatch, timeout=2)
    assert p._effective_timeout(_item("t::a", marker=None)) == 2.0


def test_effective_timeout_marker_without_args_uses_global(monkeypatch):
    p = _plugin(monkeypatch, timeout=2)
    marker = SimpleNamespace(args=())  # bare @pytest.mark.timeout
    assert p._effective_timeout(_item("t::a", marker=marker)) == 2.0


def test_effective_timeout_marker_overrides_global(monkeypatch):
    p = _plugin(monkeypatch, timeout=2)
    marker = SimpleNamespace(args=(0.3,))
    assert p._effective_timeout(_item("t::a", marker=marker)) == 0.3


def test_effective_timeout_marker_zero_disables(monkeypatch):
    # @pytest.mark.timeout(0) parses to None -> no per-test deadline.
    p = _plugin(monkeypatch, timeout=2)
    marker = SimpleNamespace(args=(0,))
    assert p._effective_timeout(_item("t::a", marker=marker)) is None


def test_effective_timeout_none_when_nothing_set(monkeypatch):
    p = _plugin(monkeypatch)  # no global
    assert p._effective_timeout(_item("t::a", marker=None)) is None


# --- _arm_timeout ----------------------------------------------------------


@pytest.mark.skipif(not HAS_SIGALRM, reason="SIGALRM unavailable (Windows)")
def test_arm_timeout_fires_and_raises(monkeypatch):
    # Baseline: no SIGALRM handler wired yet in this test's scope.
    cancel = StreamPlugin._arm_timeout(0.05)
    assert cancel is not None
    try:
        with pytest.raises(Timeout) as exc:
            # Signal fires on the main thread during the wait.
            deadline = time.monotonic() + 1.0
            while time.monotonic() < deadline:
                time.sleep(0.01)
        assert "exceeded --timeout (0.05s)" in str(exc.value)
    finally:
        cancel()


@pytest.mark.skipif(not HAS_SIGALRM, reason="SIGALRM unavailable (Windows)")
def test_arm_timeout_cancel_disarms_and_restores_handler():
    sentinel = signal.getsignal(signal.SIGALRM)
    cancel = StreamPlugin._arm_timeout(10)
    assert signal.getitimer(signal.ITIMER_REAL)[0] > 0  # armed
    cancel()
    assert signal.getitimer(signal.ITIMER_REAL) == (0.0, 0.0)  # disarmed
    assert signal.getsignal(signal.SIGALRM) is sentinel  # prior handler restored


@pytest.mark.skipif(not HAS_SIGALRM, reason="SIGALRM unavailable (Windows)")
def test_arm_timeout_none_off_main_thread():
    result = {}

    def worker():
        result["cancel"] = StreamPlugin._arm_timeout(1)

    t = threading.Thread(target=worker)
    t.start()
    t.join()
    assert result["cancel"] is None  # only the main thread can be interrupted


def test_arm_timeout_none_without_sigalrm(monkeypatch):
    # Simulate a platform without SIGALRM (e.g. Windows).
    import rstest_worker._internal.stream as stream_mod

    fake_signal = SimpleNamespace()  # no SIGALRM attribute
    monkeypatch.setitem(__import__("sys").modules, "signal", fake_signal)
    # _arm_timeout imports signal fresh inside the function.
    assert stream_mod.StreamPlugin._arm_timeout(1) is None


# --- pytest_runtest_call wrapper -------------------------------------------


def test_call_passthrough_when_nothing_enabled(monkeypatch):
    # secs is None and not doctor -> straight passthrough, no timer, no cpu.
    p = _plugin(monkeypatch)
    assert _drive_call(p, _item("t::a")) == "RESULT"
    assert p._cpu == {}


@pytest.mark.skipif(not HAS_SIGALRM, reason="SIGALRM unavailable (Windows)")
def test_call_arms_and_cancels_timer_on_success(monkeypatch):
    p = _plugin(monkeypatch, timeout=100)  # large: never fires during the test
    assert _drive_call(p, _item("t::a")) == "RESULT"
    # Timer disarmed by the finally's cancel().
    assert signal.getitimer(signal.ITIMER_REAL) == (0.0, 0.0)
    assert p._cpu == {}  # doctor off -> no cpu recorded


def test_call_records_cpu_under_doctor_without_timeout(monkeypatch):
    p = _plugin(monkeypatch, doctor=True)
    assert _drive_call(p, _item("t::a")) == "RESULT"
    assert "t::a" in p._cpu and p._cpu["t::a"] >= 0.0


@pytest.mark.skipif(not HAS_SIGALRM, reason="SIGALRM unavailable (Windows)")
def test_call_timeout_and_doctor_together(monkeypatch):
    p = _plugin(monkeypatch, timeout=100, doctor=True)
    assert _drive_call(p, _item("t::b")) == "RESULT"
    assert signal.getitimer(signal.ITIMER_REAL) == (0.0, 0.0)
    assert "t::b" in p._cpu  # both layers ran


# --- pytest_sessionstart worker cache-write guard --------------------------


def _session_with_cache():
    writes = []

    def real_set(key, value):
        writes.append((key, value))

    cache = SimpleNamespace(set=real_set)
    config = SimpleNamespace(cache=cache)
    return SimpleNamespace(config=config), writes


def test_sessionstart_guards_worker_cache_writes(monkeypatch):
    monkeypatch.setenv("RSTEST_WORKER_ID", "gw0")
    p = _plugin(monkeypatch)
    session, writes = _session_with_cache()
    p.pytest_sessionstart(session)

    guarded = session.config.cache.set
    # Shared last-failed/nodeids/stepwise writes are dropped (orchestrator owns
    # the merged truth); everything else passes through to the real setter.
    assert guarded("cache/lastfailed", [1]) is None
    assert guarded("cache/nodeids", [2]) is None
    assert guarded("cache/stepwise", [3]) is None
    guarded("some/other", "keep")
    assert writes == [("some/other", "keep")]


def test_sessionstart_no_guard_outside_worker(monkeypatch):
    # No RSTEST_WORKER_ID (e.g. -n 0 / collection): cache.set left untouched.
    monkeypatch.delenv("RSTEST_WORKER_ID", raising=False)
    p = _plugin(monkeypatch)
    session, _ = _session_with_cache()
    original = session.config.cache.set
    p.pytest_sessionstart(session)
    assert session.config.cache.set is original
