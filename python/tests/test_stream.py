"""Unit tests for StreamPlugin seams not covered by the leakcheck/timeout
suites: cmdline_main, xdist node-hook plumbing, warning aggregation,
sessionfinish emission, the doctor fixture timer, and report payloads."""

from __future__ import annotations

from types import SimpleNamespace
from typing import Any

import pytest
from rstest_worker._internal import stream
from rstest_worker._internal.stream import StreamPlugin


class FakeConn:
    def __init__(self) -> None:
        self.sent: list[tuple[str, Any]] = []

    def send(self, kind: str, payload: Any) -> None:
        self.sent.append((kind, payload))


def _plugin() -> StreamPlugin:
    return StreamPlugin(FakeConn())


def mk_report(
    when,
    outcome,
    *,
    nodeid="t.py::a",
    failed=False,
    skipped=False,
    longreprtext="",
    longrepr=None,
    sections=None,
    location=("t.py", 1, "a"),
):
    return SimpleNamespace(
        nodeid=nodeid,
        when=when,
        outcome=outcome,
        duration=0.0,
        longreprtext=longreprtext,
        longrepr=longrepr,
        failed=failed,
        skipped=skipped,
        sections=sections or [],
        location=location,
    )


# ── pytest_cmdline_main: rerunfailures neutralization ───────────────────────


def test_cmdline_main_neutralizes_rerunfailures_in_worker(monkeypatch):
    seen: list[Any] = []
    monkeypatch.setattr(stream, "_neutralize_rerunfailures", lambda c: seen.append(c))
    monkeypatch.setenv("RSTEST_WORKER_ID", "gw0")
    config = object()
    assert _plugin().pytest_cmdline_main(config) is None
    assert seen == [config]


def test_cmdline_main_noop_outside_worker(monkeypatch):
    seen: list[Any] = []
    monkeypatch.setattr(stream, "_neutralize_rerunfailures", lambda c: seen.append(c))
    monkeypatch.delenv("RSTEST_WORKER_ID", raising=False)
    _plugin().pytest_cmdline_main(object())
    assert seen == []


# ── _call_configure_node / plugin_registered ───────────────────────────────


def test_call_configure_node_invokes_once_and_marks(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    p._xdist_node = SimpleNamespace()  # truthy shim
    seen: list[Any] = []
    plugin = SimpleNamespace(pytest_configure_node=lambda node: seen.append(node))

    p._call_configure_node(plugin)
    assert seen == [p._xdist_node]
    assert id(plugin) in p._node_configured

    p._call_configure_node(plugin)  # already configured -> skipped
    assert len(seen) == 1


def test_call_configure_node_skips_without_shim():
    p = _plugin()  # _xdist_node is None
    plugin = SimpleNamespace(pytest_configure_node=lambda node: pytest.fail("called"))
    p._call_configure_node(plugin)  # returns early, no call


def test_call_configure_node_skips_plugin_without_hook(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    p._xdist_node = SimpleNamespace()
    p._call_configure_node(SimpleNamespace())  # no pytest_configure_node attr


def test_call_configure_node_lenient_swallows_error(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    p._xdist_node = SimpleNamespace()

    def boom(node):
        raise RuntimeError("state not ready")

    plugin = SimpleNamespace(pytest_configure_node=boom)
    p._call_configure_node(plugin, lenient=True)  # swallowed
    assert id(plugin) not in p._node_configured
    with pytest.raises(RuntimeError):  # strict re-raises
        p._call_configure_node(plugin)


def test_plugin_registered_defers_to_configure_node(monkeypatch):
    p = _plugin()
    seen: list[Any] = []
    monkeypatch.setattr(
        p, "_call_configure_node", lambda pl, lenient=False: seen.append((pl, lenient))
    )
    plugin = object()
    p.pytest_plugin_registered(plugin, manager=None)
    assert seen == [(plugin, True)]


# ── _call_node_hooks / run_foreign_node_down ───────────────────────────────


def test_call_node_hooks_noop_without_shim():
    p = _plugin()  # _xdist_node None
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [1 / 0]))
    p._call_node_hooks(config, "pytest_testnodeready")  # returns before touching plugins


def test_call_node_hooks_invokes_local_hook(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    p._xdist_node = SimpleNamespace(workerid="gw0")
    seen: list[Any] = []
    plugin = SimpleNamespace(pytest_testnodeready=lambda node: seen.append(node))
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    p._call_node_hooks(config, "pytest_testnodeready")
    assert seen == [p._xdist_node]


def test_run_foreign_node_down_calls_testnodedown(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    seen: list[Any] = []
    plugin = SimpleNamespace(
        pytest_testnodedown=lambda node, error=None: seen.append((node.workerinput, error))
    )
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    p.run_foreign_node_down(config, {"workerinput": {"workerid": "gw3"}, "error": "boom"})
    assert seen == [({"workerid": "gw3"}, "boom")]


def test_run_foreign_node_down_swallows_hook_error(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()

    def boom(node, error=None):
        raise RuntimeError("dead sibling hook blew up")

    plugin = SimpleNamespace(pytest_testnodedown=boom)
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    # Must not propagate (cleanup for a dead sibling can't poison this worker).
    p.run_foreign_node_down(config, {"workerinput": {}, "error": None})


def test_run_foreign_node_down_skips_dist_internal(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: True)
    p = _plugin()
    plugin = SimpleNamespace(pytest_testnodedown=lambda node, error=None: pytest.fail("called"))
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    p.run_foreign_node_down(config, {"workerinput": {}})  # skipped, no call


def test_sweep_configure_node_retries_all_plugins(monkeypatch):
    p = _plugin()
    plugin = object()
    p._xdist_node = SimpleNamespace(
        config=SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    )
    seen: list[Any] = []
    monkeypatch.setattr(p, "_call_configure_node", lambda pl: seen.append(pl))
    p._sweep_configure_node()
    assert seen == [plugin]  # strict retry of each plugin


def test_sweep_configure_node_noop_without_shim():
    p = _plugin()  # _xdist_node None
    p._sweep_configure_node()  # returns early, no crash


# ── pytest_sessionstart: node_input snapshot for crash cleanup ─────────────


def test_sessionstart_ships_node_input_snapshot(monkeypatch):
    monkeypatch.delenv("RSTEST_WORKER_ID", raising=False)
    p = _plugin()
    monkeypatch.setattr(p, "_sweep_configure_node", lambda: None)
    monkeypatch.setattr(p, "_call_node_hooks", lambda *a, **k: None)
    p._xdist_node = SimpleNamespace(workerinput={"workerid": "gw0", "workercount": 4})
    session = SimpleNamespace(config=SimpleNamespace(cache=None))
    p.pytest_sessionstart(session)
    node_inputs = [pl for k, pl in p._conn.sent if k == "node_input"]
    assert node_inputs == [{"workerinput": {"workerid": "gw0", "workercount": 4}}]


# ── pytest_warning_recorded: aggregation ───────────────────────────────────


def test_warning_recorded_aggregates_duplicates():
    p = _plugin()
    wm = SimpleNamespace(
        message="deprecated thing",
        category=SimpleNamespace(__name__="DeprecationWarning"),
        filename="f.py",
        lineno=5,
    )
    p.pytest_warning_recorded(wm, "runtest", "t.py::a", ("f.py", 5, ""))
    p.pytest_warning_recorded(wm, "runtest", "t.py::a", ("f.py", 5, ""))
    assert list(p._warnings.values()) == [2]  # same key -> counted twice
    (key,) = p._warnings
    assert key == ("runtest", "DeprecationWarning", "deprecated thing", "f.py", 5)


def test_warning_recorded_uses_message_type_for_nonstring():
    p = _plugin()

    class MyWarning(Warning):
        pass

    wm = SimpleNamespace(
        message=MyWarning("x"),
        category=SimpleNamespace(__name__="ignored"),
        filename="g.py",
        lineno=1,
    )
    p.pytest_warning_recorded(wm, "collect", "t.py", ("g.py", 1, ""))
    (key,) = p._warnings
    assert key[1] == "MyWarning"  # type(message).__name__, not category


# ── pytest_sessionfinish: warnings + doctor fixtures emission ───────────────


def test_sessionfinish_emits_warnings():
    p = _plugin()  # _xdist_node None -> node hooks are a no-op
    p._warnings = {("runtest", "UserWarning", "msg", "f.py", 3): 4}
    p.pytest_sessionfinish(session=SimpleNamespace(config=SimpleNamespace()), exitstatus=0)
    kinds = {k for k, _ in p._conn.sent}
    assert "warnings" in kinds
    entries = next(pl for k, pl in p._conn.sent if k == "warnings")["entries"]
    assert entries == [
        {
            "when": "runtest",
            "category": "UserWarning",
            "message": "msg",
            "filename": "f.py",
            "lineno": 3,
            "count": 4,
        }
    ]


def test_sessionfinish_emits_doctor_fixtures(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()  # _doctor read from env at init
    p._fixtures = {("db", "session"): [3, 1.23456]}
    p.pytest_sessionfinish(session=SimpleNamespace(config=SimpleNamespace()), exitstatus=0)
    fixtures = next(pl for k, pl in p._conn.sent if k == "doctor_fixtures")["fixtures"]
    assert fixtures == [{"name": "db", "scope": "session", "count": 3, "total": 1.2346}]


def test_sessionfinish_quiet_when_nothing_to_report():
    p = _plugin()
    p.pytest_sessionfinish(session=SimpleNamespace(config=SimpleNamespace()), exitstatus=0)
    assert p._conn.sent == []  # no warnings, doctor off -> nothing sent


# ── pytest_fixture_setup: doctor timing wrapper ────────────────────────────


def test_fixture_setup_records_under_doctor(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    fd = SimpleNamespace(argname="db", scope="function")
    gen = p.pytest_fixture_setup(fd, request=None)
    assert next(gen) is None  # wrapper yields to the real setup
    with pytest.raises(StopIteration):
        gen.send("result")
    count, total = p._fixtures[("db", "function")]
    assert count == 1 and total >= 0.0


def test_fixture_setup_passthrough_without_doctor():
    p = _plugin()  # doctor off
    fd = SimpleNamespace(argname="db", scope="function")
    gen = p.pytest_fixture_setup(fd, request=None)
    next(gen)
    with pytest.raises(StopIteration):
        gen.send("result")
    assert p._fixtures == {}  # nothing timed


def test_fixture_setup_ignores_request_fixture(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    fd = SimpleNamespace(argname="request", scope="function")
    gen = p.pytest_fixture_setup(fd, request=None)
    next(gen)
    with pytest.raises(StopIteration):
        gen.send("result")
    assert p._fixtures == {}  # the built-in request fixture is not measured


# ── pytest_runtest_logreport: payload branches ─────────────────────────────


def test_logreport_basic_report_payload():
    p = _plugin()
    p.pytest_runtest_logreport(mk_report("call", "passed"))
    kind, payload = p._conn.sent[0]
    assert kind == "report"
    assert payload["nodeid"] == "t.py::a"
    assert payload["when"] == "call"
    assert payload["outcome"] == "passed"
    assert payload["lineno"] == 1  # from location[1]


def test_logreport_attaches_cpu_on_call(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    p._cpu["t.py::a"] = 0.123456
    p.pytest_runtest_logreport(mk_report("call", "passed"))
    payload = p._conn.sent[0][1]
    assert payload["cpu"] == 0.1235  # rounded to 4 places
    assert "t.py::a" not in p._cpu  # popped


def test_logreport_ships_sections_only_on_failure():
    p = _plugin()
    big = "x" * 30000
    p.pytest_runtest_logreport(
        mk_report("call", "failed", failed=True, sections=[("Captured stdout", big)])
    )
    payload = p._conn.sent[0][1]
    assert payload["sections"] == [["Captured stdout", big[-20000:]]]  # tail-truncated


def test_logreport_extracts_skip_reason():
    p = _plugin()
    p.pytest_runtest_logreport(
        mk_report("setup", "skipped", skipped=True, longrepr=("f.py", 3, "needs network"))
    )
    payload = p._conn.sent[0][1]
    assert payload["skip_reason"] == "needs network"


def test_logreport_omits_lineno_when_location_lineno_none():
    p = _plugin()
    p.pytest_runtest_logreport(mk_report("call", "passed", location=("t.py", None, "a")))
    assert "lineno" not in p._conn.sent[0][1]
